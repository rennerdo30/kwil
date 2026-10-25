use super::*;
use kwil_core_syntax::{TextSource, WorkerSettlementReason};
use std::{
    borrow::Cow,
    io,
    sync::{Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

fn settle(host: &mut HighlightSession, deadline: Instant) -> Result<(), WorkerSettlementReason> {
    loop {
        match host.settle_until(deadline) {
            Err(WorkerSettlementReason::ThreadExitPending) if Instant::now() < deadline => thread::yield_now(),
            result => return result,
        }
    }
}
struct Source {
    entered: mpsc::Sender<()>,
    gate: Mutex<Option<mpsc::Receiver<()>>>,
    dropped: mpsc::Sender<thread::ThreadId>,
    deadline: Instant,
}
impl TextSource for Source {
    fn len(&self) -> usize {
        1
    }
    fn chunk(&self, _: usize, _: usize) -> io::Result<Cow<'_, [u8]>> {
        if let Some(gate) = self.gate.lock().unwrap().take() {
            self.entered.send(()).unwrap();
            gate.recv_timeout(self.deadline.saturating_duration_since(Instant::now())).unwrap();
        }
        Ok(Cow::Borrowed(b"x"))
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.dropped.send(thread::current().id()).unwrap();
    }
}
fn request(source: Arc<dyn TextSource>) -> HighlightRequest {
    HighlightRequest {
        key: HighlightKey { document_id: DocumentId(1), revision: 1, viewport_generation: 1 },
        source,
        path: Arc::new("unknown".into()),
        choice: LanguageChoice::Explicit(Language::Generic),
        mappings: Arc::from([]),
        visible: 0..1,
        previous_revision: None,
        edit: None,
        snapshot: None,
    }
}
#[test]
fn timeout_keeps_all_facade_owners_then_success_retires_on_calling_off_ui_executor() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let ui = thread::current().id();
    let (entered, observed) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let (dropped, destroyed) = mpsc::channel();
    let source: Arc<dyn TextSource> = Arc::new(Source { entered, gate: Mutex::new(Some(gate)), dropped, deadline });
    let source_identity = Arc::downgrade(&source);
    let mut host = HighlightSession::new(|| {}).unwrap();
    host.worker.as_ref().unwrap().submit(request(Arc::clone(&source))).unwrap();
    host.rejected = Some(request(Arc::clone(&source)));
    host.snapshot = Some(Arc::new(SnapshotTextSource::new(kwil_core_text::Snapshot::default())));
    let snapshot_identity = Arc::downgrade(host.snapshot.as_ref().unwrap());
    host.source = Some(source);
    observed.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
    thread::spawn(move || {
        let executor = thread::current().id();
        assert_ne!(executor, ui);
        assert_eq!(host.settle_until(Instant::now()), Err(WorkerSettlementReason::TimedOut));
        assert!(host.worker.is_some());
        assert!(Arc::downgrade(host.source.as_ref().unwrap()).ptr_eq(&source_identity));
        assert!(Arc::downgrade(&host.rejected.as_ref().unwrap().source).ptr_eq(&source_identity));
        assert!(Arc::downgrade(host.snapshot.as_ref().unwrap()).ptr_eq(&snapshot_identity));
        assert!(destroyed.try_recv().is_err(), "timeout destroyed a frontend source");
        release.send(()).unwrap();
        assert_eq!(settle(&mut host, deadline), Ok(()));
        assert!(host.worker.is_none());
        assert!(host.source.is_none() && host.rejected.is_none() && host.snapshot.is_none());
        assert_eq!(destroyed.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap(), executor);
        assert!(source_identity.upgrade().is_none());
        assert!(snapshot_identity.upgrade().is_none());
        assert_eq!(host.settle_until(deadline), Ok(()));
        drop(host); // settled Drop cannot create a detached cleanup thread.
    })
    .join()
    .unwrap();
}

#[test]
fn panic_keeps_exact_session_and_sources_and_never_reports_fake_success() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let marker = Arc::new(41_u8);
    let expected = Arc::clone(&marker);
    let (entered, observed) = mpsc::channel();
    let mut host = HighlightSession::new(move || {
        entered.send(()).unwrap();
        std::panic::panic_any(Arc::clone(&marker));
    })
    .unwrap();
    let source: Arc<dyn TextSource> = Arc::new(vec![b'x']);
    host.worker.as_ref().unwrap().submit(request(Arc::clone(&source))).unwrap();
    host.rejected = Some(request(Arc::clone(&source)));
    host.source = Some(Arc::clone(&source));
    host.snapshot = Some(Arc::new(SnapshotTextSource::new(kwil_core_text::Snapshot::default())));
    observed.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
    thread::spawn(move || {
        assert_eq!(settle(&mut host, deadline), Err(WorkerSettlementReason::WorkerPanicked));
        assert!(host.worker.is_some());
        assert!(Arc::ptr_eq(host.source.as_ref().unwrap(), &source));
        assert!(Arc::ptr_eq(&host.rejected.as_ref().unwrap().source, &source));
        assert!(host.snapshot.is_some());
        assert!(Arc::ptr_eq(host.panic_payload().unwrap().downcast_ref::<Arc<u8>>().unwrap(), &expected));
        assert_eq!(host.settle_until(deadline), Err(WorkerSettlementReason::WorkerPanicked));
        assert!(host.source.is_some() && host.rejected.is_some() && host.snapshot.is_some());
        // Explicit frontend disposal is OFF UI after join/right retirement.
        // The original typed panic owner stays sticky; its Drop is quarantine.
        let error = host.worker.take().unwrap().shutdown().unwrap_err();
        assert_eq!(error.reason(), WorkerSettlementReason::WorkerPanicked);
        let worker = error.into_worker();
        assert!(Arc::ptr_eq(worker.panic_payload().unwrap().downcast_ref::<Arc<u8>>().unwrap(), &expected));
        drop((host.source.take(), host.rejected.take(), host.snapshot.take(), source, worker));
        drop(host);
    })
    .join()
    .unwrap();
}

#[test]
fn stop_retains_frontend_sources_and_whole_session_until_explicit_off_ui_shutdown() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut host = HighlightSession::new(|| {}).unwrap();
    let source: Arc<dyn TextSource> = Arc::new(vec![b'x']);
    host.source = Some(Arc::clone(&source));
    host.rejected = Some(request(Arc::clone(&source)));
    host.snapshot = Some(Arc::new(SnapshotTextSource::new(kwil_core_text::Snapshot::default())));
    host.stop();
    let retained = host.worker.is_some()
        && host.source.as_ref().is_some_and(|owned| Arc::ptr_eq(owned, &source))
        && host.rejected.as_ref().is_some_and(|request| Arc::ptr_eq(&request.source, &source))
        && host.snapshot.is_some();
    let result = settle(&mut host, deadline);
    let cleaned = host.worker.is_none() && host.source.is_none() && host.rejected.is_none() && host.snapshot.is_none();
    let cleanup = host.shutdown();
    assert!(retained, "stop must retain every original frontend owner");
    assert_eq!(result, Ok(()));
    assert!(cleaned);
    assert!(cleanup.is_continue());
}
