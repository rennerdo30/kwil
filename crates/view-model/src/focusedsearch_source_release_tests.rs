//! Focused host-boundary controls; ROOT runs these with the real-worker tests.
use super::*;
use std::{
    sync::{Mutex, mpsc},
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(5);

fn request(generation: u64) -> SearchRequest {
    let mut buffer = kwil_core_text::Buffer::empty();
    buffer.insert(0, b"owned").unwrap();
    SearchRequest {
        key: SearchKey { document_id: 7, revision: 0, generation },
        task: SearchTask::Document(DocumentSearch {
            snapshot: buffer.snapshot(),
            query: Query::literal("x".into()),
            ranges: std::iter::once(0..5).collect(),
            page: Page::default(),
            navigation: None,
            replacement: None,
        }),
    }
}

#[test]
fn prebound_host_key_survives_take_without_sync_or_new_search_generation() {
    let mut host = SearchSession::new(|| {}).unwrap();
    let foreign = SearchSession::new(|| {}).unwrap();
    let key = host.source_release_key().unwrap();
    let foreign_key = foreign.source_release_key().unwrap();
    let worker =
        host.take_shutdown_with(Some(request(1))).unwrap_or_else(|_| panic!("first close must admit its owner"));
    assert!(host.source_release_key().is_none());
    assert_eq!(host.generation, 0);
    assert!(host.state.key.is_none());
    let receipt = worker.wait_inputs_released(WAIT).unwrap();
    assert!(receipt.valid_for(&key));
    assert!(!receipt.valid_for(&foreign_key));
    worker.shutdown_bounded(WAIT).unwrap();
    let mut foreign = foreign;
    foreign.take_shutdown().unwrap().shutdown_bounded(WAIT).unwrap();
}

#[test]
fn close_refusal_restores_original_worker_internal_retry_and_exact_external_owner() {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let mut host = SearchSession::new(move || {
        entered_tx.send(()).unwrap();
        release_rx.lock().unwrap().recv_timeout(WAIT).unwrap();
    })
    .unwrap();
    let key = host.source_release_key().unwrap();
    // A first close occupies the internal lifetime reservation. Keep its actual
    // coordinator at a source-free wake so subsequent close cannot race exit.
    host.worker.as_ref().unwrap().close_with(Some(request(1))).continue_value().unwrap();
    entered_rx.recv_timeout(WAIT).unwrap();
    host.rejected = Some(request(2));
    let external = host.take_shutdown_with(Some(request(3))).err().unwrap().unwrap();
    assert_eq!(external.key.generation, 3);
    assert_eq!(host.rejected.as_ref().unwrap().key.generation, 2);
    assert!(host.worker.is_some(), "refusal retains the original facade in its session");
    let worker = host.worker.as_ref().unwrap();
    let receipt = worker.wait_inputs_released(WAIT).unwrap();
    assert!(receipt.valid_for(&key), "refused admission cannot change the bound worker/epoch");
    let SearchTask::Document(doc) = &external.task else { unreachable!() };
    assert_eq!(doc.snapshot.len(), 5);
    release_tx.send(()).unwrap();
    // All refused source owners are explicitly retired on a worker, rather
    // than being lost in the wrapper's original Option<Box<request>> refusal.
    let retirement = SearchWorker::new(|| {}).unwrap();
    retirement.close_with_requests(host.rejected.take(), Some(*external)).continue_value().unwrap();
    retirement.shutdown_bounded(WAIT).unwrap();
    host.worker.take().unwrap().shutdown_bounded(WAIT).unwrap();
}

#[test]
fn stop_keeps_original_worker_for_explicit_off_ui_handoff() {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut host = SearchSession::new(|| {}).unwrap();
    let key = host.source_release_key().unwrap();
    host.stop();
    let retained = host.worker.is_some();
    let worker = host.take_shutdown().unwrap();
    let receipt = worker.wait_inputs_released(deadline.saturating_duration_since(std::time::Instant::now()));
    // The explicit unbounded compatibility cleanup is OFF UI in this fixture;
    // its original handle/right are cleaned BEFORE assertions.
    let valid = receipt.as_ref().is_ok_and(|receipt| receipt.valid_for(&key));
    let cleanup = worker.shutdown();
    assert!(retained, "stop cannot discard the only original shutdown facade");
    assert!(valid);
    assert!(cleanup.is_ok());
    assert!(host.worker.is_none());
}
