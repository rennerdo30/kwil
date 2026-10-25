//! The terminal owns control metadata only. Source-bearing completions stay in
//! the original guarded model slot through live acceptance and shutdown.
use super::*;
use kwil_view_model::{
    visual_rows::MetadataReservation,
    workspace_document::{DocumentShutdownBindReason, WorkspaceDocumentShutdown, WorkspaceDocumentUpdate},
};

/// A TUI that never admitted its document worker returns the original owner.
/// The caller decides its next destination; no View extraction or reopen is
/// performed, and this is the same allocation counter supplied by the caller.
#[must_use = "retain or transfer the exact Workspace, budget and startup error"]
pub struct TuiStartRefusal {
    pub workspace: Workspace,
    pub budget: AllocationBudget,
    pub error: std::io::Error,
}
impl std::fmt::Debug for TuiStartRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TuiStartRefusal").field("error", &self.error).finish_non_exhaustive()
    }
}

/// Reuse the existing cleanup ticket as the ONE source owner, including before
/// header admission/terminal initialization. Fallible setup borrows this guard.
pub(super) struct StartupOwner {
    pub ticket: ShutdownTicket,
    pub budget: AllocationBudget,
}
impl StartupOwner {
    pub fn new(workspace: Workspace, budget: AllocationBudget) -> Self {
        Self {
            ticket: ShutdownTicket {
                document: None,
                highlighting: None,
                search_binding: None,
                search_release_refusal: None,
                terminal_panic: None,
                wrap: None,
                view: Some(workspace),
                search: None,
                worker: None,
                rx: None,
                #[cfg(test)]
                stopped: None,
                #[cfg(test)]
                document_started: false,
                terminal_failure: None,
            },
            budget,
        }
    }
    pub fn worker_admitted(&self) -> bool {
        self.ticket.document.as_ref().is_some_and(|document| document.transport.is_some())
    }
    pub fn refuse(mut self, error: std::io::Error) -> TuiStartRefusal {
        assert!(!self.worker_admitted(), "an admitted worker keeps its existing cleanup ticket");
        let error = self.ticket.document.as_mut().and_then(DocumentLane::take_unstarted_error).unwrap_or(error);
        TuiStartRefusal {
            workspace: self.ticket.view.take().expect("original startup Workspace"),
            budget: self.budget,
            error,
        }
    }
    pub fn into_parts(self) -> (ShutdownTicket, AllocationBudget) {
        (self.ticket, self.budget)
    }
}

pub(super) struct DocumentLane {
    pub transport: Option<WorkspaceDocumentTransport>,
    pub shutdown: Option<WorkspaceDocumentShutdown>,
    pub bind_reason: Option<DocumentShutdownBindReason>,
    pub settlement_reason: Option<DocumentResultSettlementReason>,
    pub refused_prepare: Option<mpsc::TrySendError<Box<DocumentRequest>>>,
    _startup_refusal: Option<WorkspaceDocumentStartFailure>,
    pub retry: Option<DocumentWrapRetryToken>,
    pub replacement: Option<kwil_view_model::workspace_document::ExactReplacementReceipt>,
    pub deadline: Option<std::time::Instant>,
    pub settled: bool,
    // Includes destination/refusal overlap before construction or startup.
    // Search's scalar source-release control uses this SAME aggregate too.
    _metadata: MetadataReservation,
}
impl DocumentLane {
    pub fn new(budget: &AllocationBudget) -> std::io::Result<Self> {
        let bytes = (std::mem::size_of::<ShutdownTicket>()
            + std::mem::size_of::<ShutdownSlot>()
            + std::mem::size_of::<RetainedShutdownError>()
            + std::mem::size_of::<CombinedShutdownError>()
            + std::mem::size_of::<WorkspaceDocumentStartFailure>()
            + std::mem::size_of::<TuiStartRefusal>()
            + std::mem::size_of::<AllocationBudget>()
            + std::mem::size_of::<kwil_view_model::workspace_document::DocumentShutdownBindRefusal>())
        // Full worker baselines, not only the new settlement delta. The
        // search baseline includes its source-release Shared/allocation;
        // keys, receipts and refusals are inline in the ticket overlap
        // above, so do not add the old overlapping release constant.
        // Worker facades are already inline in those frontend owners;
        // deduct only these explicit headers from the full estimator.
        .checked_add(
            kwil_view_model::search_session::SearchWorker::checked_retained_metadata_bytes()
                .and_then(|bytes| {
                    bytes.checked_sub(std::mem::size_of::<kwil_view_model::search_session::SearchWorker>())
                })
                .ok_or_else(|| std::io::Error::other("TUI search metadata overflow"))?,
        )
        .and_then(|bytes| {
            bytes.checked_add(
                kwil_view_model::highlighting::HighlightWorker::checked_retained_metadata_bytes()?
                    .checked_sub(std::mem::size_of::<kwil_view_model::highlighting::HighlightWorker>())?,
            )
        })
        .and_then(|bytes| {
            bytes.checked_add(kwil_view_model::highlight_session::HighlightSession::shutdown_metadata_bytes()?)
        })
        .ok_or_else(|| std::io::Error::other("TUI worker metadata overflow"))?;
        let metadata = budget.reserve_metadata(bytes).map_err(std::io::Error::other)?;
        Ok(Self {
            transport: None,
            shutdown: None,
            bind_reason: None,
            settlement_reason: None,
            refused_prepare: None,
            _startup_refusal: None,
            retry: None,
            replacement: None,
            deadline: None,
            settled: false,
            _metadata: metadata,
        })
    }
    pub fn started(
        &mut self,
        result: Result<WorkspaceDocumentTransport, WorkspaceDocumentStartFailure>,
    ) -> std::io::Result<()> {
        match result {
            Ok(transport) => {
                self.transport = Some(transport);
                Ok(())
            }
            Err(WorkspaceDocumentStartFailure::PrepareRefused(refusal)) => {
                self.transport = Some(refusal.transport);
                self.refused_prepare = Some(refusal.error);
                Err(std::io::Error::other("Document Prepare refused; exact started owner retained for cleanup"))
            }
            Err(refusal @ WorkspaceDocumentStartFailure::NotStarted { .. }) => {
                self._startup_refusal = Some(refusal);
                let message = format!("Document worker not started: {:?}", self._startup_refusal.as_ref().unwrap());
                Err(std::io::Error::other(message))
            }
        }
    }
    /// Restore the actual pre-admission I/O error to the ownership-returning
    /// entry. The older formatted diagnostic never replaces this error owner.
    fn take_unstarted_error(&mut self) -> Option<std::io::Error> {
        use kwil_view_model::workspace_document::WorkspaceDocumentStartError;
        let WorkspaceDocumentStartFailure::NotStarted { error, .. } = self._startup_refusal.take()? else {
            unreachable!("started Prepare refusals live in their exact separate fields")
        };
        Some(match error {
            WorkspaceDocumentStartError::Spawn(error) => error,
            WorkspaceDocumentStartError::Admission(error) => std::io::Error::other(error),
            WorkspaceDocumentStartError::Allocation(error) => std::io::Error::other(error),
            WorkspaceDocumentStartError::WorkerAlreadyPresent => {
                std::io::Error::other("Document worker already present")
            }
        })
    }
    pub fn begin_shutdown(&mut self, timeout: Duration) -> Result<std::time::Instant, String> {
        if let Some(deadline) = self.deadline {
            return Ok(deadline);
        }
        let deadline = std::time::Instant::now().checked_add(timeout).ok_or("TUI shutdown deadline overflow")?;
        self.deadline = Some(deadline);
        Ok(deadline)
    }
    pub fn wrap_failed(&self) -> bool {
        self.wrap_failure_reason().is_some()
    }
    pub fn wrap_failure_reason(&self) -> Option<DocumentResultSettlementReason> {
        self.transport
            .as_ref()
            .map(WorkspaceDocumentTransport::status)
            .or_else(|| self.shutdown.as_ref().map(WorkspaceDocumentShutdown::status))
            .and_then(|status| status.failure)
            .filter(|reason| {
                matches!(
                    reason,
                    DocumentResultSettlementReason::WrapSettlementFailed
                        | DocumentResultSettlementReason::WrapThreadExitPending
                        | DocumentResultSettlementReason::WrapNativeExit(_)
                )
            })
    }
    pub fn retry_wrap(&mut self, timeout: Duration) -> Result<(), String> {
        if self.retry.is_some() {
            return Err("Wrap retry is already pending; its exact token is retained".into());
        }
        let timeout = self
            .deadline
            .map_or(timeout, |deadline| timeout.min(deadline.saturating_duration_since(std::time::Instant::now())));
        self.retry = Some(
            if let Some(transport) = &mut self.transport {
                transport.request_wrap_retry(timeout)
            } else if let Some(shutdown) = &mut self.shutdown {
                shutdown.request_wrap_retry(timeout)
            } else {
                return Err("Wrap retry has no document owner".into());
            }
            .map_err(|reason| format!("Wrap retry: {reason:?}"))?,
        );
        Ok(())
    }
    pub fn acknowledge_retry(&mut self) -> Result<(), String> {
        let Some(token) = self.retry.take() else { return Ok(()) };
        let result = if let Some(transport) = &mut self.transport {
            transport.acknowledge_wrap_retry(token)
        } else if let Some(shutdown) = &mut self.shutdown {
            shutdown.acknowledge_wrap_retry(token)
        } else {
            self.retry = Some(token);
            return Err("Wrap retry ACK has no document owner".into());
        };
        if let Err(refusal) = result {
            self.retry = Some(refusal.token);
            return Err(format!("Wrap receipt accepted; retry ACK retained: {:?}", refusal.reason));
        }
        if matches!(
            self.settlement_reason,
            Some(
                DocumentResultSettlementReason::WrapSettlementFailed
                    | DocumentResultSettlementReason::WrapThreadExitPending
                    | DocumentResultSettlementReason::WrapNativeExit(_)
            )
        ) {
            self.settlement_reason = None;
        }
        Ok(())
    }
    /// One event, one sealed receive. False admission leaves the original slot
    /// untouched, including a harness holding it for one preview frame.
    pub fn receive(
        &mut self,
        view: &mut Workspace,
        search: &mut search_ui::SearchHost,
        keys: &mut KeymapEngine,
        admitted: bool,
    ) -> Result<Option<WorkspaceDocumentUpdate>, String> {
        let Some(transport) = &mut self.transport else { return Ok(None) };
        if self.replacement.is_some() {
            return Err("Replacement acknowledgement retained; document admission closed".into());
        }
        let info = transport.result_info();
        let update = match transport.receive(view, admitted) {
            Ok(update) => update,
            Err(
                reason @ (DocumentResultSettlementReason::WrapSettlementFailed
                | DocumentResultSettlementReason::WrapThreadExitPending
                | DocumentResultSettlementReason::WrapNativeExit(_)),
            ) => {
                let status = transport.status();
                if !status.retry_pending && !status.retry_acknowledgement_required {
                    self.retry = None;
                }
                view.set_notice(format!("Wrap shutdown retained ({reason:?}); use Retry Wrap in the command palette"));
                return Ok(None);
            }
            Err(reason) => {
                self.settlement_reason = Some(reason);
                return Err(format!("Bounded document delivery: {reason:?}; exact owner retained"));
            }
        };
        if let Some(update) = &update {
            self.replacement = update.replacement;
            let interactive = self.deadline.is_none() && !view.wants_exit();
            search.document_update(view, keys, update, info, interactive)?;
            self.replacement = None;
        }
        // Accepting the receipt leaves its completed carrier in the original
        // slot until the producer retires it off UI. Keep the exact token
        // through that interval; the producer's capacity wake may return no
        // document update. Only an empty, healthy slot permits the ACK.
        if self.retry.is_some()
            && view.wrap_cleanup_ready()
            && self.transport.as_ref().is_some_and(|transport| {
                let status = transport.status();
                !status.occupied && status.failure.is_none() && status.retry_acknowledgement_required
            })
        {
            self.acknowledge_retry()?;
        }
        Ok(update)
    }
    pub fn ready(
        &mut self,
        view: &mut Workspace,
        search: &mut search_ui::SearchHost,
        keys: &mut KeymapEngine,
    ) -> Result<bool, String> {
        // Paint consumes only a borrow of the charged update. No rows are
        // cloned into a retained Vec; ratatui uses the Workspace's cache.
        let update = self.receive(view, search, keys, true)?;
        Ok(update.is_some() || self.wrap_failed())
    }
}
