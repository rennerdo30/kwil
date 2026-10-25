//! Authored source-only controls; ROOT runs these after the integration freeze.
//! Every publication below comes from the real WrapSession and Editor path.
//! Tiny immutable in-memory fixtures require no profile, registry or disk data.
use super::*;
use crate::{Cursor, Document, editor::visual::PrepareFault};
use std::{
    io,
    sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread::{self, ThreadId},
    time::Instant,
};

trait ContinueTest<B, C> {
    fn continued(self) -> C;
}
impl<B: fmt::Debug, C> ContinueTest<B, C> for ControlFlow<B, C> {
    fn continued(self) -> C {
        match self {
            ControlFlow::Continue(value) => value,
            ControlFlow::Break(error) => panic!("unexpected exact-owner refusal: {error:?}"),
        }
    }
}

struct ReadGate {
    entered: mpsc::SyncSender<()>,
    released: Mutex<bool>,
    changed: Condvar,
}
impl ReadGate {
    fn wait(&self) {
        let _ = self.entered.try_send(());
        let released = self.released.lock().unwrap();
        let (released, _) =
            self.changed.wait_timeout_while(released, Duration::from_secs(2), |released| !*released).unwrap();
        assert!(*released, "bounded source gate timed out");
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.changed.notify_all();
    }
}

struct CheckedSource {
    inner: SnapshotSource,
    ui: ThreadId,
    worker: Mutex<Option<ThreadId>>,
    reads: AtomicUsize,
    largest: AtomicUsize,
    gate: Mutex<Option<Arc<ReadGate>>>,
    panic_next: AtomicBool,
}
impl CheckedSource {
    fn worker_only(&self) {
        let current = thread::current().id();
        assert_ne!(current, self.ui, "host performed source I/O on UI");
        let mut worker = self.worker.lock().unwrap();
        if let Some(previous) = *worker {
            assert_eq!(current, previous, "host launched a worker per request");
        } else {
            *worker = Some(current);
        }
    }
    fn block_next(&self) -> (Arc<ReadGate>, mpsc::Receiver<()>) {
        let (entered, receiver) = mpsc::sync_channel(1);
        let gate = Arc::new(ReadGate { entered, released: Mutex::new(false), changed: Condvar::new() });
        *self.gate.lock().unwrap() = Some(gate.clone());
        (gate, receiver)
    }
}
impl WrapSource for CheckedSource {
    fn run_layout(
        self: Arc<Self>,
        engine: crate::visual_rows::LayoutEngine,
        request: crate::visual_rows::task::TaskRequest,
        driver: &mut dyn crate::visual_rows::TaskDriver,
    ) -> crate::visual_rows::WorkerExit {
        crate::visual_rows::fixtures::run_snapshot(self, None, engine, request, driver)
    }
    fn identity(&self) -> SourceIdentity {
        self.inner.identity()
    }
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn line_count(&self) -> u64 {
        self.inner.line_count()
    }
    fn line_start(&self, line: u64) -> io::Result<Option<u64>> {
        self.worker_only();
        self.inner.line_start(line)
    }
    fn line_of(&self, byte: u64) -> io::Result<u64> {
        self.worker_only();
        self.inner.line_of(byte)
    }
    fn read(&self, at: u64, output: &mut [u8]) -> io::Result<usize> {
        crate::visual_rows::fixtures::SnapshotHooks::before_snapshot_read(self, output.len())?;
        self.inner.read(at, output)
    }
}
impl crate::snapshot_layout_source::SnapshotOwner for CheckedSource {
    fn snapshot(&self) -> &kwil_core_text::Snapshot {
        crate::snapshot_layout_source::SnapshotOwner::snapshot(&self.inner)
    }
}
impl crate::visual_rows::fixtures::SnapshotHooks for CheckedSource {
    fn before_snapshot_query(&self) {
        self.worker_only();
    }
    fn before_snapshot_read(&self, length: usize) -> io::Result<()> {
        self.worker_only();
        assert!(!self.panic_next.swap(false, Ordering::AcqRel), "injected terminal read panic");
        let gate = self.gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.wait();
        }
        self.reads.fetch_add(1, Ordering::AcqRel);
        self.largest.fetch_max(length, Ordering::AcqRel);
        Ok(())
    }
}

struct Fixture {
    host: WrapHost,
    editor: Editor,
    source: Arc<CheckedSource>,
    context: WrapHostContext,
    wakes: mpsc::Receiver<()>,
}
impl Fixture {
    fn new(text: &str, columns: u16, rows: u16, caret: u64) -> Self {
        let mut document = Document::new();
        document.buffer.insert(0, text.as_bytes()).unwrap();
        let mut editor = Editor::new(document);
        editor.set_cursors(vec![Cursor::caret(caret)], 0).unwrap();
        let scalar = editor.document().hex_source(7);
        let identity =
            SourceIdentity { document_id: 7, source_epoch: scalar.epoch, incarnation: 11, revision: scalar.revision };
        let context = WrapHostContext {
            enabled: true,
            path_generation: 13,
            cursor_generation: 17,
            key: LayoutKey {
                source: identity,
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
            },
        };
        let source = Arc::new(CheckedSource {
            inner: SnapshotSource::new(identity, editor.document().snapshot()),
            ui: thread::current().id(),
            worker: Mutex::new(None),
            reads: AtomicUsize::new(0),
            largest: AtomicUsize::new(0),
            gate: Mutex::new(None),
            panic_next: AtomicBool::new(false),
        });
        let (wake, wakes) = mpsc::sync_channel(8);
        let mut host = WrapHost::new(move || {
            let _ = wake.try_send(());
        })
        .unwrap();
        let sync = host.sync(Some(&editor), context).unwrap();
        assert!(sync.needs_source);
        Self { host, editor, source, context, wakes }
    }
    fn drive(&mut self) {
        if !self.host.needs_source() {
            return;
        }
        let source: Arc<dyn WrapSource> = self.source.clone();
        if let ControlFlow::Break(refusal) = self.host.drive(source.clone()) {
            assert!(Arc::ptr_eq(refusal.owner.source(), &source), "refused a different source owner");
            assert!(matches!(refusal.error, WrapError::Busy), "unexpected source refusal: {:?}", refusal.error);
            // Fixture retains the original immutable source. Rejected temporary
            // requests have no final-source destruction or I/O on this thread.
            drop(refusal);
        }
    }
    /// Bounded event-driven wait, never a timer/poll/sleep or an idle-work loop.
    fn update(&mut self, fault: PrepareFault) -> WrapHostUpdate {
        let deadline = Instant::now() + Duration::from_secs(2);
        self.drive();
        for _ in 0..64 {
            let update = self.host.drain_with_fault(Some(&mut self.editor), fault).continued();
            if update.publication.is_some() || update.failure.is_some() {
                return update;
            }
            self.drive();
            self.wakes
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("missing bounded worker wake");
        }
        panic!("unbounded worker wake count");
    }
    fn publish(&mut self) -> WrapHostPublication {
        let update = self.update(PrepareFault::None);
        assert!(update.failure.is_none(), "unexpected worker/precommit failure: {:?}", update.failure);
        update.publication.expect("publication")
    }
    fn sync(&mut self) -> WrapHostSync {
        let deadline = Instant::now() + Duration::from_secs(2);
        for _ in 0..64 {
            match self.host.sync(Some(&self.editor), self.context) {
                Ok(sync) => return sync,
                Err(WrapError::Busy) => {
                    self.wakes.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
                }
                Err(error) => panic!("unexpected sync failure: {error:?}"),
            }
        }
        panic!("unbounded sync retirement wakes");
    }
    fn finish(self) {
        let ticket = self.host.stop();
        // The simulated UI thread never settles document/display owners.
        thread::spawn(move || ticket.settle(Duration::from_secs(2))).join().unwrap().unwrap();
    }
}

#[test]
fn wrap_host_relative_fifo_uses_each_published_cursor_and_retains_full_newest() {
    let mut fixture = Fixture::new("abcdefghijklmnopqrstuvwx", 4, 2, 1);
    fixture.publish();
    let first = fixture.host.navigate(VisualMotion::Down, 1, false).unwrap();
    let second = fixture.host.navigate(VisualMotion::Down, 1, false).unwrap();
    assert_eq!(fixture.host.mutation_gate(), MutationGate::RefusedPending);
    assert_eq!(fixture.publish().relative_id, Some(first));
    assert_eq!(fixture.editor.primary_cursor().head, 5);
    assert_eq!(fixture.host.pending_inputs(), 1);
    assert_eq!(fixture.publish().relative_id, Some(second));
    assert_eq!(fixture.editor.primary_cursor().head, 9, "second head resolved against actual published byte 5");
    assert_eq!(fixture.host.mutation_gate(), MutationGate::Allowed);
    for _ in 0..RELATIVE_CAPACITY {
        fixture.host.scroll(1).unwrap();
    }
    assert!(matches!(fixture.host.scroll(1), Err(WrapHostInputError::Queue(QueueError::Full))));
    assert_eq!(fixture.host.pending_inputs(), RELATIVE_CAPACITY);
    assert_eq!(fixture.host.cancel(CancelReason::Escape).records, RELATIVE_CAPACITY);
    fixture.finish();
}

#[test]
fn wrap_host_scroll_within_one_line_copies_and_short_eof_clears_blank_rows() {
    let mut fixture = Fixture::new("abcdefghijkl", 4, 4, 0);
    fixture.publish();
    let cursor = fixture.editor.primary_cursor().clone();
    fixture.host.scroll(1).unwrap();
    let publication = fixture.publish();
    assert_eq!(publication.damage.scroll, 1);
    assert!(!publication.damage.full);
    assert_eq!(fixture.host.page().unwrap().rows().len(), 2);
    assert!(publication.damage.changed_rows().any(|row| row == 3), "old trailing pixels must be cleared");
    assert_eq!(*fixture.editor.primary_cursor(), cursor, "scroll must not move cursor");
    fixture.finish();
}

#[test]
fn wrap_host_reflow_cancels_page_motion_and_keeps_source_anchor_not_old_height() {
    let mut fixture = Fixture::new("abcdefghijklmnopqrstuvwx", 4, 2, 0);
    fixture.publish();
    fixture.host.scroll(1).unwrap();
    fixture.publish();
    let old_anchor = fixture.host.page().unwrap().anchor.source_byte;
    assert_eq!(old_anchor, 4);
    fixture.host.navigate(VisualMotion::PageDown, 1, false).unwrap();
    fixture.context.key.geometry.columns = 3;
    fixture.context.key.geometry.rows = 3;
    let sync = fixture.sync();
    assert_eq!(sync.cancelled.unwrap().records, 1);
    assert_eq!(fixture.host.pending_inputs(), 0);
    let publication = fixture.publish();
    assert!(publication.damage.full);
    let page = fixture.host.page().unwrap();
    assert_eq!(page.key.geometry.rows, 3);
    assert!(page.rows()[0].source_coverage.contains(&old_anchor));
    assert_eq!(fixture.editor.primary_cursor().head, 0, "old page motion was not reinterpreted at new height");
    fixture.finish();
}

#[test]
fn wrap_host_tab_path_input_revision_and_body_fences_hide_old_results() {
    for change in 0..7 {
        let mut fixture = Fixture::new("abcdefgh", 4, 2, 0);
        let (gate, entered) = fixture.source.block_next();
        fixture.drive();
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        fixture.host.navigate(VisualMotion::Down, 1, false).unwrap();
        match change {
            0 => fixture.context.key.source.document_id += 1,
            1 => fixture.context.path_generation += 1,
            2 => fixture.context.key.input_epoch += 1,
            3 => fixture.context.key.source.incarnation += 1,
            4 => fixture.context.key.source.revision += 1,
            5 => fixture.context.cursor_generation += 1,
            _ => fixture.context.key.body = BodyOwner::MarkdownPreview,
        }
        let result = fixture.host.sync(Some(&fixture.editor), fixture.context);
        if change == 4 {
            assert!(matches!(result, Err(WrapError::Stale)));
        } else {
            assert_eq!(result.unwrap().cancelled.unwrap().records, 1);
        }
        assert!(fixture.host.page().is_none());
        gate.release();
        let update = fixture.host.drain(Some(&mut fixture.editor)).continued();
        assert!(update.publication.is_none());
        assert_eq!(fixture.editor.primary_cursor().head, 0);
        fixture.finish();
    }
}

#[test]
fn wrap_host_editor_loan_raw_markdown_log_and_zero_geometry_exclude_wrap() {
    let mut fixture = Fixture::new("abcdefgh", 4, 2, 0);
    fixture.publish();
    fixture.host.navigate(VisualMotion::Down, 1, false).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut loan = None;
    for _ in 0..64 {
        match fixture.host.sync(None, fixture.context) {
            Ok(sync) => {
                loan = Some(sync);
                break;
            }
            Err(WrapError::Busy) => {
                fixture.wakes.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
            }
            Err(error) => panic!("unexpected loan sync failure: {error:?}"),
        }
    }
    let sync = loan.expect("bounded loan retirement");
    assert_eq!(sync.availability, WrapHostAvailability::EditorUnavailable);
    assert_eq!(sync.cancelled.unwrap().records, 1);
    assert!(fixture.host.page().is_none());
    assert!(fixture.host.visual_cursors().is_none());
    for body in [BodyOwner::Raw, BodyOwner::MarkdownPreview, BodyOwner::LogProjection, BodyOwner::EditorLoan] {
        fixture.context.key.body = body;
        // No source is submitted, so no disk/byte access can be induced here.
        let sync = fixture.sync();
        assert_eq!(sync.availability, WrapHostAvailability::BodyOwned(body));
        assert!(!sync.needs_source);
        assert!(fixture.host.page().is_none());
    }
    fixture.context.key.body = BodyOwner::Text;
    fixture.context.key.geometry.columns = 0;
    assert_eq!(fixture.sync().availability, WrapHostAvailability::EmptyGeometry);
    assert!(!fixture.host.needs_source());
    fixture.finish();
}

#[test]
fn wrap_host_mutation_busy_escape_and_failed_prepare_keep_actual_editor() {
    let mut fixture = Fixture::new("abcdefghijkl", 4, 2, 1);
    fixture.publish();
    let cursors = fixture.editor.cursors().to_vec();
    let top = fixture.editor.top();
    let revision = fixture.editor.document().revision();
    let history = fixture.editor.document().buffer.history_depth();
    fixture.host.navigate(VisualMotion::Down, 1, true).unwrap();
    assert_eq!(fixture.host.mutation_gate(), MutationGate::RefusedPending);
    assert!(matches!(fixture.host.hit(0, 0, false), Err(WrapError::Busy)));
    let failed = fixture.update(PrepareFault::Damage);
    assert!(matches!(failed.failure, Some(WrapError::Admission(_))));
    assert_eq!(fixture.host.pending_inputs(), 1);
    assert_eq!(fixture.host.mutation_gate(), MutationGate::RefusedFailed);
    assert_eq!(fixture.editor.cursors(), &cursors);
    assert_eq!(fixture.editor.top(), top);
    assert_eq!(fixture.editor.document().revision(), revision);
    assert_eq!(fixture.editor.document().buffer.history_depth(), history);
    fixture.host.retry_relative().unwrap();
    fixture.publish();
    assert_eq!(fixture.editor.primary_cursor().head, 5);
    assert_eq!(fixture.editor.primary_cursor().anchor, 1);
    fixture.host.navigate(VisualMotion::Down, 1, false).unwrap();
    assert_eq!(fixture.host.cancel(CancelReason::Escape).records, 1);
    assert_eq!(fixture.host.mutation_gate(), MutationGate::Allowed);
    fixture.finish();
}

#[test]
fn wrap_host_known_scalar_interior_hit_affinity_and_zero_margin_syntax() {
    let mut fixture = Fixture::new("e\u{301}abcde", 4, 2, 1);
    fixture.publish();
    let visual = fixture.host.visual_cursors().unwrap()[fixture.host.primary_index().unwrap()];
    assert_eq!(visual.head, 1, "capture keeps existing scalar cursor inside grapheme");
    assert_eq!(fixture.host.caret(visual.head, visual.head_affinity).unwrap().cell, 0);
    assert_eq!(fixture.host.hit_test(0, 0).unwrap().byte, 0, "new hit cannot invent scalar interior");
    assert!(fixture.host.selection_cells(0, 1..1).next().is_none());
    let windows = fixture.host.syntax_windows().to_vec();
    assert!(!windows.is_empty());
    for range in windows {
        fixture.host.check_syntax_request(range.clone(), 0).unwrap();
        assert!(matches!(fixture.host.check_syntax_request(range, 1), Err(WrapError::Admission(_))));
    }
    fixture.host.hit(0, 4, false).unwrap();
    assert_eq!(fixture.host.mutation_gate(), MutationGate::RefusedPending);
    fixture.publish();
    let cursor = fixture.host.visual_cursors().unwrap()[fixture.host.primary_index().unwrap()];
    assert_eq!(cursor.head_affinity, CaretAffinity::Upstream);
    assert_eq!(fixture.host.caret(cursor.head, cursor.head_affinity).unwrap().row, 0);
    assert_eq!(fixture.editor.primary_cursor().head, cursor.head);
    fixture.finish();
}

#[test]
fn wrap_host_latest_desired_replaces_fill_but_relative_lane_is_independent() {
    let mut fixture = Fixture::new("abcdefghijklmnopqrstuvwx", 4, 2, 0);
    let (gate, entered) = fixture.source.block_next();
    fixture.drive();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    fixture.host.request_reveal(SourcePoint { byte: 4, affinity: CaretAffinity::Downstream }).unwrap();
    fixture.drive();
    fixture.host.request_reveal(SourcePoint { byte: 12, affinity: CaretAffinity::Downstream }).unwrap();
    fixture.drive();
    fixture.host.navigate(VisualMotion::Down, 1, false).unwrap();
    assert_eq!(fixture.host.pending_inputs(), 1);
    gate.release();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut published = false;
    for _ in 0..64 {
        let update = fixture.host.drain(Some(&mut fixture.editor)).continued();
        assert!(update.failure.is_none());
        if let Some(publication) = update.publication {
            assert!(publication.relative_id.is_none());
            assert_eq!(fixture.host.page().unwrap().anchor.source_byte, 12);
            published = true;
            break;
        }
        fixture.wakes.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
    }
    assert!(published);
    assert_eq!(fixture.host.pending_inputs(), 1, "fill publication must not pop relative input");
    fixture.publish();
    assert_eq!(fixture.editor.primary_cursor().head, 4);
    fixture.finish();
}

#[test]
fn wrap_host_source_refusal_is_exact_and_all_source_reads_share_fixed_worker() {
    let mut fixture = Fixture::new("abcdefghijklmnop", 4, 2, 0);
    let wrong: Arc<dyn WrapSource> =
        Arc::new(SnapshotSource::new(SourceIdentity::default(), fixture.editor.document().snapshot()));
    let refusal = match fixture.host.drive(wrong.clone()) {
        ControlFlow::Break(refusal) => refusal,
        ControlFlow::Continue(()) => panic!("wrong source admitted"),
    };
    assert!(Arc::ptr_eq(refusal.owner.source(), &wrong));
    assert!(matches!(refusal.error, WrapError::Stale));
    fixture.publish();
    fixture.host.navigate(VisualMotion::Down, 1, false).unwrap();
    fixture.publish();
    fixture.host.scroll(1).unwrap();
    fixture.publish();
    assert!(fixture.source.reads.load(Ordering::Acquire) > 0);
    assert!(fixture.source.largest.load(Ordering::Acquire) <= SOURCE_WINDOW);
    let before = fixture.source.reads.load(Ordering::Acquire);
    assert!(!fixture.host.needs_source());
    assert!(fixture.host.drain(Some(&mut fixture.editor)).continued().publication.is_none());
    assert_eq!(fixture.source.reads.load(Ordering::Acquire), before, "idle drain schedules no read work");

    // Conversion of either refusal shape must move its one Arc and preserve
    // an existing request's cursor allocation, fields, and admission charge.
    let source: Arc<dyn WrapSource> =
        Arc::new(SnapshotSource::new(fixture.context.key.source, fixture.editor.document().snapshot()));
    let identity = Arc::as_ptr(&source);
    assert_eq!(Arc::strong_count(&source), 1);
    let owner = WrapHostSourceOwner::from_source(source);
    let owner = match owner.into_request() {
        ControlFlow::Break(owner) => owner,
        ControlFlow::Continue(_) => panic!("source-only refusal invented request metadata"),
    };
    assert!(std::ptr::addr_eq(identity, Arc::as_ptr(owner.source())));
    assert_eq!(Arc::strong_count(owner.source()), 1);
    let source = owner.into_source().continued();
    let expected = VisualCursor {
        anchor: 1,
        head: 2,
        anchor_affinity: CaretAffinity::Upstream,
        head_affinity: CaretAffinity::Downstream,
        desired_cell: Some(3),
    };
    // Keep this conversion-only budget independent of asynchronous retirement
    // by the real worker used above, so admission assertions have no race.
    let conversion_budget = AllocationBudget::default();
    let cursors = CursorSet::new(&conversion_budget, &[expected], 0).unwrap();
    let cursor_storage = cursors.cursors().as_ptr();
    let fence = fixture.host.display.as_deref().unwrap().fence;
    let anchor = TopAnchor { source_byte: 4, affinity: CaretAffinity::Upstream, intra_row_offset: 2 };
    let request = WrapRequest {
        fence,
        source,
        anchor,
        intent: LayoutIntent::MoveCursors { cursors, motion: VisualMotion::Down, count: 3, extend: true },
    };
    let admitted = conversion_budget.retained_bytes();
    let allocations = conversion_budget.allocation_attempts();
    let owner = WrapHostSourceOwner::from_request(request);
    let owner = match owner.into_source() {
        ControlFlow::Break(owner) => owner,
        ControlFlow::Continue(_) => panic!("request extraction discarded cursor ownership"),
    };
    let request = owner.into_request().continued();
    assert_eq!(request.fence, fence);
    assert_eq!(request.anchor, anchor);
    assert!(std::ptr::addr_eq(identity, Arc::as_ptr(&request.source)));
    assert_eq!(Arc::strong_count(&request.source), 1, "carrier cloned the admitted source");
    match &request.intent {
        LayoutIntent::MoveCursors { cursors, motion, count, extend } => {
            assert_eq!(cursors.cursors().as_ptr(), cursor_storage, "carrier rebuilt cursor allocation");
            assert_eq!(cursors.cursors(), &[expected]);
            assert_eq!(cursors.primary, 0);
            assert_eq!(*motion, VisualMotion::Down);
            assert_eq!(*count, 3);
            assert!(*extend);
        }
        _ => panic!("carrier changed the complete request intent"),
    }
    let retired = WrapHostSourceOwner::from_request(request).into_retired();
    assert_eq!(conversion_budget.retained_bytes(), admitted);
    assert_eq!(conversion_budget.allocation_attempts(), allocations);
    thread::spawn(move || drop(retired)).join().unwrap();
    fixture.finish();
}

#[test]
fn wrap_host_escape_inflight_fill_wakes_busy_new_head_and_publishes_by_events_only() {
    let mut fixture = Fixture::new("abcdefghijklmnop", 4, 2, 0);
    let (gate, entered) = fixture.source.block_next();
    fixture.drive();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(fixture.host.page().is_none(), "initial fill is still inside its source read");
    assert_eq!(fixture.host.cancel(CancelReason::Escape).records, 0);
    let id = fixture.host.navigate(VisualMotion::Down, 1, false).unwrap();
    let source: Arc<dyn WrapSource> = fixture.source.clone();
    let refused = match fixture.host.drive(source.clone()) {
        ControlFlow::Break(refused) => refused,
        ControlFlow::Continue(()) => panic!("new navigation overtook gated canceled fill"),
    };
    assert!(matches!(refused.error, WrapError::Busy));
    assert!(Arc::ptr_eq(refused.owner.source(), &source));
    assert_eq!(fixture.host.session.relative.head().unwrap().id, id);
    assert_eq!(fixture.host.pending_inputs(), 1);
    assert!(fixture.host.active.is_none());
    assert_eq!(fixture.editor.primary_cursor().head, 0);
    assert!(matches!(fixture.wakes.try_recv(), Err(mpsc::TryRecvError::Empty)));

    // Retain the complete Busy request locally. Sending it to the worker's
    // retirement queue would supply an unrelated wake and mask this defect.
    // After release, wait FIRST: no eager drive/drain, sleeps, polling, timers,
    // second input, cancellation, or replacement worker can rescue the head.
    let deadline = Instant::now() + Duration::from_secs(2);
    gate.release();
    let mut published = false;
    for _ in 0..64 {
        fixture
            .wakes
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("canceled fill cleanup did not wake the accepted Busy head");
        let update = fixture.host.drain(Some(&mut fixture.editor)).continued();
        assert!(update.failure.is_none(), "unexpected failure: {:?}", update.failure);
        if let Some(publication) = update.publication {
            assert_eq!(publication.relative_id, Some(id));
            assert!(publication.cursor_damage.is_some(), "actual Editor cursor commit must publish");
            published = true;
            break;
        }
        assert_eq!(fixture.host.session.relative.head().unwrap().id, id, "Busy changed accepted head");
        fixture.drive();
    }
    assert!(published, "bounded event wakes never published the accepted head");
    assert_eq!(fixture.editor.primary_cursor().head, 4);
    assert_eq!(fixture.host.pending_inputs(), 0);
    assert_eq!(fixture.host.mutation_gate(), MutationGate::Allowed);
    assert!(Arc::ptr_eq(refused.owner.source(), &source), "Busy owner was not retained exactly");
    let retired = refused.owner.into_retired();
    thread::spawn(move || drop(retired)).join().unwrap();
    fixture.finish();
}

#[test]
fn wrap_host_stale_ready_retires_before_two_permits_and_wakes_latest_fill() {
    let mut fixture = Fixture::new("abcdefghijklmnopqrstuvwx", 4, 2, 0);
    fixture.drive();
    // Tiny source sends no throttled progress; this wake proves old ready owns
    // the slot. Do not consume it before superseding its registered fence.
    fixture.wakes.recv_timeout(Duration::from_secs(2)).unwrap();
    fixture.host.request_reveal(SourcePoint { byte: 12, affinity: CaretAffinity::Downstream }).unwrap();
    let stale = fixture.host.drain(Some(&mut fixture.editor)).continued();
    assert!(stale.publication.is_none());
    assert!(fixture.host.page().is_none(), "old desired page must never publish");
    fixture.publish();
    assert_eq!(fixture.host.page().unwrap().anchor.source_byte, 12);
    fixture.finish();
}

#[test]
fn wrap_host_retirement_saturation_keeps_current_ready_until_worker_wake() {
    let Fixture { host, editor, source, context, .. } = Fixture::new("abcdefgh", 4, 2, 0);
    thread::spawn(move || host.stop().settle(Duration::from_secs(2))).join().unwrap().unwrap();
    let (entered, reached) = mpsc::sync_channel(1);
    let gate = Arc::new(ReadGate { entered, released: Mutex::new(false), changed: Condvar::new() });
    let callback_gate = gate.clone();
    let first = AtomicBool::new(true);
    let (wake, wakes) = mpsc::sync_channel(8);
    let mut host = WrapHost::new(move || {
        let _ = wake.try_send(());
        if first.swap(false, Ordering::AcqRel) {
            callback_gate.wait();
        }
    })
    .unwrap();
    host.sync(Some(&editor), context).unwrap();
    let mut fixture = Fixture { host, editor, source, context, wakes };
    fixture.drive();
    reached.recv_timeout(Duration::from_secs(2)).unwrap();
    // The real worker is inside its ready callback, so neither retirement slot
    // can drain before the host observes Busy. No sleeps or scheduling guess.
    for _ in 0..2 {
        let owner: Arc<dyn WrapSource> = fixture.source.clone();
        fixture.host.session.retire_owner(RetiredOwner::Source(owner)).continued();
    }
    let busy = fixture.host.drain(Some(&mut fixture.editor)).continued();
    assert!(busy.publication.is_none());
    assert!(fixture.host.page().is_none());
    assert!(fixture.host.desired.is_some_and(|desired| desired.submitted));
    gate.release();
    fixture.publish();
    assert_eq!(fixture.host.page().unwrap().rows()[0].source_coverage, 0..4);
    fixture.finish();
}

#[test]
fn wrap_host_shutdown_timeout_preserves_exact_display_and_worker_for_retry() {
    let mut fixture = Fixture::new("abcdefghijklmnop", 4, 2, 0);
    fixture.publish();
    let display = fixture.host.display.as_deref().map(|state| state as *const VisualEditorState as usize).unwrap();
    let (gate, entered) = fixture.source.block_next();
    fixture.host.request_reveal(SourcePoint { byte: 12, affinity: CaretAffinity::Downstream }).unwrap();
    fixture.drive();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    let ticket = fixture.host.stop();
    let failure = thread::spawn(move || ticket.settle(Duration::ZERO)).join().unwrap().unwrap_err();
    assert!(matches!(failure.error, WrapError::TimedOut));
    assert_eq!(
        failure.ticket.display.as_deref().map(|state| state as *const VisualEditorState as usize),
        Some(display)
    );
    assert!(failure.ticket.retained_bytes() <= MAX_SESSION_BYTES);
    gate.release();
    thread::spawn(move || failure.ticket.settle(Duration::from_secs(2))).join().unwrap().unwrap();
}

#[test]
fn wrap_host_terminal_worker_wakes_and_keeps_failed_head_without_retry_launch() {
    let mut fixture = Fixture::new("abcdefghijkl", 4, 2, 0);
    fixture.publish();
    fixture.source.panic_next.store(true, Ordering::Release);
    fixture.host.navigate(VisualMotion::Down, 1, false).unwrap();
    let failed = fixture.update(PrepareFault::None);
    assert!(matches!(failed.failure, Some(WrapError::WorkerPanicked)));
    assert_eq!(fixture.host.pending_inputs(), 1);
    assert_eq!(fixture.host.mutation_gate(), MutationGate::RefusedFailed);
    assert_eq!(fixture.host.availability(), WrapHostAvailability::Failed);
    assert!(!fixture.host.needs_source());
    assert!(matches!(fixture.host.retry_relative(), Err(WrapError::Closed)));
    assert_eq!(fixture.editor.primary_cursor().head, 0);
    let ticket = fixture.host.stop();
    let failure = thread::spawn(move || ticket.settle(Duration::from_secs(2))).join().unwrap().unwrap_err();
    assert!(matches!(failure.error, WrapError::WorkerPanicked));
    // Sticky terminal failure is intentionally retained by the model ticket;
    // destroying the UI or dropping this owner is never claimed as success.
    drop(failure);
}
