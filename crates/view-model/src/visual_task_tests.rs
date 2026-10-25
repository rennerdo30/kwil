//! Authored SOURCE ONLY; NOT RUN. ROOT owns module registration and validation.
use super::*;
use crate::visual_rows::{BodyOwner, NavigationCommand, NavigationFence, NoCancellation, VisualCursor, WrapSource};
use std::{
    io,
    mem::size_of_val,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

#[test]
fn prepaid_access_is_exact_and_cannot_change_or_survive_cancellation() {
    let mut credit = WorkCredit::default();
    assert!(matches!(
        credit.poll(ReadWork::new(2, 1), &mut ReadWork::new(1, 1), &AtomicBool::new(false)),
        TaskPoll::Pending
    ));
    assert!(matches!(
        credit.poll(ReadWork::new(3, 1), &mut ReadWork::new(1, 1), &AtomicBool::new(false)),
        TaskPoll::Failed(_)
    ));
    assert!(matches!(
        credit.poll(ReadWork::new(2, 1), &mut ReadWork::new(1, 1), &AtomicBool::new(true)),
        TaskPoll::Cancelled
    ));
    assert!(matches!(
        credit.poll(ReadWork::new(2, 1), &mut ReadWork::new(1, 1), &AtomicBool::new(false)),
        TaskPoll::Pending
    ));
    assert!(matches!(
        credit.poll(ReadWork::new(2, 1), &mut ReadWork::new(1, 0), &AtomicBool::new(false)),
        TaskPoll::Ready(ReadWork { bytes: 2, operations: 1 })
    ));
}

#[test]
fn zero_geometry_has_no_source_work_and_returns_the_exact_owner() {
    let key = key(0, 0);
    let source = Bytes::new(key, b"unchanged", 1);
    let owner = source.owner.clone();
    let task = begin(source, TaskRequest::Fill { key, anchor: TopAnchor::default() }, AllocationBudget::default(), 1);
    let (_, source, _, output) = run(task, ReadWork::new(0, 0));
    assert!(Arc::ptr_eq(&source.owner, &owner));
    assert_eq!(owner.reads.load(Ordering::Relaxed), 0);
    assert!(page(output).rows().is_empty());
}

struct Query {
    at: u64,
    length: usize,
    credit: WorkCredit,
}
impl Query {
    fn new(at: u64, length: usize) -> Self {
        Self { at, length, credit: WorkCredit::default() }
    }
}
#[derive(Default)]
struct Counts {
    reads: AtomicUsize,
    bytes: AtomicUsize,
    revision: AtomicU64,
    provenance: AtomicUsize,
}
struct Bytes {
    key: LayoutKey,
    text: Vec<u8>,
    starts: Vec<u64>,
    opaque: Vec<Range<u64>>,
    short: usize,
    unit_provenance: bool,
    owner: Arc<Counts>,
}
impl Bytes {
    fn new(key: LayoutKey, text: &[u8], short: usize) -> Self {
        let mut starts = vec![0];
        starts.extend(text.iter().enumerate().filter_map(|(i, b)| (*b == b'\n').then_some(i as u64 + 1)));
        Self {
            key,
            text: text.to_vec(),
            starts,
            opaque: Vec::new(),
            short,
            unit_provenance: false,
            owner: Arc::default(),
        }
    }
    fn span(&self, line: usize) -> LineSpan {
        let start = self.starts[line];
        let end = self.starts.get(line + 1).copied().unwrap_or(self.text.len() as u64);
        let terminator = if self.starts.get(line + 1).is_none() {
            Terminator::None
        } else if end > start + 1 && self.text[end as usize - 2] == b'\r' {
            Terminator::CrLf
        } else {
            Terminator::Lf
        };
        let count = match terminator {
            Terminator::None => 0,
            Terminator::Lf => 1,
            Terminator::CrLf => 2,
        };
        LineSpan { start, end, content_end: end - count, terminator }
    }
}
impl CooperativeSource for Bytes {
    type LineQuery = Query;
    type RankQuery = Query;
    type ReadQuery = Query;
    type ProvenanceQuery = Query;
    fn metadata(&self) -> SourceMetadata {
        let mut identity = self.key.source;
        identity.revision += self.owner.revision.load(Ordering::Relaxed);
        SourceMetadata { identity, len: self.text.len() as u64, line_count: self.starts.len() as u64 }
    }
    fn new_line_query(&self, line: u64) -> Query {
        Query::new(line, 0)
    }
    fn new_rank_query(&self, at: u64) -> Query {
        Query::new(at, 0)
    }
    fn new_read_query(&self, at: u64, length: usize, _: &AllocationBudget) -> Result<Query, WrapError> {
        Ok(Query::new(at, length))
    }
    fn new_provenance_query(&self, at: u64) -> Query {
        Query::new(at, 0)
    }
    fn poll_line(&self, query: &mut Query, work: &mut ReadWork, cancel: &AtomicBool) -> TaskPoll<LineSpan> {
        ready!(query.credit.poll(ReadWork::new(size_of::<LineSpan>() as u64 + 2, 1), work, cancel));
        if query.at >= self.starts.len() as u64 {
            return TaskPoll::Failed(InputError::Range.into());
        }
        TaskPoll::Ready(self.span(query.at as usize))
    }
    fn poll_rank(&self, query: &mut Query, work: &mut ReadWork, cancel: &AtomicBool) -> TaskPoll<Rank> {
        ready!(query.credit.poll(ReadWork::new(self.starts.len() as u64 * 8, 1), work, cancel));
        if query.at > self.text.len() as u64 {
            return TaskPoll::Failed(InputError::Range.into());
        }
        let line = self.starts.partition_point(|&at| at <= query.at).saturating_sub(1);
        TaskPoll::Ready(Rank { line: line as u64, column: query.at - self.starts[line] })
    }
    fn poll_read(
        &self,
        query: &mut Query,
        output: &mut [u8],
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> TaskPoll<usize> {
        ready!(query.credit.poll(ReadWork::new(query.length as u64 * 2, 1), work, cancel));
        if query.at > self.text.len() as u64 || query.length != output.len() {
            return TaskPoll::Failed(InputError::Range.into());
        }
        let n = query.length.min(self.short).min(self.text.len() - query.at as usize);
        self.owner.reads.fetch_add(1, Ordering::Relaxed);
        self.owner.bytes.fetch_add(n, Ordering::Relaxed);
        output[..n].copy_from_slice(&self.text[query.at as usize..query.at as usize + n]);
        TaskPoll::Ready(n)
    }
    fn poll_provenance(&self, query: &mut Query, work: &mut ReadWork, cancel: &AtomicBool) -> TaskPoll<Provenance> {
        ready!(query.credit.poll(ReadWork::new(1 + self.opaque.len() as u64 * 16, 1), work, cancel));
        self.owner.provenance.fetch_add(1, Ordering::Relaxed);
        if let Some(span) = self.opaque.iter().find(|span| span.contains(&query.at)) {
            return TaskPoll::Ready(Provenance { source: span.clone(), opaque: true });
        }
        let start = self.opaque.iter().filter(|span| span.end <= query.at).map(|span| span.end).max().unwrap_or(0);
        let end = self
            .opaque
            .iter()
            .filter(|span| span.start > query.at)
            .map(|span| span.start)
            .min()
            .unwrap_or(self.text.len() as u64);
        let source = if self.unit_provenance { query.at..query.at + 1 } else { start..end };
        TaskPoll::Ready(Provenance { source, opaque: false })
    }
}
impl WrapSource for Bytes {
    fn identity(&self) -> SourceIdentity {
        self.metadata().identity
    }
    fn len(&self) -> u64 {
        self.text.len() as u64
    }
    fn line_count(&self) -> u64 {
        self.starts.len() as u64
    }
    fn line_start(&self, line: u64) -> io::Result<Option<u64>> {
        Ok(self.starts.get(line as usize).copied())
    }
    fn line_of(&self, byte: u64) -> io::Result<u64> {
        Ok(self.starts.partition_point(|&at| at <= byte).saturating_sub(1) as u64)
    }
    fn read(&self, at: u64, output: &mut [u8]) -> io::Result<usize> {
        let n = output.len().min(self.short).min(self.text.len() - at as usize);
        output[..n].copy_from_slice(&self.text[at as usize..at as usize + n]);
        Ok(n)
    }
}
fn key(columns: u16, rows: u16) -> LayoutKey {
    LayoutKey {
        source: SourceIdentity { document_id: 21, source_epoch: 28, incarnation: 3, revision: 7 },
        body: BodyOwner::Text,
        geometry: LayoutGeometry { columns, rows, tab_size: 4, ..LayoutGeometry::default() },
        ..LayoutKey::default()
    }
}
fn begin(source: Bytes, request: TaskRequest, budget: AllocationBudget, chunk: usize) -> VisualTask<Bytes> {
    let engine = LayoutEngine::with_chunk_limit(budget, chunk).unwrap();
    match engine.begin_task(source, request) {
        ControlFlow::Continue(task) => task,
        ControlFlow::Break(refusal) => panic!("{}", refusal.error),
    }
}
fn run(mut task: VisualTask<Bytes>, quantum: ReadWork) -> (LayoutEngine, Bytes, Option<TaskRequest>, TaskOutput) {
    for _ in 0..2_000_000 {
        match task.step(quantum, &AtomicBool::new(false)) {
            TaskStep::Pending => {}
            TaskStep::Ready => {
                let (engine, source, request, output) = task.into_parts();
                return (engine, source, request, output.expect("completed output"));
            }
            TaskStep::Cancelled => panic!("unexpected cancellation"),
            TaskStep::Failed(error) => panic!("{error:?}"),
        }
    }
    panic!("bounded fixture did not progress; retained queries must not restart")
}
fn page(output: TaskOutput) -> LayoutPage {
    match output.into_page() {
        ControlFlow::Continue(page) => page,
        ControlFlow::Break(_) => panic!("page output"),
    }
}
fn ranges(page: &LayoutPage) -> Vec<Range<u64>> {
    page.rows().iter().map(|row| row.source_coverage.clone()).collect()
}

#[test]
fn one_byte_quanta_and_short_reads_match_existing_layout() {
    let key = key(6, 8);
    let text = "one two\t界e\u{301}👩‍💻\r\n\nlast";
    let reference = Bytes::new(key, text.as_bytes(), 2);
    let mut engine = LayoutEngine::with_chunk_limit(AllocationBudget::default(), 3).unwrap();
    let expected = engine.fill(key, &reference, TopAnchor::default(), &NoCancellation).unwrap();
    let source = Bytes::new(key, text.as_bytes(), 2);
    let owner = source.owner.clone();
    let task = begin(source, TaskRequest::Fill { key, anchor: TopAnchor::default() }, AllocationBudget::default(), 3);
    let (_, source, _, output) = run(task, ReadWork::new(1, 1));
    assert!(Arc::ptr_eq(&owner, &source.owner));
    let actual = page(output);
    assert_eq!(actual.rows(), expected.rows());
    for row in 0..actual.rows().len() {
        assert_eq!(actual.atoms(row), expected.atoms(row));
    }
    assert_eq!(actual.syntax_windows(), expected.syntax_windows());
    assert_eq!(actual.anchor, expected.anchor);
    assert_eq!((actual.at_start, actual.at_end), (expected.at_start, expected.at_end));
}

#[test]
fn giant_combining_cluster_keeps_endpoint_and_constant_retained_state() {
    let key = key(1, 2);
    let text = format!("a{}Z", "\u{301}".repeat(300));
    let budget = AllocationBudget::default();
    let task = begin(
        Bytes::new(key, text.as_bytes(), 1),
        TaskRequest::Fill { key, anchor: TopAnchor::default() },
        budget.clone(),
        7,
    );
    let (_, source, _, output) = run(task, ReadWork::new(1, 1));
    let actual = page(output);
    assert_eq!(ranges(&actual), vec![0..601, 601..602]);
    assert_eq!(actual.atoms(0)[0].kind, AtomKind::Opaque);
    assert_eq!(actual.hit_test(0, 0).unwrap().byte, 0);
    assert_eq!(actual.hit_test(0, 1).unwrap().byte, 601);
    assert_eq!(source.text, text.as_bytes());
    assert!(budget.peak_bytes() <= crate::visual_rows::MAX_SESSION_BYTES);
}

#[test]
fn deep_locate_and_backward_context_resume_without_restarting() {
    let key = key(2, 2);
    let text = "🇦🇧🇨🇩🇪🇫a\u{301}bcdefghijklmnop";
    let reference = Bytes::new(key, text.as_bytes(), 1);
    let anchor =
        TopAnchor { source_byte: text.len() as u64 - 3, affinity: CaretAffinity::Upstream, intra_row_offset: 0 };
    let mut engine = LayoutEngine::with_limits(AllocationBudget::default(), 1, 2).unwrap();
    let expected = engine.scroll(key, &reference, anchor, -3, &NoCancellation).unwrap();
    let task = begin(
        Bytes::new(key, text.as_bytes(), 1),
        TaskRequest::Scroll { key, anchor, delta: -3 },
        AllocationBudget::default(),
        1,
    );
    let (_, _, _, output) = run(task, ReadWork::new(1, 1));
    let actual = page(output);
    assert_eq!(actual.rows(), expected.rows());
    for row in 0..actual.rows().len() {
        assert_eq!(actual.atoms(row), expected.atoms(row));
    }
}

#[test]
fn complete_terminator_metadata_preserves_literal_cr_at_eof() {
    let key = key(20, 4);
    let task = begin(
        Bytes::new(key, b"A\r\nB\r", 1),
        TaskRequest::Fill { key, anchor: TopAnchor::default() },
        AllocationBudget::default(),
        1,
    );
    let (_, _, _, output) = run(task, ReadWork::new(1, 1));
    let actual = page(output);
    assert_eq!(ranges(&actual), vec![0..1, 3..5]);
    assert_eq!(actual.atoms(1).last().unwrap().text(), "\\x0D");
    assert_eq!(actual.atoms(1).last().unwrap().source, 4..5);
    assert!(actual.at_end);
}

#[test]
fn malformed_opaque_provenance_is_neutral_and_has_no_syntax_window() {
    let key = key(10, 2);
    let mut source = Bytes::new(key, b"a\xff\xfez", 1);
    source.opaque.push(1..3);
    let task = begin(source, TaskRequest::Fill { key, anchor: TopAnchor::default() }, AllocationBudget::default(), 1);
    let (_, _, _, output) = run(task, ReadWork::new(1, 1));
    let actual = page(output);
    let opaque = actual.atoms(0).iter().find(|atom| atom.kind == AtomKind::Opaque).unwrap();
    assert_eq!(opaque.source, 1..3);
    assert_eq!(opaque.text(), "?");
    assert!(!actual.syntax_windows().iter().any(|range| range.start < 3 && range.end > 1));
    assert!(actual.check_syntax_request(1..3, 0).is_err());
}

#[test]
fn full_ordered_navigation_matches_affinity_goals_and_primary() {
    let key = key(4, 3);
    let text = b"one two three\nA\tB\nlast row\n";
    let cursors = [
        VisualCursor { head: 2, anchor: 2, ..VisualCursor::default() },
        VisualCursor { head: 15, anchor: 14, desired_cell: Some(3), ..VisualCursor::default() },
    ];
    let fence = NavigationFence { key, cursor_set_generation: 31, intent_generation: 19 };
    for motion in [VisualMotion::Up, VisualMotion::Down, VisualMotion::PageUp, VisualMotion::PageDown] {
        for extend in [false, true] {
            let command = NavigationCommand { motion, count: 2, extend };
            let reference = Bytes::new(key, text, 1);
            let mut engine = LayoutEngine::with_chunk_limit(AllocationBudget::default(), 1).unwrap();
            let expected = CursorSet::new(engine.budget(), &cursors, 1).unwrap();
            let reference = engine.navigate(fence, &reference, expected, command, &NoCancellation).unwrap();
            let budget = AllocationBudget::default();
            let expected = CursorSet::new(&budget, &cursors, 1).unwrap();
            let task = begin(Bytes::new(key, text, 1), TaskRequest::Navigate { fence, expected, command }, budget, 1);
            let (_, _, request, output) = run(task, ReadWork::new(1, 1));
            assert!(request.is_none());
            let ControlFlow::Continue(actual) = output.into_navigation() else { panic!("navigation output") };
            assert_eq!(actual.fence, reference.fence);
            assert_eq!(actual.expected.cursors(), reference.expected.cursors());
            assert_eq!(actual.targets.cursors(), reference.targets.cursors());
            assert_eq!(actual.targets.primary, reference.targets.primary);
            assert_eq!(actual.reveal.rows(), reference.reveal.rows());
            assert_eq!(actual.reveal.anchor, reference.reveal.anchor);
        }
    }
}

#[test]
fn cancellation_and_source_change_return_exact_task_source_and_request() {
    eprintln!(
        "STACK_ENVELOPE {:?}; lease_read_query={}; largest_inline_byte_payload={}; reader_payload_heap_bytes={}",
        VisualTask::<Bytes>::stack_envelope(),
        size_of::<LeaseReadQuery>(),
        DISPLAY_BYTES,
        2 * SOURCE_WINDOW
    );
    for change in [false, true] {
        let key = key(1, 1);
        let source = Bytes::new(key, b"abcdefgh", 1);
        let owner = source.owner.clone();
        let budget = AllocationBudget::default();
        let mut task = begin(source, TaskRequest::Scroll { key, anchor: TopAnchor::default(), delta: 10 }, budget, 1);
        match task.step(ReadWork::new(1, 1), &AtomicBool::new(false)) {
            TaskStep::Pending => {}
            _ => panic!("tiny quantum must pause"),
        };
        let before = owner.reads.load(Ordering::Relaxed);
        if change {
            owner.revision.store(1, Ordering::Relaxed);
        }
        match task.step(ReadWork::new(100, 100), &AtomicBool::new(!change)) {
            TaskStep::Cancelled if !change => {}
            TaskStep::Failed(TaskError::Layout(WrapError::Stale)) if change => {}
            _ => panic!("cancel/stale must retain its task"),
        };
        assert_eq!(owner.reads.load(Ordering::Relaxed), before);
        let (_, source, request, output) = task.into_parts();
        assert!(Arc::ptr_eq(&source.owner, &owner));
        assert!(matches!(request, Some(TaskRequest::Scroll { delta: 10, .. })));
        assert!(output.is_none());
    }
}

#[test]
fn refusal_and_continuation_overlap_use_the_same_aggregate() {
    let key = key(2, 2);
    let budget = AllocationBudget::default();
    let baseline = budget.retained_bytes();
    let context = match TaskContext::try_new(Bytes::new(key, b"abcd", 1), &budget, 1) {
        Ok(context) => context,
        Err(error) => panic!("{}", error.error),
    };
    assert!(budget.retained_bytes() >= baseline + 3 * size_of_val(&context));
    let during = budget.retained_bytes();
    let second = match TaskContext::try_new(Bytes::new(key, b"efgh", 1), &budget, 1) {
        Ok(context) => context,
        Err(error) => panic!("{}", error.error),
    };
    assert!(budget.retained_bytes() >= during + 3 * size_of_val(&second));
    drop((context.into_source(), second.into_source()));
    assert_eq!(budget.retained_bytes(), baseline);
    let small = AllocationBudget::new(4096);
    let before = small.retained_bytes();
    let source = Bytes::new(key, b"x", 1);
    let owner = source.owner.clone();
    match TaskContext::try_new(source, &small, 1) {
        Err(Refused { source, error: WrapError::Admission(_) }) => assert!(Arc::ptr_eq(&source.owner, &owner)),
        _ => panic!("must refuse before large inline state is constructed"),
    }
    assert_eq!(small.retained_bytes(), before);
}

#[test]
fn pending_read_and_failed_query_leave_output_untouched() {
    let key = key(2, 2);
    let source = Bytes::new(key, b"abcd", 1);
    let budget = AllocationBudget::default();
    let mut query = source.new_read_query(0, 4, &budget).unwrap();
    let mut output = [77; 4];
    assert!(matches!(
        source.poll_read(&mut query, &mut output, &mut ReadWork::new(1, 1), &AtomicBool::new(false)),
        TaskPoll::Pending
    ));
    assert_eq!(output, [77; 4]);
    assert_eq!(source.owner.reads.load(Ordering::Relaxed), 0);
    let mut bad = source.new_read_query(100, 4, &budget).unwrap();
    assert!(matches!(
        source.poll_read(&mut bad, &mut output, &mut ReadWork::new(8, 1), &AtomicBool::new(false)),
        TaskPoll::Failed(_)
    ));
    assert_eq!(output, [77; 4]);
}

// ROOT's Snapshot fixture at CSV 969F86BA... exhausts its bound at assert!(done)
// because its UTF-8 scalars cross one-byte pieces. The former Scalar phase
// repeated lead provenance after tail provenance had completed, evicting the
// latter before its pending byte read could advance. This isolates that state
// transition without any Snapshot/index implementation or larger deadline.
#[test]
fn scalar_keeps_completed_lead_provenance_while_tail_read_is_pending() {
    let key = key(4, 1);
    let mut source = Bytes::new(key, "界".as_bytes(), 1);
    source.unit_provenance = true;
    let owner = source.owner.clone();
    let budget = AllocationBudget::default();
    let mut context = TaskContext::try_new(source, &budget, 1).unwrap_or_else(|error| panic!("{}", error.error));
    let mut stream = GraphemeContinuation::new(0, 3).unwrap();
    let mut actual = None;
    for _ in 0..20_000 {
        match stream.poll(&mut context.reader, &context.source, &mut ReadWork::new(1, 1), &AtomicBool::new(false)) {
            TaskPoll::Pending => {}
            TaskPoll::Ready(cluster) => {
                actual = cluster;
                break;
            }
            TaskPoll::Cancelled => panic!("unexpected cancellation"),
            TaskPoll::Failed(error) => panic!("{error:?}"),
        }
    }
    assert_eq!(actual, Some(Cluster { source: 0..3, valid: true, opaque: false }));
    assert_eq!(owner.provenance.load(Ordering::Relaxed), 3);
    assert_eq!(owner.reads.load(Ordering::Relaxed), 3);
    assert_eq!(context.reader.read_bytes(), 3);
    assert!(matches!(
        stream.poll(&mut context.reader, &context.source, &mut ReadWork::new(1, 1), &AtomicBool::new(false)),
        TaskPoll::Ready(None)
    ));
    assert_eq!(owner.provenance.load(Ordering::Relaxed), 3);
}

// The fence fixture's store, read leases and staged query all use the SAME
// AllocationBudget. Fixed charge slots keep the tiny fixture accounting itself
// bounded; source retirement is explicit even though the fixture is short.
#[derive(Clone)]
struct FenceAdmission(Arc<std::sync::Mutex<FenceLedger>>);
struct FenceLedger {
    budget: AllocationBudget,
    usage: kwil_core_text::compare_input::Usage,
    charges: [Option<crate::visual_rows::AllocationCharge>; 16],
    _header: MetadataReservation,
}
impl kwil_core_text::compare_input::Admission for FenceAdmission {
    fn reserve(
        &mut self,
        additional: kwil_core_text::compare_input::Usage,
    ) -> Result<(), kwil_core_text::compare_input::AdmissionError> {
        use kwil_core_text::compare_input::{AdmissionError, Usage};
        let mut ledger = self.0.lock().unwrap();
        let next = Usage {
            resident: ledger.usage.resident.checked_add(additional.resident).ok_or(AdmissionError)?,
            disk: ledger.usage.disk.checked_add(additional.disk).ok_or(AdmissionError)?,
            handles: ledger.usage.handles.checked_add(additional.handles).ok_or(AdmissionError)?,
        };
        if additional.resident != 0 {
            let slot = ledger.charges.iter().position(Option::is_none).ok_or(AdmissionError)?;
            ledger.charges[slot] = Some(ledger.budget.reserve(additional.resident).map_err(|_| AdmissionError)?);
        }
        ledger.usage = next;
        Ok(())
    }
    fn release(&mut self, released: kwil_core_text::compare_input::Usage) {
        let mut ledger = self.0.lock().unwrap();
        ledger.usage.resident -= released.resident;
        ledger.usage.disk -= released.disk;
        ledger.usage.handles -= released.handles;
        let mut remaining = released.resident;
        for slot in ledger.charges.iter_mut().rev() {
            if remaining == 0 {
                break;
            }
            if let Some(charge) = slot {
                let bytes = charge.charged_bytes();
                if bytes <= remaining {
                    remaining -= bytes;
                    *slot = None;
                } else {
                    charge.reduce_to(bytes - remaining);
                    remaining = 0;
                }
            }
        }
        assert_eq!(remaining, 0);
    }
}
struct FenceText {
    done: bool,
}
impl kwil_core_text::compare_input::SourceLease for FenceText {
    fn identity(&self) -> kwil_core_text::compare_input::InputIdentity {
        kwil_core_text::compare_input::InputIdentity([21, 28, 3, 7])
    }
    fn validate(&mut self, work: &mut ReadWork, cancel: &AtomicBool) -> Step<()> {
        work.debit(0, 1, cancel)
    }
    fn capture(
        &mut self,
        logical: &mut [u8],
        _: &mut [u8],
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<Option<kwil_core_text::compare_input::Captured>> {
        match work.debit(u64::from(!self.done), 1, cancel) {
            Step::Ready(()) => {}
            other => {
                return match other {
                    Step::Pending => Step::Pending,
                    Step::Cancelled => Step::Cancelled,
                    Step::Failed(error) => Step::Failed(error),
                    Step::Ready(()) => unreachable!(),
                };
            }
        }
        if self.done {
            return Step::Ready(None);
        }
        logical[0] = b'a';
        self.done = true;
        Step::Ready(Some(kwil_core_text::compare_input::Captured {
            logical: 1,
            original: 0,
            kind: kwil_core_text::compare_input::CaptureKind::Text,
            encoding: 0,
        }))
    }
}
#[test]
fn staged_lease_read_rejects_an_independent_equal_identity_owner_without_new_source_slots() {
    use crate::compare_source::{CompareSourceStep, begin_compare_source};
    use kwil_core_text::compare_input::{
        BuildStep, ReadLeaseRetirementStep, RetirementStep, StoreAdmission, StorePaths, Usage, begin,
    };
    let budget = AllocationBudget::default();
    let baseline = budget.retained_bytes();
    let header = budget.reserve_metadata(3 * size_of::<FenceLedger>() + 2 * size_of::<usize>()).unwrap();
    let admission = FenceAdmission(Arc::new(std::sync::Mutex::new(FenceLedger {
        budget: budget.clone(),
        usage: Usage::default(),
        charges: std::array::from_fn(|_| None),
        _header: header,
    })));
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let directory = std::env::temp_dir().join(format!(
        "kwil-read-fence-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let paths = StorePaths {
        logical: directory.join("logical"),
        original: directory.join("original"),
        lines: directory.join("lines"),
        atoms: directory.join("atoms"),
        directory,
    };
    let work = || ReadWork::new(1024 * 1024, 4096);
    let mut build = match begin(FenceText { done: false }, StoreAdmission { aggregate: admission.clone(), paths }) {
        std::ops::ControlFlow::Continue(build) => build,
        _ => panic!("fixture begin"),
    };
    let mut store = None;
    for _ in 0..32 {
        match build.step(work(), &AtomicBool::new(false)) {
            BuildStep::Pending(next) => build = next,
            BuildStep::Ready { store: ready, .. } => {
                store = Some(ready);
                break;
            }
            _ => panic!("fixture build"),
        }
    }
    let store = store.expect("bounded fixture store");
    let (source_a, source_b) = {
        let source = || match begin_compare_source(&store, admission.clone(), &mut work(), &AtomicBool::new(false)) {
            CompareSourceStep::Ready(source) => source,
            _ => panic!("fixture lease"),
        };
        (source(), source())
    };
    assert_eq!(source_a.metadata(), source_b.metadata());
    assert_ne!(source_a.read_owner_id(), source_b.read_owner_id());
    assert_eq!(source_a.owner_count(), 1);
    assert_eq!(source_b.owner_count(), 1);
    let mut query = source_a.new_read_query(0, 1, &budget).unwrap();
    let mut output = [77];
    // The two byte/one operation envelope funds exactly the one-byte OS read,
    // leaving its caller transfer pending and the source data safely staged.
    assert!(matches!(
        source_a.poll_read(&mut query, &mut output, &mut ReadWork::new(2, 1), &AtomicBool::new(false)),
        TaskPoll::Pending
    ));
    assert_eq!(query.returned, Some(1));
    assert_eq!(output, [77]);
    let mut transfer = ReadWork::new(1, 1);
    let before = transfer;
    assert!(matches!(
        source_b.poll_read(&mut query, &mut output, &mut transfer, &AtomicBool::new(false)),
        TaskPoll::Failed(TaskError::Source(InputError::QueryChanged))
    ));
    assert_eq!(transfer, before);
    assert_eq!(output, [77]);
    assert!(matches!(
        source_a.poll_read(&mut query, &mut output, &mut transfer, &AtomicBool::new(true)),
        TaskPoll::Cancelled
    ));
    assert_eq!(output, [77]);
    let moved_a = source_a;
    assert!(matches!(
        moved_a.poll_read(&mut query, &mut output, &mut transfer, &AtomicBool::new(false)),
        TaskPoll::Ready(1)
    ));
    assert_eq!(output, *b"a");
    let mut fresh = moved_a.new_read_query(0, 1, &budget).unwrap();
    output = [77];
    assert!(matches!(
        source_b.poll_read(&mut fresh, &mut output, &mut work(), &AtomicBool::new(false)),
        TaskPoll::Failed(TaskError::Source(InputError::QueryChanged))
    ));
    assert_eq!(output, [77]);
    assert_eq!(moved_a.owner_count(), 1);
    assert_eq!(source_b.owner_count(), 1);
    // The binding names Shared ownership rather than an individual slot or
    // stack address: an explicit same-owner clone can resume and publish A.
    let same_owner = moved_a.try_clone().unwrap_or_else(|_| panic!("one explicit fixture clone"));
    assert_eq!(same_owner.read_owner_id(), moved_a.read_owner_id());
    assert_eq!(moved_a.owner_count(), 2);
    assert!(matches!(
        moved_a.poll_read(&mut fresh, &mut output, &mut ReadWork::new(2, 1), &AtomicBool::new(false)),
        TaskPoll::Pending
    ));
    assert_eq!(fresh.returned, Some(1));
    assert_eq!(output, [77]);
    assert!(matches!(
        same_owner.poll_read(&mut fresh, &mut output, &mut ReadWork::new(1, 1), &AtomicBool::new(false)),
        TaskPoll::Ready(1)
    ));
    assert_eq!(output, *b"a");
    drop(same_owner);
    assert_eq!(moved_a.owner_count(), 1);
    assert_eq!(source_b.owner_count(), 1);
    drop((fresh, query));
    for owner in [moved_a, source_b] {
        let retirement =
            owner.try_into_retirement().unwrap_or_else(|_| panic!("query must not retain another source slot"));
        assert!(matches!(retirement.step(work(), &AtomicBool::new(false)), ReadLeaseRetirementStep::Ready(_)));
    }
    assert!(matches!(store.into_retirement().step(work(), &AtomicBool::new(false)), RetirementStep::Ready(_)));
    assert_eq!(admission.0.lock().unwrap().usage, Usage::default());
    drop(admission);
    assert_eq!(budget.retained_bytes(), baseline);
}

#[test]
fn output_wrong_tag_returns_complete_page_and_navigation_ownership_with_charge() {
    let key = key(4, 2);
    let budget = AllocationBudget::default();
    let source = Bytes::new(key, b"abcd\nefgh", 1);
    let task = begin(source, TaskRequest::Fill { key, anchor: TopAnchor::default() }, budget.clone(), 1);
    let (_, _, _, output) = run(task, ReadWork::new(1, 1));
    let before = budget.retained_bytes();
    let carrier_charge = output._header.reserved_bytes();
    let row_storage = output.page.rows.data.as_ptr();
    let ControlFlow::Break(output) = output.into_navigation() else { panic!("wrong page tag must retain carrier") };
    assert_eq!(budget.retained_bytes(), before);
    assert_eq!(output.page.rows.data.as_ptr(), row_storage);
    let ControlFlow::Continue(page) = output.into_page() else { panic!("retained page") };
    assert_eq!(page.key, key);
    assert_eq!(page.rows.data.as_ptr(), row_storage);
    assert_eq!(budget.retained_bytes(), before - carrier_charge);
    drop(page);

    let cursor = VisualCursor { anchor: 2, head: 2, ..VisualCursor::default() };
    let expected = CursorSet::new(&budget, &[cursor], 0).unwrap();
    let fence = NavigationFence { key, cursor_set_generation: 23, intent_generation: 29 };
    let command = NavigationCommand { motion: VisualMotion::Down, count: 1, extend: false };
    let task =
        begin(Bytes::new(key, b"abcd\nefgh", 1), TaskRequest::Navigate { fence, expected, command }, budget.clone(), 1);
    let (_, _, _, output) = run(task, ReadWork::new(1, 1));
    let before = budget.retained_bytes();
    let carrier_charge = output._header.reserved_bytes();
    let row_storage = output.page.rows.data.as_ptr();
    let ControlFlow::Break(output) = output.into_page() else { panic!("wrong navigation tag must retain carrier") };
    assert_eq!(budget.retained_bytes(), before);
    assert_eq!(output.page.rows.data.as_ptr(), row_storage);
    assert_eq!(output.navigation.as_ref().unwrap().expected.cursors(), &[cursor]);
    let ControlFlow::Continue(navigation) = output.into_navigation() else { panic!("retained navigation") };
    assert_eq!(navigation.fence, fence);
    assert_eq!(navigation.expected.cursors(), &[cursor]);
    assert_eq!(navigation.targets.cursors()[0].head, 7);
    assert_eq!(navigation.reveal.rows.data.as_ptr(), row_storage);
    assert_eq!(budget.retained_bytes(), before - carrier_charge);
}

#[test]
fn control_flow_refusal_returns_exact_source_engine_and_request_without_allocation() {
    let key = key(4, 2);
    let budget = AllocationBudget::default();
    let baseline = budget.retained_bytes();
    let engine = LayoutEngine::with_chunk_limit(budget.clone(), 1).unwrap();
    let source = Bytes::new(key, b"exact", 1);
    let owner = source.owner.clone();
    let mut stale = key;
    stale.source.revision += 1;
    let anchor = TopAnchor { source_byte: 2, ..TopAnchor::default() };
    let request = TaskRequest::Fill { key: stale, anchor };
    let before = budget.retained_bytes();
    let allocations = budget.allocation_attempts();
    let ControlFlow::Break(refusal) = engine.begin_task(source, request) else { panic!("stale source refuses") };
    assert!(matches!(refusal.error, WrapError::Stale));
    assert!(Arc::ptr_eq(&refusal.source.owner, &owner));
    assert_eq!(refusal.source.text, b"exact");
    assert!(same_budget(refusal.engine.budget(), &budget));
    assert!(matches!(refusal.request, TaskRequest::Fill { key, anchor: actual } if key == stale && actual == anchor));
    assert_eq!(budget.retained_bytes(), before);
    assert_eq!(budget.allocation_attempts(), allocations);
    drop(refusal);
    assert_eq!(budget.retained_bytes(), baseline);
}
