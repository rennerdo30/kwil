//! Source-only regression inventory. ROOT wires `#[cfg(test)] mod wrap_tests;`
//! and runs focused selectors/negative controls after the source freeze.
use crate::{visual_rows::*, wrap_session::*};
use std::{
    io,
    ops::ControlFlow,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

trait OutcomeTestExt<B, C> {
    fn unwrap(self) -> C;
    fn unwrap_err(self) -> B;
}
impl<B: std::fmt::Debug, C> OutcomeTestExt<B, C> for ControlFlow<B, C> {
    fn unwrap(self) -> C {
        match self {
            ControlFlow::Continue(value) => value,
            ControlFlow::Break(owner) => panic!("unexpected refusal: {owner:?}"),
        }
    }
    fn unwrap_err(self) -> B {
        match self {
            ControlFlow::Break(owner) => owner,
            ControlFlow::Continue(_) => panic!("expected refused owner"),
        }
    }
}

fn key(columns: u16, rows: u16) -> LayoutKey {
    LayoutKey {
        source: SourceIdentity { document_id: 7, source_epoch: 11, incarnation: 13, revision: 17 },
        input_epoch: 19,
        layout_generation: 23,
        body_generation: 29,
        body: BodyOwner::Text,
        geometry: LayoutGeometry {
            columns,
            rows,
            tab_size: 4,
            font_generation: 31,
            width_policy_generation: 37,
            settings_generation: 41,
        },
    }
}
fn fence(key: LayoutKey, intent: u64) -> NavigationFence {
    NavigationFence { key, cursor_set_generation: 1, intent_generation: intent }
}

struct Bytes {
    identity: SourceIdentity,
    bytes: Vec<u8>,
    starts: Vec<u64>,
    chunk: usize,
    reads: AtomicUsize,
    largest: AtomicUsize,
}
impl Bytes {
    fn new(key: LayoutKey, bytes: &[u8], chunk: usize) -> Self {
        let mut starts = vec![0];
        starts.extend(bytes.iter().enumerate().filter_map(|(at, b)| (*b == b'\n').then_some(at as u64 + 1)));
        Self {
            identity: key.source,
            bytes: bytes.to_vec(),
            starts,
            chunk,
            reads: AtomicUsize::new(0),
            largest: AtomicUsize::new(0),
        }
    }
}
impl WrapSource for Bytes {
    fn run_layout(
        self: Arc<Self>,
        engine: crate::visual_rows::LayoutEngine,
        request: crate::visual_rows::task::TaskRequest,
        driver: &mut dyn crate::visual_rows::TaskDriver,
    ) -> crate::visual_rows::WorkerExit {
        crate::visual_rows::fixtures::run(self, engine, request, driver)
    }
    fn identity(&self) -> SourceIdentity {
        self.identity
    }
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }
    fn line_count(&self) -> u64 {
        self.starts.len() as u64
    }
    fn line_start(&self, line: u64) -> io::Result<Option<u64>> {
        Ok(self.starts.get(line as usize).copied())
    }
    fn line_of(&self, byte: u64) -> io::Result<u64> {
        if byte > self.len() {
            return Err(io::Error::other("test offset outside source"));
        }
        Ok(self.starts.partition_point(|start| *start <= byte).saturating_sub(1) as u64)
    }
    fn read(&self, at: u64, output: &mut [u8]) -> io::Result<usize> {
        self.reads.fetch_add(1, Ordering::AcqRel);
        self.largest.fetch_max(output.len(), Ordering::AcqRel);
        let count = output.len().min(self.chunk).min(self.bytes.len().saturating_sub(at as usize));
        output[..count].copy_from_slice(&self.bytes[at as usize..at as usize + count]);
        Ok(count)
    }
}
fn ranges(page: &LayoutPage) -> Vec<std::ops::Range<u64>> {
    page.rows().iter().map(|r| r.source_coverage.clone()).collect()
}

#[test]
fn wrap_exact_rows_greedy_whitespace_and_logical_gutter() {
    let key = key(4, 3);
    let source = Bytes::new(key, b"one two\nZ", 5);
    let mut engine = LayoutEngine::new(AllocationBudget::default()).unwrap();
    let page = engine.fill(key, &source, TopAnchor::default(), &NoCancellation).unwrap();
    assert_eq!(ranges(&page), vec![0..4, 4..7, 8..9]);
    assert_eq!(page.rows().iter().map(|r| r.logical_line).collect::<Vec<_>>(), vec![0, 0, 1]);
    assert!(!page.rows()[0].continuation);
    assert!(page.rows()[1].continuation);
    assert_eq!(page.atoms(0).iter().map(PaintAtom::text).collect::<String>(), "one ");
    assert!(page.at_end);
    assert!(!page.scroll_extent(source.len()).exact_visual_total);
    assert_eq!(source.bytes, b"one two\nZ");
}

#[test]
fn wrap_forced_scalar_chunks_tabs_cjk_combining_zwj_crlf_and_final_empty_row() {
    for chunk in [5, 7] {
        let key = key(6, 6);
        let text = "A界e\u{301}👩‍💻\tZ\r\n\n";
        let source = Bytes::new(key, text.as_bytes(), chunk);
        let mut engine = LayoutEngine::with_chunk_limit(AllocationBudget::default(), chunk).unwrap();
        let page = engine.fill(key, &source, TopAnchor::default(), &NoCancellation).unwrap();
        assert_eq!(ranges(&page), vec![0..18, 18..20, 22..22, 23..23]);
        assert_eq!(page.atoms(0).iter().map(|a| a.source.clone()).collect::<Vec<_>>(), vec![0..1, 1..4, 4..7, 7..18]);
        assert_eq!(page.atoms(1)[0].cells, 0..4, "tab origin resets per continuation");
        assert!(page.atoms(1)[0].is_tab());
        assert!(source.largest.load(Ordering::Acquire) <= chunk);
        assert_eq!(
            page.source_to_screen(5, CaretAffinity::Downstream).unwrap().cell,
            3,
            "known scalar cursor paints at cluster start without changing source"
        );
        assert_eq!(page.source_to_screen(10, CaretAffinity::Downstream), None, "no interior ZWJ caret");
    }
}

#[test]
fn wrap_width_one_oversized_invalid_atom_advances_and_has_exact_endpoints() {
    let key = key(1, 3);
    let source = Bytes::new(key, b"\xffX", 1);
    let mut engine = LayoutEngine::with_chunk_limit(AllocationBudget::default(), 1).unwrap();
    let page = engine.fill(key, &source, TopAnchor::default(), &NoCancellation).unwrap();
    assert_eq!(ranges(&page), vec![0..1, 1..2]);
    assert_eq!(page.atoms(0)[0].kind, AtomKind::InvalidByte);
    assert_eq!(page.atoms(0)[0].text(), "\\xFF");
    assert_eq!(page.atoms(0)[0].cells, 0..1);
    assert_eq!(page.hit_test(0, 0).unwrap().byte, 0);
    assert_eq!(page.hit_test(0, 1).unwrap().byte, 1);
}

#[test]
fn wrap_known_normal_scalar_cursor_maps_without_inventing_hit_or_navigation_offsets() {
    let key = key(4, 2);
    let source = Bytes::new(key, "e\u{301}\nX".as_bytes(), 5);
    let budget = AllocationBudget::default();
    let mut engine = LayoutEngine::with_chunk_limit(budget.clone(), 5).unwrap();
    let page = engine.fill(key, &source, TopAnchor::default(), &NoCancellation).unwrap();
    assert_eq!(page.atoms(0)[0].source, 0..3);
    assert_eq!(page.source_to_screen(1, CaretAffinity::Downstream).unwrap().cell, 0);
    assert_eq!(page.source_to_screen(2, CaretAffinity::Downstream), None);
    assert_eq!(page.hit_test(0, 0).unwrap().byte, 0);
    assert_eq!(page.hit_test(0, 1).unwrap().byte, 3);
    let resolved = engine
        .navigate(
            fence(key, 1),
            &source,
            cursors(&budget, &[1]),
            NavigationCommand { motion: VisualMotion::Down, count: 1, extend: false },
            &NoCancellation,
        )
        .unwrap();
    assert_eq!(resolved.expected.cursors()[0].head, 1);
    assert_eq!(resolved.targets.cursors()[0].head, 4);
    assert_eq!(resolved.targets.cursors()[0].desired_cell, Some(0));
    let invalid = engine.navigate(
        fence(key, 2),
        &source,
        cursors(&budget, &[2]),
        NavigationCommand { motion: VisualMotion::Down, count: 1, extend: false },
        &NoCancellation,
    );
    assert!(matches!(invalid, Err(WrapError::Invalid(_))));
}

#[test]
fn wrap_huge_grapheme_is_opaque_without_syntax_extent_expansion() {
    let key = key(1, 3);
    let text = format!("A{}Z", "\u{301}".repeat(160));
    let source = Bytes::new(key, text.as_bytes(), 7);
    let mut engine = LayoutEngine::with_chunk_limit(AllocationBudget::default(), 7).unwrap();
    let page = engine.fill(key, &source, TopAnchor::default(), &NoCancellation).unwrap();
    assert_eq!(ranges(&page), vec![0..321, 321..322]);
    let atom = &page.atoms(0)[0];
    assert_eq!(atom.kind, AtomKind::Opaque);
    assert_eq!(atom.text(), "?");
    assert_eq!(page.syntax_windows().len(), 1);
    assert_eq!(page.syntax_windows()[0], 321..322);
    assert_eq!(page.source_to_screen(111, CaretAffinity::Downstream), None);
    assert_eq!(page.hit_test(0, 1).unwrap().byte, 321);
    let selected = page.selection_cells(0, 100..101).collect::<Vec<_>>();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0], 0..1);
    assert!(page.retained_bytes() < MAX_PAGE_BYTES);
}

#[test]
fn wrap_syntax_consumer_refuses_default_margin_before_any_source_read() {
    let key = key(4, 2);
    let source = Bytes::new(key, b"abcdef", 5);
    let mut engine = LayoutEngine::new(AllocationBudget::default()).unwrap();
    let page = engine.fill(key, &source, TopAnchor::default(), &NoCancellation).unwrap();
    let reads = source.reads.load(Ordering::Acquire);
    let config = kwil_core_syntax::Config::default();
    assert_eq!(config.margin_bytes, 4096);
    for window in page.syntax_windows() {
        assert!(page.check_syntax_request(window.clone(), config.margin_bytes).is_err());
        assert!(page.check_syntax_request(window.clone(), 0).is_ok());
        let widened = window.start..window.end + 1;
        assert!(page.check_syntax_request(widened, 0).is_err());
    }
    assert_eq!(source.reads.load(Ordering::Acquire), reads, "refusal/config boundary never widens reads");
}

#[test]
fn wrap_syntax_window_cap_does_not_omit_text_rows_or_expand_a_coverage_gap() {
    let key = key(1, 130);
    let bytes = "X\n".repeat(129);
    let source = Bytes::new(key, bytes.as_bytes(), 7);
    let mut engine = LayoutEngine::with_chunk_limit(AllocationBudget::default(), 7).unwrap();
    let page = engine.fill(key, &source, TopAnchor::default(), &NoCancellation).unwrap();
    assert_eq!(page.rows().len(), 130);
    assert!(page.at_end);
    assert_eq!(page.syntax_windows().len(), MAX_SYNTAX_WINDOWS);
    assert_eq!(page.syntax_windows().iter().map(|range| range.end - range.start).sum::<u64>(), 128);
    assert!(page.check_syntax_request(0..3, 0).is_err(), "logical coverage/newline gaps are not read admission");
}

#[test]
fn wrap_boundary_affinity_selection_resize_and_scroll_within_one_logical_line() {
    let key = key(4, 3);
    let source = Bytes::new(key, b"abcdefghij", 5);
    let mut engine = LayoutEngine::new(AllocationBudget::default()).unwrap();
    let page = engine.fill(key, &source, TopAnchor::default(), &NoCancellation).unwrap();
    assert_eq!(ranges(&page), vec![0..4, 4..8, 8..10]);
    assert_eq!(page.hit_test(1, 1).unwrap().byte, 5);
    assert_eq!(page.source_to_screen(4, CaretAffinity::Upstream).unwrap().row, 0);
    assert_eq!(page.source_to_screen(4, CaretAffinity::Downstream).unwrap().row, 1);
    for row in 0..3 {
        assert!(!page.selection_cells(row, 2..9).collect::<Vec<_>>().is_empty());
    }
    let scrolled = engine.scroll(key, &source, page.anchor, 1, &NoCancellation).unwrap();
    assert_eq!(scrolled.anchor.source_byte, 4);
    assert_eq!(scrolled.rows()[0].logical_line, 0);
    let delta = scrolled.delta_from(Some(&page));
    assert!(!delta.full);
    assert_eq!(delta.scroll, 1);
    assert_eq!(delta.changed_rows().collect::<Vec<_>>(), vec![2]);
    let mut resized_key = key;
    resized_key.geometry.columns = 6;
    resized_key.layout_generation += 1;
    let resized = engine.fill(resized_key, &source, scrolled.anchor, &NoCancellation).unwrap();
    assert_eq!(resized.rows()[0].source_coverage, 0..6);
    assert!(resized.source_to_screen(4, CaretAffinity::Downstream).is_some());
    let back = engine.fill(key, &source, scrolled.anchor, &NoCancellation).unwrap();
    assert_eq!(back.rows()[0].source_coverage, 4..8);
}

struct VirtualLine {
    identity: SourceIdentity,
    length: u64,
    reads: AtomicUsize,
    largest: AtomicUsize,
}
impl WrapSource for VirtualLine {
    fn run_layout(
        self: Arc<Self>,
        engine: crate::visual_rows::LayoutEngine,
        request: crate::visual_rows::task::TaskRequest,
        driver: &mut dyn crate::visual_rows::TaskDriver,
    ) -> crate::visual_rows::WorkerExit {
        crate::visual_rows::fixtures::run(self, engine, request, driver)
    }
    fn identity(&self) -> SourceIdentity {
        self.identity
    }
    fn len(&self) -> u64 {
        self.length
    }
    fn line_count(&self) -> u64 {
        1
    }
    fn line_start(&self, line: u64) -> io::Result<Option<u64>> {
        Ok((line == 0).then_some(0))
    }
    fn line_of(&self, _: u64) -> io::Result<u64> {
        Ok(0)
    }
    fn read(&self, at: u64, output: &mut [u8]) -> io::Result<usize> {
        self.reads.fetch_add(1, Ordering::AcqRel);
        self.largest.fetch_max(output.len(), Ordering::AcqRel);
        let count = output.len().min(self.length.saturating_sub(at).min(usize::MAX as u64) as usize);
        output[..count].fill(b'x');
        Ok(count)
    }
}
struct CancelAtReads<'a> {
    reads: &'a AtomicUsize,
    limit: usize,
}
impl LayoutControl for CancelAtReads<'_> {
    fn cancelled(&self) -> bool {
        self.reads.load(Ordering::Acquire) >= self.limit
    }
    fn progress(&self, _: u64, _: u64) {}
}
#[test]
fn wrap_virtual_giant_line_bounded_reads_storage_checkpoint_cache_and_cancellation() {
    let key = key(4, 3);
    let source =
        VirtualLine { identity: key.source, length: 1 << 32, reads: AtomicUsize::new(0), largest: AtomicUsize::new(0) };
    let budget = AllocationBudget::default();
    let mut engine = LayoutEngine::with_chunk_limit(budget.clone(), 7).unwrap();
    let page = engine.fill(key, &source, TopAnchor::default(), &NoCancellation).unwrap();
    assert_eq!(ranges(&page), vec![0..4, 4..8, 8..12]);
    assert!(page.retained_bytes() < 32 * 1024);
    assert!(budget.peak_bytes() <= MAX_SESSION_BYTES);
    assert!(engine.checkpoint_count() <= MAX_CHECKPOINTS);
    assert!(engine.checkpoint_bytes() <= MAX_CHECKPOINT_BYTES);
    let control = CancelAtReads { reads: &source.reads, limit: source.reads.load(Ordering::Acquire) + 4 };
    let result =
        engine.fill(key, &source, TopAnchor { source_byte: source.length - 5, ..TopAnchor::default() }, &control);
    assert!(matches!(result, Err(WrapError::Cancelled)));
    assert!(source.largest.load(Ordering::Acquire) <= 7);
    assert!(budget.retained_bytes() <= MAX_SESSION_BYTES);
}

#[test]
fn wrap_zero_geometry_and_admission_do_not_read_or_omit_visible_rows() {
    let key = key(0, 4);
    let source = Bytes::new(key, b"text", 5);
    let mut engine = LayoutEngine::new(AllocationBudget::default()).unwrap();
    let page = engine.fill(key, &source, TopAnchor::default(), &NoCancellation).unwrap();
    assert!(page.rows().is_empty());
    assert_eq!(source.reads.load(Ordering::Acquire), 0);
    // The engine no longer reserves a nonexistent synchronous Reader. Preserve
    // the 1024-byte refusal control at actual cooperative scratch admission.
    assert!(
        LayoutEngine::new(AllocationBudget::new(1024))
            .and_then(|mut engine| engine.fill(key, &source, TopAnchor::default(), &NoCancellation))
            .is_err()
    );
    let invalid = LayoutGeometry { columns: 4097, ..key.geometry };
    assert!(invalid.validate().is_err());
}

#[test]
fn wrap_checkpoint_eviction_preserves_exact_rows_and_width_change_invalidates() {
    let key = key(2, 2);
    let source = Bytes::new(key, b"abcdefghijklmnop", 5);
    let budget = AllocationBudget::default();
    let mut engine = LayoutEngine::with_limits(budget, 5, 3).unwrap();
    let page =
        engine.fill(key, &source, TopAnchor { source_byte: 12, ..TopAnchor::default() }, &NoCancellation).unwrap();
    assert_eq!(ranges(&page), vec![12..14, 14..16]);
    assert_eq!(engine.checkpoint_count(), 3);
    let back = engine.scroll(key, &source, page.anchor, -1, &NoCancellation).unwrap();
    assert_eq!(back.rows()[0].source_coverage, 10..12);
    assert!(engine.checkpoint_count() <= 3);
    let mut resized = key;
    resized.geometry.columns = 4;
    resized.layout_generation += 1;
    let changed = engine.fill(resized, &source, TopAnchor::default(), &NoCancellation).unwrap();
    assert_eq!(ranges(&changed), vec![0..4, 4..8]);
    assert_eq!(engine.checkpoint_count(), 2);
}

fn cursors(budget: &AllocationBudget, positions: &[u64]) -> CursorSet {
    let cursors: Vec<_> =
        positions.iter().map(|&head| VisualCursor { anchor: head, head, ..VisualCursor::default() }).collect();
    CursorSet::new(budget, &cursors, 0).unwrap()
}
#[test]
fn wrap_navigation_targets_outside_page_and_visual_goal_columns() {
    let key = key(2, 1);
    let source = Bytes::new(key, b"abcdef\n012345\nXYZ", 5);
    let budget = AllocationBudget::default();
    let mut engine = LayoutEngine::new(budget.clone()).unwrap();
    let navigation = engine
        .navigate(
            fence(key, 1),
            &source,
            cursors(&budget, &[1, 15]),
            NavigationCommand { motion: VisualMotion::Down, count: 1, extend: true },
            &NoCancellation,
        )
        .unwrap();
    assert_eq!(navigation.targets.cursors().iter().map(|c| c.head).collect::<Vec<_>>(), vec![3, 17]);
    assert_eq!(navigation.targets.cursors()[0].anchor, 1);
    assert_eq!(navigation.targets.cursors()[0].desired_cell, Some(1));
    assert_eq!(navigation.reveal.rows().len(), 1);
    assert!(
        navigation.reveal.source_to_screen(17, CaretAffinity::Downstream).is_none(),
        "offscreen targets do not require one page per cursor"
    );
}

#[test]
fn wrap_relative_fifo_preserves_down3_clamps_mixed_page_extend_and_capacity() {
    let key = key(2, 2);
    let source = Bytes::new(key, b"abcdefghij", 5);
    let budget = AllocationBudget::default();
    let mut engine = LayoutEngine::new(budget.clone()).unwrap();
    let mut queue = RelativeQueue::default();
    for _ in 0..3 {
        queue
            .enqueue(RelativeInput {
                key,
                kind: RelativeKind::Move { motion: VisualMotion::Down, count: 1, extend: false },
            })
            .unwrap();
    }
    assert_eq!(queue.mutation_gate(), MutationGate::RefusedPending);
    let mut actual = cursors(&budget, &[0]);
    for intent in 1..=3 {
        let record = queue.begin(key).unwrap().unwrap();
        let resolved = engine
            .navigate(
                fence(key, intent),
                &source,
                actual,
                NavigationCommand { motion: VisualMotion::Down, count: 1, extend: false },
                &NoCancellation,
            )
            .unwrap();
        actual = resolved.targets;
        queue.published(record.id).unwrap();
    }
    assert_eq!(actual.cursors()[0].head, 6);
    assert_eq!(queue.mutation_gate(), MutationGate::Allowed);
    let down = engine
        .navigate(
            fence(key, 4),
            &source,
            cursors(&budget, &[10]),
            NavigationCommand { motion: VisualMotion::Down, count: 1, extend: false },
            &NoCancellation,
        )
        .unwrap();
    let up = engine
        .navigate(
            fence(key, 5),
            &source,
            down.targets,
            NavigationCommand { motion: VisualMotion::Up, count: 1, extend: false },
            &NoCancellation,
        )
        .unwrap();
    assert_eq!(up.targets.cursors()[0].head, 8, "Down then Up at EOF is not net-zero");
    let mixed = [
        RelativeKind::Move { motion: VisualMotion::PageDown, count: 1, extend: true },
        RelativeKind::Scroll(-1),
        RelativeKind::Move { motion: VisualMotion::Up, count: 2, extend: false },
    ];
    for kind in mixed {
        queue.enqueue(RelativeInput { key, kind }).unwrap();
    }
    for kind in mixed {
        let head = queue.begin(key).unwrap().unwrap();
        assert_eq!(head.input.kind, kind);
        queue.published(head.id).unwrap();
    }
    for _ in 0..RELATIVE_CAPACITY {
        queue.enqueue(RelativeInput { key, kind: RelativeKind::Scroll(1) }).unwrap();
    }
    let first = queue.head();
    assert_eq!(queue.enqueue(RelativeInput { key, kind: RelativeKind::Scroll(-1) }), Err(QueueError::Full));
    assert_eq!(queue.len(), RELATIVE_CAPACITY);
    assert_eq!(queue.head(), first);
    let canceled = queue.cancel(CancelReason::Escape);
    assert_eq!(canceled.records, RELATIVE_CAPACITY);
    assert_eq!(queue.mutation_gate(), MutationGate::Allowed);
}

struct Gate {
    entered: mpsc::SyncSender<()>,
    release: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn wait(&self) -> io::Result<()> {
        let release = self.release.lock().unwrap();
        if *release {
            return Ok(());
        }
        let _ = self.entered.try_send(());
        let (release, _) =
            self.changed.wait_timeout_while(release, Duration::from_secs(2), |release| !*release).unwrap();
        if *release { Ok(()) } else { Err(io::Error::other("controlled test gate timed out")) }
    }
    fn open(&self) {
        *self.release.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct GatedSource {
    bytes: Bytes,
    gate: Arc<Gate>,
}
impl WrapSource for GatedSource {
    fn run_layout(
        self: Arc<Self>,
        engine: crate::visual_rows::LayoutEngine,
        request: crate::visual_rows::task::TaskRequest,
        driver: &mut dyn crate::visual_rows::TaskDriver,
    ) -> crate::visual_rows::WorkerExit {
        crate::visual_rows::fixtures::run(self, engine, request, driver)
    }
    fn identity(&self) -> SourceIdentity {
        self.bytes.identity()
    }
    fn len(&self) -> u64 {
        self.bytes.len()
    }
    fn line_count(&self) -> u64 {
        self.bytes.line_count()
    }
    fn line_start(&self, line: u64) -> io::Result<Option<u64>> {
        self.bytes.line_start(line)
    }
    fn line_of(&self, byte: u64) -> io::Result<u64> {
        self.bytes.line_of(byte)
    }
    fn read(&self, at: u64, output: &mut [u8]) -> io::Result<usize> {
        self.gate.wait()?;
        self.bytes.read(at, output)
    }
}
fn request(key: LayoutKey, intent: u64, source: Arc<dyn WrapSource>) -> WrapRequest {
    WrapRequest { fence: fence(key, intent), source, anchor: TopAnchor::default(), intent: LayoutIntent::Fill }
}

#[test]
fn wrap_mailbox_refusal_returns_exact_source_and_retirement_is_bounded() {
    let key = key(4, 2);
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let gate = Arc::new(Gate { entered: entered_tx, release: Mutex::new(false), changed: Condvar::new() });
    let source: Arc<dyn WrapSource> =
        Arc::new(GatedSource { bytes: Bytes::new(key, b"abcdef", 5), gate: gate.clone() });
    let session = WrapSession::new(|| {}).unwrap();
    session.submit(request(key, 1, source.clone())).unwrap();
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    // Running source + one desired + two retired desired owners. The fourth
    // replacement must refuse intact while the one worker is held at its read.
    for intent in 2..=4 {
        session.submit(request(key, intent, source.clone())).unwrap();
    }
    let refused = session.submit(request(key, 5, source.clone())).unwrap_err();
    assert!(matches!(refused.error, WrapError::Busy));
    assert!(Arc::ptr_eq(&refused.request.source, &source));
    assert_eq!(refused.request.fence.intent_generation, 5);
    drop(refused);
    gate.open();
    let ticket = session.stop();
    settle(ticket);
}

fn settle(ticket: SettlementTicket) {
    crate::wrap_session::settle_final(ticket, Duration::from_secs(2)).unwrap();
}

#[test]
fn wrap_shutdown_timeout_retains_exact_owner_and_retry_acknowledges_cleanup() {
    let key = key(4, 1);
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let gate = Arc::new(Gate { entered: entered_tx, release: Mutex::new(false), changed: Condvar::new() });
    let source: Arc<dyn WrapSource> = Arc::new(GatedSource { bytes: Bytes::new(key, b"owned", 5), gate: gate.clone() });
    let weak = Arc::downgrade(&source);
    let session = WrapSession::new(|| {}).unwrap();
    session.submit(request(key, 1, source.clone())).unwrap();
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    drop(source);
    let failure = session.stop().settle(Duration::ZERO).unwrap_err();
    assert!(matches!(failure.error, WrapError::TimedOut));
    assert!(weak.upgrade().is_some());
    assert!(failure.ticket.retained_bytes() <= MAX_SESSION_BYTES);
    gate.open();
    settle(failure.ticket);
    assert!(weak.upgrade().is_none());
}

#[test]
fn wrap_full_layout_fences_reject_raw_loan_projection_and_stale_source() {
    let base = key(4, 1);
    let source = Bytes::new(base, b"source", 5);
    let mut engine = LayoutEngine::new(AllocationBudget::default()).unwrap();
    for body in [BodyOwner::Raw, BodyOwner::MarkdownPreview, BodyOwner::LogProjection, BodyOwner::EditorLoan] {
        let changed = LayoutKey { body, ..base };
        assert!(matches!(engine.fill(changed, &source, TopAnchor::default(), &NoCancellation), Err(WrapError::Stale)));
    }
    for identity in [
        SourceIdentity { revision: base.source.revision + 1, ..base.source },
        SourceIdentity { document_id: 100, ..base.source },
        SourceIdentity { incarnation: 100, ..base.source },
    ] {
        assert!(matches!(
            engine.fill(LayoutKey { source: identity, ..base }, &source, TopAnchor::default(), &NoCancellation),
            Err(WrapError::Stale)
        ));
    }
    assert_eq!(source.reads.load(Ordering::Acquire), 0);
    let mut queue = RelativeQueue::default();
    queue.enqueue(RelativeInput { key: base, kind: RelativeKind::Scroll(1) }).unwrap();
    let mut resized = base;
    resized.geometry.rows += 1;
    assert_eq!(queue.begin(resized), Err(QueueError::StaleGeometry));
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.cancel(CancelReason::GeometryChanged).records, 1);
}

#[derive(Default)]
struct WakeLatch {
    sequence: Mutex<u64>,
    changed: Condvar,
}
impl WakeLatch {
    fn wake(&self) {
        *self.sequence.lock().unwrap() += 1;
        self.changed.notify_all();
    }
    fn next(&self, seen: &mut u64) {
        let sequence = self.sequence.lock().unwrap();
        let (sequence, _) =
            self.changed.wait_timeout_while(sequence, Duration::from_secs(2), |sequence| *sequence == *seen).unwrap();
        assert_ne!(*sequence, *seen, "bounded worker wake was not delivered");
        *seen = *sequence;
    }
}
fn update(session: &mut WrapSession, fence: NavigationFence, wakes: &WakeLatch, seen: &mut u64) -> LayoutUpdate {
    // Event-driven, finitely bounded mailbox consumption. Retire/progress wakes
    // may precede the ready wake; no timer, sleep or busy-poll loop.
    for _ in 0..8 {
        if let Some(update) = session.take_update(fence) {
            return update;
        }
        wakes.next(seen);
    }
    panic!("worker never produced the matching update");
}

#[test]
fn wrap_relative_head_cannot_ack_early_or_stale_and_desired_lane_does_not_cancel_it() {
    let key = key(2, 2);
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let gate = Arc::new(Gate { entered: entered_tx, release: Mutex::new(false), changed: Condvar::new() });
    let source: Arc<dyn WrapSource> =
        Arc::new(GatedSource { bytes: Bytes::new(key, b"abcdefgh", 5), gate: gate.clone() });
    let wakes = Arc::new(WakeLatch::default());
    let notify = wakes.clone();
    let mut session = WrapSession::new(move || notify.wake()).unwrap();
    let mut seen = 0;
    let id = session.relative.enqueue(RelativeInput { key, kind: RelativeKind::Scroll(1) }).unwrap();
    assert_eq!(session.relative.begin(key).unwrap().unwrap().id, id);
    let request_fence = fence(key, 1);
    session
        .submit(WrapRequest {
            fence: request_fence,
            source: source.clone(),
            anchor: TopAnchor::default(),
            intent: LayoutIntent::ScrollRows(1),
        })
        .unwrap();
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(session.published_relative(id, request_fence), Err(QueueError::WrongHead));
    assert_eq!(session.relative.mutation_gate(), MutationGate::RefusedPending);
    // Two desired requests must not invalidate the relative generation.
    session.submit(request(key, 2, source.clone())).unwrap();
    session.submit(request(key, 3, source)).unwrap();
    gate.open();
    let relative = update(&mut session, request_fence, &wakes, &mut seen);
    assert_eq!(relative.relative_id, Some(id));
    match &relative.result {
        Ok(LayoutPayload::Page(page)) => assert_eq!(page.anchor.source_byte, 2),
        _ => panic!("relative page missing"),
    }
    assert_eq!(session.published_relative(id + 1, request_fence), Err(QueueError::WrongHead));
    assert_eq!(session.published_relative(id, fence(key, 99)), Err(QueueError::WrongHead));
    assert_eq!(session.relative.len(), 1);
    // Test stands in for successful page publication; adapters must perform it
    // before this explicit acknowledgement.
    session.published_relative(id, request_fence).unwrap();
    session.retire_owner(RetiredOwner::Update(relative)).unwrap();
    let desired = update(&mut session, fence(key, 3), &wakes, &mut seen);
    assert_eq!(desired.relative_id, None);
    assert_eq!(session.relative.mutation_gate(), MutationGate::Allowed);
    session.retire_owner(RetiredOwner::Update(desired)).unwrap();
    settle(session.stop());
}

#[test]
fn wrap_stale_ready_geometry_is_retired_without_installing_or_touching_a_cursor() {
    let key = key(4, 1);
    let source: Arc<dyn WrapSource> = Arc::new(Bytes::new(key, b"abcdef", 5));
    let wakes = Arc::new(WakeLatch::default());
    let notify = wakes.clone();
    let mut seen = 0;
    let mut session = WrapSession::new(move || notify.wake()).unwrap();
    session.submit(request(key, 1, source.clone())).unwrap();
    wakes.next(&mut seen);
    let mut changed = key;
    changed.geometry.columns = 3;
    changed.layout_generation += 1;
    assert!(session.take_update(fence(changed, 1)).is_none());
    assert!(session.relative.is_empty());
    settle(session.stop());
}

#[test]
fn wrap_retirement_reservation_close_refusal_and_shutdown_retry_preserve_permits() {
    let session = WrapSession::new(|| {}).unwrap();
    let first = session.reserve_retirement().unwrap();
    let second = session.reserve_retirement().unwrap();
    assert!(matches!(session.reserve_retirement(), Err(WrapError::Busy)));
    let failure = session.stop().settle(Duration::ZERO).unwrap_err();
    assert!(matches!(failure.error, WrapError::TimedOut));
    let mut called = false;
    let first = match first.publish(|| {
        called = true;
        ((), None)
    }) {
        Err(first) => first,
        Ok(()) => panic!("closed permit must refuse"),
    };
    assert!(!called);
    assert!(!first.is_current());
    drop(first);
    drop(second);
    settle(failure.ticket);
}

#[test]
fn wrap_empty_selection_never_paints_normal_invalid_or_opaque_atoms() {
    let key = key(4, 2);
    let source = Bytes::new(key, "e\u{301}X".as_bytes(), 5);
    let mut engine = LayoutEngine::new(AllocationBudget::default()).unwrap();
    let page = engine.fill(key, &source, TopAnchor::default(), &NoCancellation).unwrap();
    assert_eq!(page.atoms(0)[0].source, 0..3);
    assert_eq!(page.selection_cells(0, 1..1).count(), 0);
    assert_eq!(page.selection_cells(0, 4..4).count(), 0);
    assert_eq!(page.selection_cells(0, 1..2).next(), Some(0..1));
    let source = Bytes::new(key, &[0xff], 5);
    let page = engine.fill(key, &source, TopAnchor::default(), &NoCancellation).unwrap();
    assert_eq!(page.selection_cells(0, 0..0).count(), 0);
    let mut cluster = String::from("e");
    cluster.extend(std::iter::repeat_n('\u{301}', DISPLAY_BYTES));
    let source = Bytes::new(key, cluster.as_bytes(), 7);
    let page = engine.fill(key, &source, TopAnchor::default(), &NoCancellation).unwrap();
    assert_eq!(page.atoms(0)[0].kind, AtomKind::Opaque);
    assert_eq!(page.selection_cells(0, 1..1).count(), 0);
    assert_eq!(page.selection_cells(0, 1..2).next(), Some(0..1));
}

#[test]
fn wrap_duplicate_caret_normalization_preserves_primary_affinity_and_goal_in_both_orders() {
    let budget = AllocationBudget::default();
    let primary = VisualCursor {
        anchor: 4,
        head: 4,
        anchor_affinity: CaretAffinity::Upstream,
        head_affinity: CaretAffinity::Upstream,
        desired_cell: Some(4),
    };
    let secondary = VisualCursor {
        anchor_affinity: CaretAffinity::Downstream,
        head_affinity: CaretAffinity::Downstream,
        desired_cell: Some(0),
        ..primary
    };
    for (cursors, index) in [([primary, secondary], 0), ([secondary, primary], 1)] {
        let normalized = CursorSet::new(&budget, &cursors, index).unwrap().normalized(&budget).unwrap();
        assert_eq!(normalized.cursors(), &[primary]);
        assert_eq!(normalized.primary, 0);
    }
}

#[test]
fn wrap_page_aggregate_refuses_before_allocator_and_counts_transient_replacement() {
    let session = AllocationBudget::default();
    let metadata = AllocationBudget::new(0).retained_bytes();
    let aggregate = AllocationBudget::new(metadata + 256);
    let _header = aggregate.reserve(64).unwrap();
    let mut rows = AccountedVec::<u64>::with_aggregate(&session, &aggregate, 64, 512).unwrap();
    let mut atoms = AccountedVec::<u64>::with_aggregate(&session, &aggregate, 64, 512).unwrap();
    let mut syntax = AccountedVec::<u64>::with_aggregate(&session, &aggregate, 64, 512).unwrap();
    let before = aggregate.allocation_attempts();
    assert!(matches!(rows.reserve_len(25), Err(WrapError::Admission(_))));
    assert_eq!(aggregate.allocation_attempts(), before, "initial admission precedes allocator call");
    rows.reserve_len(8).unwrap();
    atoms.reserve_len(8).unwrap();
    syntax.reserve_len(8).unwrap();
    assert_eq!(aggregate.retained_bytes(), metadata + 256);
    let before = aggregate.allocation_attempts();
    for vector in [&mut rows, &mut atoms, &mut syntax] {
        assert!(matches!(vector.reserve_len(9), Err(WrapError::Admission(_))));
        assert_eq!(vector.data.capacity(), 8);
    }
    assert_eq!(aggregate.allocation_attempts(), before, "aggregate refusal must not allocate then check");
    drop(syntax);
    // Final replacement (96 bytes) could fit after freeing the old 64 bytes,
    // but their simultaneous 160 bytes do not fit the 64 bytes still available.
    assert!(matches!(rows.reserve_len(12), Err(WrapError::Admission(_))));
    assert_eq!(aggregate.allocation_attempts(), before);
    assert!(aggregate.peak_bytes() <= metadata + 256);
    assert!(session.peak_bytes() <= MAX_SESSION_BYTES);
    let key = key(3, 3);
    let source = Bytes::new(key, b"abcdefghi", 5);
    let mut engine = LayoutEngine::new(session).unwrap();
    let page = engine.fill(key, &source, TopAnchor::default(), &NoCancellation).unwrap();
    assert_eq!(page.rows().len(), 3);
    assert_eq!(
        page.aggregate_bytes(),
        page.retained_bytes(),
        "all real page buffers and metadata must participate in aggregate admission"
    );
    assert!(page.aggregate_peak_bytes() <= MAX_PAGE_BYTES);
}

struct DroppingSource {
    bytes: Bytes,
    gate: Arc<Gate>,
    worker: std::thread::ThreadId,
}
impl WrapSource for DroppingSource {
    fn run_layout(
        self: Arc<Self>,
        engine: crate::visual_rows::LayoutEngine,
        request: crate::visual_rows::task::TaskRequest,
        driver: &mut dyn crate::visual_rows::TaskDriver,
    ) -> crate::visual_rows::WorkerExit {
        crate::visual_rows::fixtures::run(self, engine, request, driver)
    }
    fn identity(&self) -> SourceIdentity {
        self.bytes.identity()
    }
    fn len(&self) -> u64 {
        self.bytes.len()
    }
    fn line_count(&self) -> u64 {
        self.bytes.line_count()
    }
    fn line_start(&self, line: u64) -> io::Result<Option<u64>> {
        self.bytes.line_start(line)
    }
    fn line_of(&self, byte: u64) -> io::Result<u64> {
        self.bytes.line_of(byte)
    }
    fn read(&self, at: u64, output: &mut [u8]) -> io::Result<usize> {
        self.bytes.read(at, output)
    }
}
impl Drop for DroppingSource {
    fn drop(&mut self) {
        assert_ne!(std::thread::current().id(), self.worker, "last-owner destruction must be off the caller/UI lane");
        self.gate.wait().unwrap();
    }
}

#[test]
fn wrap_blocking_retirement_drop_keeps_slot_reserved_until_worker_destruction_finishes() {
    let key = key(1, 1);
    let (entered, waiting) = mpsc::sync_channel(1);
    let gate = Arc::new(Gate { entered, release: Mutex::new(false), changed: Condvar::new() });
    let owner: Arc<dyn WrapSource> = Arc::new(DroppingSource {
        bytes: Bytes::new(key, b"x", 5),
        gate: gate.clone(),
        worker: std::thread::current().id(),
    });
    let session = WrapSession::new(|| {}).unwrap();
    session.retire_owner(RetiredOwner::Source(owner)).unwrap();
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    let second = session.reserve_retirement().unwrap();
    assert!(matches!(session.reserve_retirement(), Err(WrapError::Busy)), "in-progress Drop still occupies first slot");
    gate.open();
    drop(second);
    settle(session.stop());
}

struct FailOnceSource {
    bytes: Bytes,
    failures: AtomicUsize,
}
impl WrapSource for FailOnceSource {
    fn run_layout(
        self: Arc<Self>,
        engine: crate::visual_rows::LayoutEngine,
        request: crate::visual_rows::task::TaskRequest,
        driver: &mut dyn crate::visual_rows::TaskDriver,
    ) -> crate::visual_rows::WorkerExit {
        crate::visual_rows::fixtures::run(self, engine, request, driver)
    }
    fn identity(&self) -> SourceIdentity {
        self.bytes.identity()
    }
    fn len(&self) -> u64 {
        self.bytes.len()
    }
    fn line_count(&self) -> u64 {
        self.bytes.line_count()
    }
    fn line_start(&self, line: u64) -> io::Result<Option<u64>> {
        self.bytes.line_start(line)
    }
    fn line_of(&self, byte: u64) -> io::Result<u64> {
        self.bytes.line_of(byte)
    }
    fn read(&self, at: u64, output: &mut [u8]) -> io::Result<usize> {
        if self.failures.swap(0, Ordering::AcqRel) != 0 {
            return Err(io::Error::other("controlled first read failure"));
        }
        self.bytes.read(at, output)
    }
}

#[test]
fn wrap_relative_io_failure_retains_head_and_exact_source_for_explicit_retry() {
    let key = key(2, 1);
    let source: Arc<dyn WrapSource> =
        Arc::new(FailOnceSource { bytes: Bytes::new(key, b"abcdef", 5), failures: AtomicUsize::new(1) });
    let wakes = Arc::new(WakeLatch::default());
    let notify = wakes.clone();
    let mut session = WrapSession::new(move || notify.wake()).unwrap();
    let id = session.relative.enqueue(RelativeInput { key, kind: RelativeKind::Scroll(1) }).unwrap();
    session.relative.begin(key).unwrap();
    let current = fence(key, 1);
    session
        .submit(WrapRequest {
            fence: current,
            source: source.clone(),
            anchor: TopAnchor::default(),
            intent: LayoutIntent::ScrollRows(1),
        })
        .unwrap();
    let mut seen = 0;
    let failed = update(&mut session, current, &wakes, &mut seen);
    assert!(matches!(failed.result, Err(WrapError::Source(kwil_core_text::compare_input::InputError::Io(_)))));
    assert_eq!(failed.relative_id, Some(id));
    assert!(Arc::ptr_eq(&failed.source, &source));
    assert_eq!(session.relative.head().unwrap().id, id);
    assert_eq!(session.relative.mutation_gate(), MutationGate::RefusedFailed);
    assert_eq!(session.published_relative(id, current), Err(QueueError::WrongHead));
    let retry_source = failed.source;
    session.retry_relative(id).unwrap();
    assert_eq!(session.relative.mutation_gate(), MutationGate::RefusedPending);
    session
        .submit(WrapRequest {
            fence: current,
            source: retry_source,
            anchor: TopAnchor::default(),
            intent: LayoutIntent::ScrollRows(1),
        })
        .unwrap();
    let success = update(&mut session, current, &wakes, &mut seen);
    assert!(success.result.is_ok());
    assert!(Arc::ptr_eq(&success.source, &source));
    assert_eq!(session.relative.head().unwrap().id, id, "delivery alone is not host publication");
    // This source test verifies protocol acknowledgement only. GUI/TUI
    // publication coupling remains ROOT's integration acceptance.
    session.published_relative(id, current).unwrap();
    assert!(session.relative.is_empty());
    drop(success);
    settle(session.stop());
}

#[test]
fn wrap_normalized_upstream_anchor_refills_same_page_and_scrolls_from_selected_row() {
    let key = key(4, 2);
    let source = Bytes::new(key, b"abcdefghijkl", 5);
    let mut engine = LayoutEngine::new(AllocationBudget::default()).unwrap();
    let page = engine
        .fill(
            key,
            &source,
            TopAnchor { source_byte: 8, affinity: CaretAffinity::Upstream, intra_row_offset: 1 },
            &NoCancellation,
        )
        .unwrap();
    assert_eq!(ranges(&page), vec![4..8, 8..12]);
    assert_eq!(page.anchor, TopAnchor { source_byte: 4, affinity: CaretAffinity::Downstream, intra_row_offset: 1 });
    let refill = engine.fill(key, &source, page.anchor, &NoCancellation).unwrap();
    assert_eq!(ranges(&refill), ranges(&page));
    assert_eq!(refill.anchor, page.anchor);
    let still = engine.scroll(key, &source, page.anchor, 0, &NoCancellation).unwrap();
    assert_eq!(ranges(&still), ranges(&page));
    let down = engine.scroll(key, &source, page.anchor, 1, &NoCancellation).unwrap();
    assert_eq!(down.rows().len(), 1);
    assert_eq!(down.rows()[0].source_coverage, 8..12);
    assert_eq!(down.anchor.affinity, CaretAffinity::Downstream);
    let up = engine.scroll(key, &source, page.anchor, -1, &NoCancellation).unwrap();
    assert_eq!(ranges(&up), vec![0..4, 4..8]);
    let back = engine.scroll(key, &source, down.anchor, -1, &NoCancellation).unwrap();
    assert_eq!(ranges(&back), ranges(&page));
    assert_eq!(back.anchor, page.anchor);
}

struct PanickingSource {
    bytes: Bytes,
    gate: Arc<Gate>,
}
impl WrapSource for PanickingSource {
    fn run_layout(
        self: Arc<Self>,
        engine: crate::visual_rows::LayoutEngine,
        request: crate::visual_rows::task::TaskRequest,
        driver: &mut dyn crate::visual_rows::TaskDriver,
    ) -> crate::visual_rows::WorkerExit {
        crate::visual_rows::fixtures::run(self, engine, request, driver)
    }
    fn identity(&self) -> SourceIdentity {
        self.bytes.identity()
    }
    fn len(&self) -> u64 {
        self.bytes.len()
    }
    fn line_count(&self) -> u64 {
        self.bytes.line_count()
    }
    fn line_start(&self, line: u64) -> io::Result<Option<u64>> {
        self.bytes.line_start(line)
    }
    fn line_of(&self, byte: u64) -> io::Result<u64> {
        self.bytes.line_of(byte)
    }
    fn read(&self, _at: u64, _output: &mut [u8]) -> io::Result<usize> {
        self.gate.wait().unwrap();
        panic!("controlled relative source panic");
    }
}

#[test]
fn wrap_terminal_relative_panic_wakes_once_preserves_fifo_and_settles_exact_owners() {
    let key = key(4, 2);
    let (read_entered, reading) = mpsc::sync_channel(1);
    let read_gate = Arc::new(Gate { entered: read_entered, release: Mutex::new(false), changed: Condvar::new() });
    let source: Arc<dyn WrapSource> =
        Arc::new(PanickingSource { bytes: Bytes::new(key, b"abcdefghijkl", 5), gate: read_gate.clone() });
    let (wake_entered, woke) = mpsc::sync_channel(2);
    let wake_gate = Arc::new(Gate { entered: wake_entered, release: Mutex::new(false), changed: Condvar::new() });
    let notify = wake_gate.clone();
    let wake_calls = Arc::new(AtomicUsize::new(0));
    let notified = wake_calls.clone();
    let mut session = WrapSession::new(move || {
        notified.fetch_add(1, Ordering::AcqRel);
        notify.wait().unwrap();
    })
    .unwrap();
    let head = session.relative.enqueue(RelativeInput { key, kind: RelativeKind::Scroll(1) }).unwrap();
    let tail = session.relative.enqueue(RelativeInput { key, kind: RelativeKind::Scroll(-1) }).unwrap();
    session.relative.begin(key).unwrap();
    let current = fence(key, 1);
    session
        .submit(WrapRequest {
            fence: current,
            source,
            anchor: TopAnchor::default(),
            intent: LayoutIntent::ScrollRows(1),
        })
        .unwrap();
    reading.recv_timeout(Duration::from_secs(2)).unwrap();
    let held_permit = session.reserve_retirement().unwrap();
    let desired: Arc<dyn WrapSource> = Arc::new(Bytes::new(key, b"owned", 5));
    let retained = Arc::downgrade(&desired);
    session.submit(request(key, 2, desired)).unwrap();
    read_gate.open();
    // The callback is held after publishing terminal status and before owner
    // cleanup. Reading that status must not need another wake or worker result.
    woke.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(retained.upgrade().is_some());
    assert!(matches!(session.terminal_failure(), Some(WrapError::WorkerPanicked)));
    assert_eq!(session.relative.head().unwrap().id, head);
    assert_eq!(session.relative.len(), 2);
    assert_eq!(session.relative.record_ids().collect::<Vec<_>>(), vec![head, tail]);
    assert_eq!(session.relative.mutation_gate(), MutationGate::RefusedFailed);
    assert!(session.take_registered_update(Some(current), None).is_none());
    assert!(matches!(session.retry_relative(head), Err(WrapError::WorkerPanicked)));
    assert_eq!(session.relative.mutation_gate(), MutationGate::RefusedFailed);
    let rejected: Arc<dyn WrapSource> = Arc::new(Bytes::new(key, b"rejected", 5));
    let refused = session
        .submit(WrapRequest {
            fence: current,
            source: rejected.clone(),
            anchor: TopAnchor::default(),
            intent: LayoutIntent::ScrollRows(1),
        })
        .unwrap_err();
    assert!(matches!(refused.error, WrapError::WorkerPanicked));
    assert!(Arc::ptr_eq(&refused.request.source, &rejected));
    assert_eq!(session.relative.record_ids().collect::<Vec<_>>(), vec![head, tail]);
    drop(refused);
    assert!(matches!(session.reserve_retirement(), Err(WrapError::WorkerPanicked)));
    assert!(matches!(session.terminal_failure(), Some(WrapError::WorkerPanicked)));
    assert!(matches!(woke.try_recv(), Err(mpsc::TryRecvError::Empty)));
    assert_eq!(wake_calls.load(Ordering::Acquire), 1);
    let ticket = session.stop();
    wake_gate.open();
    let failure = ticket.settle(Duration::from_millis(5)).unwrap_err();
    assert!(
        matches!(failure.error, WrapError::TimedOut),
        "panic cleanup must not acknowledge an external active permit"
    );
    drop(held_permit);
    let failure = crate::wrap_session::settle_final(failure.ticket, Duration::from_secs(2)).unwrap_err();
    assert!(matches!(failure.error, WrapError::WorkerPanicked));
    assert!(retained.upgrade().is_none());
    assert!(matches!(woke.try_recv(), Err(mpsc::TryRecvError::Empty)));
    assert_eq!(wake_calls.load(Ordering::Acquire), 1);
    std::mem::forget(failure);
}

fn atomic_update(
    session: &mut WrapSession,
    relative: Option<NavigationFence>,
    desired: Option<NavigationFence>,
    wakes: &WakeLatch,
    seen: &mut u64,
) -> (LayoutUpdate, RetirementPermit, RetirementPermit) {
    for _ in 0..8 {
        match session.take_registered_update_with_retirement(relative, desired) {
            Ok(Some(owned)) => return owned,
            Ok(None) | Err(WrapError::Busy) => wakes.next(seen),
            Err(error) => panic!("unexpected intake refusal: {error:?}"),
        }
    }
    panic!("bounded worker events did not produce an admitted update");
}

#[test]
fn wrap_atomic_intake_stale_ready_with_two_reserved_slots_wakes_and_worker_continues() {
    let key = key(4, 2);
    let (dropping, entered_drop) = mpsc::sync_channel(1);
    let gate = Arc::new(Gate { entered: dropping, release: Mutex::new(false), changed: Condvar::new() });
    let source: Arc<dyn WrapSource> = Arc::new(DroppingSource {
        bytes: Bytes::new(key, b"abcdefghijkl", 5),
        gate: gate.clone(),
        worker: std::thread::current().id(),
    });
    let retained = Arc::downgrade(&source);
    let wakes = Arc::new(WakeLatch::default());
    let notify = wakes.clone();
    let mut session = WrapSession::new(move || notify.wake()).unwrap();
    let first = session.reserve_retirement().unwrap();
    let second = session.reserve_retirement().unwrap();
    session.submit(request(key, 1, source)).unwrap();
    let mut seen = 0;
    wakes.next(&mut seen);
    let current = fence(key, 2);
    let mut busy = false;
    for _ in 0..8 {
        match session.take_registered_update_with_retirement(None, Some(current)) {
            Err(WrapError::Busy) => {
                busy = true;
                break;
            }
            Ok(None) => wakes.next(&mut seen),
            _ => panic!("stale or saturated owner left the mailbox"),
        }
    }
    assert!(busy);
    assert!(retained.upgrade().is_some());
    // Only one slot becomes free. A capacity event must wake the UI, and stale
    // retirement must use that one slot BEFORE attempting two-slot valid intake.
    drop(first);
    wakes.next(&mut seen);
    assert!(session.take_registered_update_with_retirement(None, Some(current)).unwrap().is_none());
    entered_drop.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(
        matches!(session.reserve_retirement(), Err(WrapError::Busy)),
        "stale owner Drop and the second permit still occupy both slots"
    );
    let next: Arc<dyn WrapSource> = Arc::new(Bytes::new(key, b"next", 5));
    session.submit(request(key, 2, next.clone())).unwrap();
    drop(second);
    gate.open();
    let (update, source_permit, publication_permit) =
        atomic_update(&mut session, None, Some(current), &wakes, &mut seen);
    assert_eq!(update.fence, current);
    assert!(update.result.is_ok());
    assert!(Arc::ptr_eq(&update.source, &next));
    assert!(source_permit.is_current());
    assert!(publication_permit.is_current());
    assert!(matches!(session.reserve_retirement(), Err(WrapError::Busy)), "valid take atomically reserves both slots");
    assert!(retained.upgrade().is_none());
    source_permit.publish(|| ((), Some(RetiredOwner::Update(update)))).unwrap();
    drop(publication_permit);
    settle(session.stop());
}

#[test]
fn wrap_atomic_intake_keeps_relative_failure_retry_and_publication_ack_bookkeeping() {
    let key = key(2, 1);
    let source: Arc<dyn WrapSource> =
        Arc::new(FailOnceSource { bytes: Bytes::new(key, b"abcdef", 5), failures: AtomicUsize::new(1) });
    let wakes = Arc::new(WakeLatch::default());
    let notify = wakes.clone();
    let mut session = WrapSession::new(move || notify.wake()).unwrap();
    let first = session.reserve_retirement().unwrap();
    let second = session.reserve_retirement().unwrap();
    let id = session.relative.enqueue(RelativeInput { key, kind: RelativeKind::Scroll(1) }).unwrap();
    session.relative.begin(key).unwrap();
    let current = fence(key, 1);
    session
        .submit(WrapRequest {
            fence: current,
            source: source.clone(),
            anchor: TopAnchor::default(),
            intent: LayoutIntent::ScrollRows(1),
        })
        .unwrap();
    let mut seen = 0;
    wakes.next(&mut seen);
    assert!(matches!(session.take_registered_update_with_retirement(Some(current), None), Err(WrapError::Busy)));
    assert_eq!(session.relative.head().unwrap().id, id);
    assert_eq!(
        session.relative.mutation_gate(),
        MutationGate::RefusedPending,
        "Busy has not delivered the IO error or consumed its owner"
    );
    drop(first);
    assert!(
        matches!(session.take_registered_update_with_retirement(Some(current), None), Err(WrapError::Busy)),
        "valid intake requires BOTH slots atomically"
    );
    drop(second);
    wakes.next(&mut seen);
    let (failed, source_permit, publication_permit) =
        atomic_update(&mut session, Some(current), None, &wakes, &mut seen);
    assert!(matches!(failed.result, Err(WrapError::Source(kwil_core_text::compare_input::InputError::Io(_)))));
    assert_eq!(failed.relative_id, Some(id));
    assert!(Arc::ptr_eq(&failed.source, &source));
    assert_eq!(session.relative.mutation_gate(), MutationGate::RefusedFailed);
    assert_eq!(session.published_relative(id, current), Err(QueueError::WrongHead));
    source_permit.publish(|| ((), Some(RetiredOwner::Update(failed)))).unwrap();
    drop(publication_permit);
    session.retry_relative(id).unwrap();
    session
        .submit(WrapRequest {
            fence: current,
            source: source.clone(),
            anchor: TopAnchor::default(),
            intent: LayoutIntent::ScrollRows(1),
        })
        .unwrap();
    let (success, source_permit, publication_permit) =
        atomic_update(&mut session, Some(current), None, &wakes, &mut seen);
    assert!(success.result.is_ok());
    assert_eq!(success.relative_id, Some(id));
    assert!(Arc::ptr_eq(&success.source, &source));
    assert_eq!(session.relative.head().unwrap().id, id);
    assert_eq!(session.relative.mutation_gate(), MutationGate::RefusedPending);
    // Protocol publication acknowledgement, not GUI/TUI acceptance.
    session.published_relative(id, current).unwrap();
    assert!(session.relative.is_empty());
    source_permit.publish(|| ((), Some(RetiredOwner::Update(success)))).unwrap();
    drop(publication_permit);
    settle(session.stop());
}

struct CancelCleanupSource {
    bytes: Bytes,
    read_gate: Arc<Gate>,
    cleanup_gate: Arc<Gate>,
    caller: std::thread::ThreadId,
}
impl WrapSource for CancelCleanupSource {
    fn run_layout(
        self: Arc<Self>,
        engine: crate::visual_rows::LayoutEngine,
        request: crate::visual_rows::task::TaskRequest,
        driver: &mut dyn crate::visual_rows::TaskDriver,
    ) -> crate::visual_rows::WorkerExit {
        crate::visual_rows::fixtures::run(self, engine, request, driver)
    }
    fn identity(&self) -> SourceIdentity {
        self.bytes.identity()
    }
    fn len(&self) -> u64 {
        self.bytes.len()
    }
    fn line_count(&self) -> u64 {
        self.bytes.line_count()
    }
    fn line_start(&self, line: u64) -> io::Result<Option<u64>> {
        self.bytes.line_start(line)
    }
    fn line_of(&self, byte: u64) -> io::Result<u64> {
        self.bytes.line_of(byte)
    }
    fn read(&self, at: u64, output: &mut [u8]) -> io::Result<usize> {
        self.read_gate.wait()?;
        self.bytes.read(at, output)
    }
}
impl Drop for CancelCleanupSource {
    fn drop(&mut self) {
        assert_ne!(std::thread::current().id(), self.caller);
        self.cleanup_gate.wait().unwrap();
    }
}

#[test]
fn wrap_canceled_cleanup_wakes_busy_relative_head_once_then_events_deliver_same_owner() {
    let key = key(4, 2);
    let (reading, read_entered) = mpsc::sync_channel(1);
    let read_gate = Arc::new(Gate { entered: reading, release: Mutex::new(false), changed: Condvar::new() });
    let (dropping, cleanup_entered) = mpsc::sync_channel(1);
    let cleanup_gate = Arc::new(Gate { entered: dropping, release: Mutex::new(false), changed: Condvar::new() });
    let old: Arc<dyn WrapSource> = Arc::new(CancelCleanupSource {
        bytes: Bytes::new(key, b"abcdefghijkl", 5),
        read_gate: read_gate.clone(),
        cleanup_gate: cleanup_gate.clone(),
        caller: std::thread::current().id(),
    });
    let (wake, events) = mpsc::sync_channel(8);
    let calls = Arc::new(AtomicUsize::new(0));
    let notified = calls.clone();
    let mut session = WrapSession::new(move || {
        notified.fetch_add(1, Ordering::AcqRel);
        let _ = wake.try_send(());
    })
    .unwrap();
    session.submit(request(key, 1, old)).unwrap();
    read_entered.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(session.cancel(CancelReason::Escape).records, 0);
    let id = session
        .relative
        .enqueue(RelativeInput {
            key,
            kind: RelativeKind::Move { motion: VisualMotion::Down, count: 1, extend: false },
        })
        .unwrap();
    session.relative.begin(key).unwrap();
    let current = fence(key, 2);
    let source: Arc<dyn WrapSource> = Arc::new(Bytes::new(key, b"abcdefghijkl", 5));
    let expected = cursors(session.budget(), &[0]);
    let expected_pointer = expected.cursors().as_ptr();
    let refused = session
        .submit(WrapRequest {
            fence: current,
            source: source.clone(),
            anchor: TopAnchor::default(),
            intent: LayoutIntent::MoveCursors {
                cursors: expected,
                motion: VisualMotion::Down,
                count: 1,
                extend: false,
            },
        })
        .unwrap_err();
    assert!(matches!(refused.error, WrapError::Busy));
    assert!(Arc::ptr_eq(&refused.request.source, &source));
    assert_eq!(session.relative.head().unwrap().id, id);
    assert_eq!(session.relative.len(), 1);
    assert_eq!(session.relative.mutation_gate(), MutationGate::RefusedPending);
    assert!(matches!(events.try_recv(), Err(mpsc::TryRecvError::Empty)));
    // Keep the unaccepted request LOCAL. Retiring it through the worker would
    // introduce an unrelated capacity wake and mask the canceled-completion bug.
    read_gate.open();
    cleanup_entered.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(calls.load(Ordering::Acquire), 0, "read completion is not owner-cleanup completion");
    assert!(matches!(events.try_recv(), Err(mpsc::TryRecvError::Empty)));
    let refused = session.submit(refused.request).unwrap_err();
    assert!(matches!(refused.error, WrapError::Busy), "old owner destruction still occupies the worker");
    assert!(Arc::ptr_eq(&refused.request.source, &source));
    cleanup_gate.open();
    // Receive FIRST: no eager submission, polling, retirement, or timer may
    // manufacture the admission event which the canceled worker must produce.
    events.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert!(matches!(events.try_recv(), Err(mpsc::TryRecvError::Empty)));
    assert_eq!(session.relative.head().unwrap().id, id);
    session.submit(refused.request).unwrap();
    let mut delivered = None;
    for _ in 0..8 {
        events.recv_timeout(Duration::from_secs(2)).unwrap();
        match session.take_registered_update_with_retirement(Some(current), None) {
            Ok(Some(owned)) => {
                delivered = Some(owned);
                break;
            }
            Ok(None) | Err(WrapError::Busy) => {}
            Err(error) => panic!("unexpected relative intake failure: {error:?}"),
        }
    }
    let (update, source_permit, publication_permit) = delivered.expect("same head did not complete from worker events");
    assert_eq!(update.relative_id, Some(id));
    assert_eq!(update.fence, current);
    assert!(Arc::ptr_eq(&update.source, &source));
    let navigation = match &update.result {
        Ok(LayoutPayload::Navigation(navigation)) => navigation,
        _ => panic!("accepted navigation did not resolve"),
    };
    assert_eq!(navigation.expected.cursors().as_ptr(), expected_pointer);
    assert_eq!(navigation.targets.cursors()[0].head, 4);
    assert_eq!(session.relative.head().unwrap().id, id, "delivery alone does not publish or replace the head");
    // Protocol acknowledgement only; the sibling host regression checks the
    // actual Editor publication and caret through this same event sequence.
    session.published_relative(id, current).unwrap();
    assert!(session.relative.is_empty());
    source_permit.publish(|| ((), Some(RetiredOwner::Update(update)))).unwrap();
    drop(publication_permit);
    settle(session.stop());
}

#[test]
fn wrap_canceled_cleanup_without_busy_relative_input_adds_no_wake() {
    let key = key(4, 2);
    let (reading, read_entered) = mpsc::sync_channel(1);
    let read_gate = Arc::new(Gate { entered: reading, release: Mutex::new(false), changed: Condvar::new() });
    let (dropping, cleanup_entered) = mpsc::sync_channel(1);
    let cleanup_gate = Arc::new(Gate { entered: dropping, release: Mutex::new(false), changed: Condvar::new() });
    let old: Arc<dyn WrapSource> = Arc::new(CancelCleanupSource {
        bytes: Bytes::new(key, b"abcdefghijkl", 5),
        read_gate: read_gate.clone(),
        cleanup_gate: cleanup_gate.clone(),
        caller: std::thread::current().id(),
    });
    let (wake, events) = mpsc::sync_channel(8);
    let calls = Arc::new(AtomicUsize::new(0));
    let notified = calls.clone();
    let mut session = WrapSession::new(move || {
        notified.fetch_add(1, Ordering::AcqRel);
        let _ = wake.try_send(());
    })
    .unwrap();
    session.submit(request(key, 1, old)).unwrap();
    read_entered.recv_timeout(Duration::from_secs(2)).unwrap();
    session.cancel(CancelReason::Escape);
    read_gate.open();
    cleanup_entered.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(events.try_recv(), Err(mpsc::TryRecvError::Empty)));
    // A zero-height fill gives an actual completion with no source scan or
    // progress callbacks. There was no Busy relative refusal to arm a wake.
    let mut zero = key;
    zero.geometry.rows = 0;
    zero.layout_generation += 1;
    let source: Arc<dyn WrapSource> = Arc::new(Bytes::new(zero, b"abcdefghijkl", 5));
    session.submit(request(zero, 2, source)).unwrap();
    cleanup_gate.open();
    let mut delivered = None;
    for _ in 0..8 {
        events.recv_timeout(Duration::from_secs(2)).unwrap();
        match session.take_registered_update_with_retirement(None, Some(fence(zero, 2))) {
            Ok(Some(owned)) => {
                delivered = Some(owned);
                break;
            }
            Ok(None) | Err(WrapError::Busy) => {}
            Err(error) => panic!("unexpected desired intake failure: {error:?}"),
        }
    }
    let (update, source_permit, publication_permit) = delivered.expect("new fill did not complete");
    assert_eq!(calls.load(Ordering::Acquire), 1, "only the new fill completion may wake; canceled cleanup was unarmed");
    assert!(matches!(events.try_recv(), Err(mpsc::TryRecvError::Empty)));
    assert!(session.relative.is_empty());
    assert_eq!(session.relative.mutation_gate(), MutationGate::Allowed);
    match &update.result {
        Ok(LayoutPayload::Page(page)) => assert!(page.rows().is_empty()),
        _ => panic!("zero-height fill did not complete"),
    }
    source_permit.publish(|| ((), Some(RetiredOwner::Update(update)))).unwrap();
    drop(publication_permit);
    settle(session.stop());
}
