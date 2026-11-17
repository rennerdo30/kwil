//! `kwil --wait`: reports to the instance server when the tabs a launcher asked
//! for are gone. State-based, so every way a tab can leave the session counts
//! (close, discard, a cancelled binary offer) and nothing polls or arms a timer.
use kwil_core_workspace::{
    instance::{WaitOutcome, Waits},
    session::{DocumentId, Session},
};
use std::sync::Arc;

#[derive(Default)]
pub(crate) struct WaitWatch {
    sink: Option<Arc<Waits>>,
    pending: Vec<(u64, Vec<DocumentId>)>,
}

impl WaitWatch {
    pub(crate) fn set_sink(&mut self, sink: Arc<Waits>) {
        self.sink = Some(sink);
    }

    /// Watches the tabs opened for `token`. Without a sink (no instance server)
    /// nobody is waiting, so nothing is kept.
    pub(crate) fn watch(&mut self, token: u64, tabs: Vec<DocumentId>, session: &Session) {
        if self.sink.is_some() {
            self.pending.push((token, tabs));
            self.settle(session);
        }
    }

    pub(crate) fn fail(&mut self, token: u64) {
        if let Some(sink) = &self.sink {
            sink.complete(token, WaitOutcome::OpenFailed);
        }
    }

    /// Completes every watch none of whose tabs is still open.
    pub(crate) fn settle(&mut self, session: &Session) {
        let Some(sink) = &self.sink else { return };
        self.pending.retain(|(token, tabs)| {
            let open = tabs.iter().any(|id| session.tabs().iter().any(|tab| tab.id == *id));
            if !open {
                sink.complete(*token, WaitOutcome::Closed);
            }
            open
        });
    }
}
