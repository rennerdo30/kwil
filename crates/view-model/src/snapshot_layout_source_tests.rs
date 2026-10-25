//! SOURCE ONLY / NOT RUN. ROOT owns registration, compilation and controls.
use super::*;
use crate::visual_rows::task::{TaskRequest, TaskStep};
use crate::visual_rows::{
    LayoutEngine, LayoutGeometry, LayoutKey, NoCancellation, SnapshotSource, TopAnchor, WrapSource,
};
use kwil_core_text::Buffer;
fn snapshot(text: &[u8]) -> Snapshot {
    let mut buffer = Buffer::empty();
    for byte in text.iter().rev() {
        buffer.insert(0, &[*byte]).unwrap();
    }
    buffer.snapshot()
}
fn key() -> LayoutKey {
    LayoutKey {
        source: SourceIdentity { document_id: 21, source_epoch: 28, incarnation: 17, revision: 3 },
        geometry: LayoutGeometry { columns: 4, rows: 8, tab_size: 4, ..LayoutGeometry::default() },
        ..LayoutKey::default()
    }
}
#[test]
fn snapshot_adapter_one_byte_quanta_match_existing_engine_and_preserve_owner() {
    eprintln!(
        "SNAPSHOT_STACK_ENVELOPE {:?}",
        crate::visual_rows::task::VisualTask::<SnapshotLayoutSource>::stack_envelope()
    );
    let key = key();
    let text = "one two\t界e\u{301}\r\n\nlast";
    let snapshot = snapshot(text.as_bytes());
    let reference = SnapshotSource::new(key.source, snapshot.clone());
    let mut engine = LayoutEngine::with_chunk_limit(AllocationBudget::default(), 1).unwrap();
    let expected = engine.reference_fill(key, &reference, TopAnchor::default(), &NoCancellation).unwrap();
    let budget = AllocationBudget::default();
    let source =
        SnapshotLayoutSource::try_new(snapshot, key.source, &budget).unwrap_or_else(|error| panic!("{}", error.error));
    let engine = LayoutEngine::with_chunk_limit(budget, 1).unwrap();
    let mut task = match engine.begin_task(source, TaskRequest::Fill { key, anchor: TopAnchor::default() }) {
        std::ops::ControlFlow::Continue(task) => task,
        std::ops::ControlFlow::Break(error) => panic!("{}", error.error),
    };
    let mut done = false;
    for _ in 0..500_000 {
        match task.step(ReadWork::new(1, 1), &AtomicBool::new(false)) {
            TaskStep::Pending => {}
            TaskStep::Ready => {
                done = true;
                break;
            }
            TaskStep::Failed(error) => panic!("{error:?}"),
            TaskStep::Cancelled => panic!("cancel"),
        }
    }
    assert!(done);
    let (_, source, _, output) = task.into_parts();
    assert_eq!(source.metadata().identity, key.source);
    let Some(output) = output else { panic!("page") };
    let std::ops::ControlFlow::Continue(actual) = output.into_page() else { panic!("page") };
    assert_eq!(actual.rows(), expected.rows());
    for row in 0..actual.rows().len() {
        assert_eq!(actual.atoms(row), expected.atoms(row));
    }
    assert_eq!(actual.syntax_windows(), expected.syntax_windows());
    let returned = source.into_snapshot();
    let mut bytes = Vec::new();
    returned.write_to(&mut bytes).unwrap();
    assert_eq!(bytes, text.as_bytes());
    assert_eq!(reference.len(), returned.len());
}
#[test]
fn snapshot_read_pending_cancel_error_are_atomic_and_heap_charge_retires() {
    let key = key();
    let budget = AllocationBudget::default();
    let before = budget.retained_bytes();
    let source = SnapshotLayoutSource::try_new(snapshot(b"abc"), key.source, &budget)
        .unwrap_or_else(|error| panic!("{}", error.error));
    let source_charge = budget.retained_bytes();
    let mut query = source.new_read_query(0, 3, &budget).unwrap();
    let mut output = [77; 3];
    assert!(budget.retained_bytes() >= source_charge + 3 + SOURCE_WINDOW);
    assert!(matches!(
        source.poll_read(&mut query, &mut output, &mut ReadWork::new(1, 1), &AtomicBool::new(false)),
        TaskPoll::Pending
    ));
    assert_eq!(output, [77; 3]);
    assert!(matches!(
        source.poll_read(&mut query, &mut output, &mut ReadWork::new(100, 100), &AtomicBool::new(true)),
        TaskPoll::Cancelled
    ));
    assert_eq!(output, [77; 3]);
    drop(query);
    let mut invalid = source.new_read_query(100, 3, &budget).unwrap();
    assert!(matches!(
        source.poll_read(&mut invalid, &mut output, &mut ReadWork::new(100, 100), &AtomicBool::new(false)),
        TaskPoll::Failed(_)
    ));
    assert_eq!(output, [77; 3]);
    drop(invalid);
    assert_eq!(budget.retained_bytes(), source_charge);
    drop(source.into_snapshot());
    assert_eq!(budget.retained_bytes(), before);
}
#[test]
fn snapshot_source_refusal_returns_snapshot_and_rejects_a_second_allowance() {
    let key = key();
    let budget = AllocationBudget::new(0);
    let before = budget.retained_bytes();
    match SnapshotLayoutSource::try_new(snapshot(b"exact"), key.source, &budget) {
        Err(SnapshotLayoutRefusal { snapshot, error: WrapError::Admission(_) }) => assert_eq!(snapshot.len(), 5),
        _ => panic!("must refuse before allocating source/query storage"),
    }
    assert_eq!(budget.retained_bytes(), before);
    let budget = AllocationBudget::default();
    let source = SnapshotLayoutSource::try_new(snapshot(b"exact"), key.source, &budget)
        .unwrap_or_else(|error| panic!("{}", error.error));
    let separate = AllocationBudget::default();
    let separate_before = separate.retained_bytes();
    assert!(matches!(source.new_read_query(0, 1, &separate), Err(WrapError::Invalid(_))));
    assert_eq!(separate.retained_bytes(), separate_before);
}

#[test]
fn staged_snapshot_read_rejects_another_owner_with_the_same_cached_identity() {
    let key = key();
    let budget = AllocationBudget::default();
    let source_a = SnapshotLayoutSource::try_new(snapshot(b"a"), key.source, &budget)
        .unwrap_or_else(|error| panic!("{}", error.error));
    let source_b = SnapshotLayoutSource::try_new(snapshot(b"b"), key.source, &budget)
        .unwrap_or_else(|error| panic!("{}", error.error));
    assert_eq!(source_a.metadata(), source_b.metadata());
    let mut query = source_a.new_read_query(0, 1, &budget).unwrap();
    let mut output = [77];
    for _ in 0..20_000 {
        let status = source_a.poll_read(&mut query, &mut output, &mut ReadWork::new(1, 1), &AtomicBool::new(false));
        assert!(matches!(status, TaskPoll::Pending));
        assert_eq!(output, [77]);
        if query.returned.is_some() {
            break;
        }
    }
    assert_eq!(query.returned, Some(1));
    let mut work = ReadWork::new(1024, 1024);
    let before = work;
    assert!(matches!(
        source_b.poll_read(&mut query, &mut output, &mut work, &AtomicBool::new(false)),
        TaskPoll::Failed(TaskError::Source(InputError::QueryChanged))
    ));
    assert_eq!(output, [77]);
    assert_eq!(work, before);
    assert!(matches!(
        source_a.poll_read(&mut query, &mut output, &mut work, &AtomicBool::new(true)),
        TaskPoll::Cancelled
    ));
    assert_eq!(output, [77]);
    // A move preserves the original owner key; a rejected B poll and explicit
    // cancellation do not erase A's admitted staging or prepaid copy credit.
    let moved_a = source_a;
    assert!(matches!(
        moved_a.poll_read(&mut query, &mut output, &mut work, &AtomicBool::new(false)),
        TaskPoll::Ready(1)
    ));
    assert_eq!(output, *b"a");
    drop(query);
    assert_eq!(moved_a.into_snapshot().len(), 1);
    assert_eq!(source_b.into_snapshot().len(), 1);
}

#[test]
fn snapshot_read_is_bound_when_created_before_any_core_poll() {
    let key = key();
    let budget = AllocationBudget::default();
    let snapshot = snapshot(b"same");
    let source_a = SnapshotLayoutSource::try_new(snapshot.clone(), key.source, &budget)
        .unwrap_or_else(|error| panic!("{}", error.error));
    let source_b =
        SnapshotLayoutSource::try_new(snapshot, key.source, &budget).unwrap_or_else(|error| panic!("{}", error.error));
    let mut query = source_a.new_read_query(0, 1, &budget).unwrap();
    let mut output = [77];
    assert!(matches!(
        source_b.poll_read(&mut query, &mut output, &mut ReadWork::new(1024, 1024), &AtomicBool::new(false)),
        TaskPoll::Failed(TaskError::Source(InputError::QueryChanged))
    ));
    assert_eq!(output, [77]);
    assert!(query.returned.is_none());
}

#[test]
fn generic_snapshot_owner_refusal_and_success_move_the_exact_sole_arc() {
    use std::sync::Arc;
    let identity = key().source;
    let owner = Arc::new(SnapshotSource::new(identity, snapshot(b"exact owner")));
    let pointer = Arc::as_ptr(&owner);
    assert_eq!(Arc::strong_count(&owner), 1);
    let refusal = match SnapshotLayoutSource::try_with_owner(owner, identity, &AllocationBudget::new(0)) {
        Err(refusal) => refusal,
        Ok(_) => panic!("generic owner was constructed without admission"),
    };
    assert!(matches!(refusal.error, WrapError::Admission(_)));
    assert_eq!(Arc::as_ptr(&refusal.owner), pointer);
    assert_eq!(Arc::strong_count(&refusal.owner), 1);
    let budget = AllocationBudget::default();
    let before = budget.retained_bytes();
    let source = SnapshotLayoutSource::try_with_owner(refusal.owner, identity, &budget)
        .unwrap_or_else(|refused| panic!("{}", refused.error));
    let charged = budget.retained_bytes() - before;
    type Adapter = SnapshotLayoutSource<Arc<SnapshotSource>>;
    let expected = 3
        * (std::mem::size_of::<Adapter>()
            + std::mem::size_of::<LayoutLineQuery>()
            + std::mem::size_of::<LayoutRankQuery>()
            + std::mem::size_of::<LayoutProvenanceQuery>()
            + std::mem::size_of::<SnapshotReadQuery>())
        + std::mem::size_of::<SnapshotOwnerRefusal<Arc<SnapshotSource>>>()
        + LayoutReadQuery::extra_resident_bytes()
        + std::mem::size_of::<AtomicU64>()
        + std::mem::size_of::<MetadataReservation>();
    assert_eq!(charged, expected, "generic owner/return headers must use their actual concrete size");
    let owner = source.into_owner();
    assert_eq!(Arc::as_ptr(&owner), pointer);
    assert_eq!(Arc::strong_count(&owner), 1);
    assert_eq!(budget.retained_bytes(), before);
    drop(owner);
}
