//! Post-event-loop settlement. An unrelated error must never skip accepted work.
use kwil_view_model::{
    Workspace,
    visual_rows::{AllocationBudget, MetadataReservation, WrapResult},
    workspace_document::{
        DocumentSearchBinding, DocumentShutdownBindReason, DocumentShutdownBindRefusal, DocumentShutdownReason,
        DocumentShutdownStatus, SearchInputsReleaseRefusal, WorkspaceDocumentShutdown,
    },
};
use std::time::{Duration, Instant};

/// Fixed, owner-free progress. The original failures remain in `Failure`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Live,
    Complete,
    NotStarted,
    UnstartedProofRefused,
    UnstartedRearmRefused,
    ExternalPending,
    RecoveryTimedOut,
    BindRefused,
    WrapPending,
    SearchCloseRefused,
    ReceiptIdentity,
    Document(DocumentShutdownReason),
    SearchInputs(kwil_view_model::search_session::SearchInputsReleaseError),
    Search(kwil_view_model::search_session::WorkerSettlementReason),
    Syntax(kwil_core_syntax::WorkerSettlementReason),
    Logs(super::log_ui::SettlementReason),
    LogRetirementRetry(kwil_view_model::log_session::LogRetirementRetryReason),
    Markdown(super::markdown_ui::SettlementReason),
    EditorFont(super::document_ui::FontSettlementReason),
    ChromeFont(super::document_ui::FontSettlementReason),
    Spawn,
    Panicked,
}

/// One inline transfer of the original frontend owners. Its full transient
/// overlap is admitted before startup alongside the retained failure headers.
pub(super) struct Owners {
    pub view: Workspace,
    pub wrap: super::wrap_ui::Host,
    pub markdown: super::markdown_ui::Host,
    pub logs: super::log_ui::Host,
    pub highlighting: Option<kwil_view_model::highlight_session::HighlightSession>,
    pub search_panel: kwil_view_model::search_panel::SearchPanel,
    pub search: Option<kwil_view_model::search_session::SearchSession>,
    pub external_inbox: kwil_view_model::document_worker::ExternalOpenInbox,
    pub external_open: Option<kwil_view_model::document_worker::ExternalOpen>,
}

/// One preallocated outcome owner, used during live delivery and returned in
/// GuiError on failure. No source-bearing callback owns this bundle.
pub struct Failure {
    budget: AllocationBudget,
    pub(super) live: super::document_ui::Host,
    pub(super) workspace: Option<Workspace>,
    bound: Option<WorkspaceDocumentShutdown>,
    bind_refusal: Option<DocumentShutdownBindRefusal>,
    binding_error: Option<DocumentShutdownBindReason>,
    retry_binding: bool,
    retry_log_retirement: bool,
    wrap: Option<super::wrap_ui::Host>,
    pub(super) markdown: Option<super::markdown_ui::Host>,
    logs: Option<super::log_ui::Host>,
    syntax: Option<kwil_view_model::highlight_session::HighlightSession>,
    search: Option<kwil_view_model::search_session::SearchSession>,
    panel: Option<kwil_view_model::search_panel::SearchPanel>,
    search_worker: Option<kwil_view_model::search_session::SearchWorker>,
    search_refusal: Option<Option<Box<kwil_view_model::search_session::SearchRequest>>>,
    search_proof_refusal: Option<SearchInputsReleaseRefusal>,
    search_bound: bool,
    search_inputs_released: bool,
    pub(super) replacements:
        [Option<(kwil_view_model::search_session::SearchKey, u64)>; kwil_core_workspace::session::MAX_TABS],
    pub(super) deadline: Option<Instant>,
    status: Status,
    pub(super) spawn_error: Option<std::io::Error>,
    pub(super) font_spawn_error: Option<super::document_ui::FontSpawnFailure>,
    pub(super) panic: Option<Box<dyn std::any::Any + Send>>,
    pub(super) auxiliary_error: Option<String>,
    pub(super) frontend_error: Option<super::GuiError>,
    pub(super) event_loop_error: Option<winit::error::EventLoopError>,
    no_document_worker: bool,
    document_complete: bool,
    rearm_error: Option<kwil_view_model::visual_rows::WrapError>,
    pub(super) external_inbox: Option<kwil_view_model::document_worker::ExternalOpenInbox>,
    pub(super) external_producer: Option<kwil_view_model::document_worker::ExternalOpen>,
    pub(super) font: Option<super::document_ui::StartupFont<super::Font>>,
    pub(super) chrome_font: Option<super::document_ui::StartupFont<super::font::UiFont>>,
    pub(super) font_size: Option<std::sync::mpsc::SyncSender<f32>>,
    pub(super) chrome_font_size: Option<std::sync::mpsc::SyncSender<f32>>,
    external_sealed: bool,
    _metadata: MetadataReservation,
}
#[derive(Clone, Copy)]
enum Operation {
    Shutdown,
    RecoverUnstarted,
}
impl std::fmt::Debug for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuiShutdownFailure")
            .field("status", &self.status)
            .field("deadline", &self.deadline)
            .field("auxiliary_error", &self.auxiliary_error)
            .field("frontend_error", &self.frontend_error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GUI shutdown retained: {:?}", self.status)?;
        if let Some(error) = self.original_error() {
            write!(f, "; {error}")?;
        }
        if let Some(error) = &self.auxiliary_error {
            write!(f, "; {error}")?;
        }
        if let Some(error) = &self.frontend_error {
            write!(f, "; {error}")?;
        }
        if let Some(reason) = self.bind_reason() {
            write!(f, "; binding: {reason:?}")?;
        }
        if let Some(error) = self.log_retirement_error() {
            write!(f, "; log retirement: {error}")?;
        }
        Ok(())
    }
}
impl Failure {
    #[cfg(test)]
    pub(super) fn admit(budget: &AllocationBudget) -> WrapResult<Box<Self>> {
        Self::admit_startup(budget, 0)
    }
    pub(super) fn admit_startup(budget: &AllocationBudget, prepared_overlap: usize) -> WrapResult<Box<Self>> {
        // The SAME Box becomes GuiError::Shutdown. Header, exact-refusal and
        // return overlap are charged before allocation or any worker startup.
        let bytes = (2 * std::mem::size_of::<Self>()
            + 2 * std::mem::size_of::<super::StartupFonts>()
            + 2 * std::mem::size_of::<Owners>()
            + 2 * std::mem::size_of::<std::ops::ControlFlow<Box<Self>, (Workspace, AllocationBudget)>>()
            + 2 * std::mem::size_of::<std::ops::ControlFlow<super::GuiStartRefusal, Result<(), super::GuiError>>>()
            + std::mem::size_of::<kwil_view_model::search_session::SearchRequest>())
        .checked_add(prepared_overlap)
        .and_then(|bytes| {
            bytes.checked_add(kwil_view_model::search_session::SearchWorker::checked_retained_metadata_bytes()?)
        })
        .and_then(|bytes| {
            bytes.checked_add(kwil_view_model::highlighting::HighlightWorker::checked_retained_metadata_bytes()?)
        })
        .and_then(|bytes| {
            bytes.checked_add(kwil_view_model::highlight_session::HighlightSession::shutdown_metadata_bytes()?)
        })
        .and_then(|bytes| {
            bytes.checked_add(kwil_view_model::log_session::LogSession::checked_retained_metadata_bytes()?)
        })
        .ok_or(kwil_view_model::visual_rows::WrapError::Admission("GUI worker metadata overflow"))?;
        let metadata = budget.reserve_metadata(bytes)?;
        let live = super::document_ui::Host::new(budget)?;
        Ok(Box::new(Self {
            budget: budget.clone(),
            live,
            workspace: None,
            bound: None,
            bind_refusal: None,
            binding_error: None,
            retry_binding: false,
            retry_log_retirement: false,
            wrap: None,
            markdown: None,
            logs: None,
            syntax: None,
            search: None,
            panel: None,
            search_worker: None,
            search_refusal: None,
            search_proof_refusal: None,
            search_bound: false,
            search_inputs_released: false,
            replacements: [None; kwil_core_workspace::session::MAX_TABS],
            deadline: None,
            status: Status::Live,
            spawn_error: None,
            font_spawn_error: None,
            panic: None,
            auxiliary_error: None,
            frontend_error: None,
            event_loop_error: None,
            no_document_worker: false,
            document_complete: false,
            rearm_error: None,
            external_inbox: None,
            external_producer: None,
            external_sealed: false,
            font: Some(super::document_ui::StartupFont::admit(budget)?),
            chrome_font: Some(super::document_ui::StartupFont::admit(budget)?),
            font_size: None,
            chrome_font_size: None,
            _metadata: metadata,
        }))
    }
    pub fn status(&self) -> Status {
        self.status
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
    pub fn auxiliary_error(&self) -> Option<&str> {
        self.auxiliary_error.as_deref()
    }
    pub fn frontend_error(&self) -> Option<&super::GuiError> {
        self.frontend_error.as_ref()
    }
    pub fn external_requests_pending(&self) -> bool {
        self.external_inbox.as_ref().is_some_and(|inbox| inbox.has_pending())
    }
    pub fn log_retirement_error(&self) -> Option<&str> {
        self.logs.as_ref().and_then(|host| host.retirement_error())
    }
    pub fn original_error(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.font_spawn_error {
            Some(super::document_ui::FontSpawnFailure::Admission(error)) => return Some(error),
            Some(super::document_ui::FontSpawnFailure::Spawn(error)) => return Some(error),
            Some(super::document_ui::FontSpawnFailure::AlreadyStarted) | None => {}
        }
        if let Some(error) = &self.rearm_error {
            return Some(error);
        }
        if let Some(error) = &self.spawn_error {
            return Some(error);
        }
        if let Some(error) = &self.event_loop_error {
            return Some(error);
        }
        self.bound.as_ref().and_then(WorkspaceDocumentShutdown::preparation_error)
    }
    pub fn panic_payload(&self) -> Option<&(dyn std::any::Any + Send)> {
        self.panic
            .as_deref()
            .or_else(|| self.font.as_ref().and_then(|font| font.panic_payload()))
            .or_else(|| self.chrome_font.as_ref().and_then(|font| font.panic_payload()))
            .or_else(|| self.markdown.as_ref().and_then(super::markdown_ui::Host::panic_payload))
            .or_else(|| self.syntax.as_ref().and_then(|host| host.panic_payload()))
            .or_else(|| self.logs.as_ref().and_then(|host| host.panic_payload()))
            .or_else(|| self.search_worker.as_ref().and_then(|worker| worker.panic_payload()))
    }
    #[cfg(test)]
    pub(super) fn document_state_for_test(
        &self,
    ) -> (
        bool,
        Option<kwil_view_model::document_worker::DocumentResultTransportStatus>,
        Option<kwil_view_model::workspace_document::ResultInfo>,
    ) {
        let (status, info) = if let Some(bundle) = &self.bound {
            (Some(bundle.status()), bundle.result_info())
        } else if let Some(transport) = &self.live.transport {
            (Some(transport.status()), transport.result_info())
        } else {
            (None, None)
        };
        (self.document_complete, status, info)
    }
    pub fn retained_document_error(
        &self,
    ) -> Option<kwil_view_model::document_worker::result_transport::DocumentResultInspection<'_>> {
        if self.document_complete {
            return None;
        }
        if let Some(bundle) = &self.bound {
            return Some(bundle.retained_commit_error());
        }
        self.live.transport.as_ref().map(|transport| transport.retained_commit_error())
    }
    pub fn bind_reason(&self) -> Option<&DocumentShutdownBindReason> {
        self.bind_refusal.as_ref().map(|refusal| &refusal.reason)
    }
    pub fn binding_error(&self) -> Option<&DocumentShutdownBindReason> {
        self.binding_error.as_ref()
    }
    /// The explicit request merely arms the existing executor. It does not move
    /// the refused Workspace/transport or create a new shutdown deadline.
    pub fn request_binding_retry(&mut self) {
        self.retry_binding = true;
    }
    pub fn request_log_retirement_retry(&mut self) -> Result<(), Status> {
        if !matches!(
            self.status,
            Status::Logs(super::log_ui::SettlementReason::Retirement) | Status::LogRetirementRetry(_)
        ) {
            return Err(self.status);
        }
        self.retry_log_retirement = true;
        Ok(())
    }
    pub fn request_prepublication_reset(&mut self) -> Result<(), Status> {
        if self.document_complete {
            return Err(Status::Complete);
        }
        if self.live.reset.is_some() || self.live.reset_refusal.is_some() {
            return Err(Status::ReceiptIdentity);
        }
        let bundle = self.bound.as_mut().ok_or(Status::Live)?;
        let delivery = bundle.result_info().and_then(|info| info.delivery()).ok_or(Status::ReceiptIdentity)?;
        let deadline = self.deadline.ok_or(Status::Live)?;
        let token = bundle
            .request_prepublication_reset(delivery, deadline.saturating_duration_since(Instant::now()))
            .map_err(|reason| Status::Document(DocumentShutdownReason::Transport(reason)))?;
        self.live.reset = Some(token);
        Ok(())
    }
    /// Explicit wake-failure acknowledgement keeps the exact original error.
    /// It cannot clear a commit panic, worker failure or a partial publication.
    pub fn acknowledge_delivery_failure(&mut self) -> bool {
        if self.document_complete {
            return false;
        }
        if self.live.delivery_failure.is_some() {
            return false;
        }
        self.live.delivery_failure = self.bound.as_mut().and_then(|bundle| bundle.acknowledge_delivery_failure());
        self.live.delivery_failure.is_some()
    }
    pub fn delivery_failure(&self) -> Option<&kwil_view_model::document_worker::DocumentDeliveryFailure> {
        self.live.delivery_failure.as_ref()
    }
    pub(super) fn attach(&mut self, owners: Owners) {
        assert!(self.workspace.is_none() && self.bound.is_none() && self.bind_refusal.is_none());
        let Owners {
            view: workspace,
            wrap,
            markdown,
            logs,
            highlighting: syntax,
            search_panel: panel,
            search,
            external_inbox,
            external_open: external_producer,
        } = owners;
        self.workspace = Some(workspace);
        self.wrap = Some(wrap);
        self.markdown = Some(markdown);
        self.logs = Some(logs);
        self.syntax = syntax;
        self.panel = Some(panel);
        self.search = search;
        self.external_inbox = Some(external_inbox);
        self.external_producer = external_producer;
        self.deadline.get_or_insert_with(|| Instant::now() + Duration::from_secs(35));
    }
    /// Explicit invocation of the existing off-UI document shutdown executor.
    /// The exact owner stays OUTSIDE catch_unwind and is only borrowed by it.
    pub fn retry(&mut self) -> Result<(), Status> {
        if self.status == Status::Complete {
            return Ok(());
        }
        if self.panic.is_some() {
            return Err(Status::Panicked);
        }
        self.execute(Operation::Shutdown)
    }
    /// Recover only an actually unstarted document lane. Every refusal returns
    /// the SAME preallocated box, retaining the original owners and errors.
    pub fn recover_unstarted(
        mut self: Box<Self>,
        deadline: Instant,
    ) -> std::ops::ControlFlow<Box<Self>, (Workspace, AllocationBudget)> {
        let stored = self.deadline.get_or_insert(deadline);
        *stored = (*stored).min(deadline);
        if self.execute(Operation::RecoverUnstarted).is_err() {
            return std::ops::ControlFlow::Break(self);
        }
        // Only non-panicking owner moves follow the off-UI proof. No source
        // read, reopen, View extraction or whole-document copy occurs here.
        let budget = self.budget.clone();
        let Some(workspace) = self.workspace.take() else {
            self.status = Status::UnstartedProofRefused;
            return std::ops::ControlFlow::Break(self);
        };
        std::ops::ControlFlow::Continue((workspace, budget))
    }
    fn execute(&mut self, operation: Operation) -> Result<(), Status> {
        if self.panic.is_some() {
            return Err(Status::Panicked);
        }
        let result = std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("kwil-gui-shutdown".into())
                .spawn_scoped(scope, || {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match operation {
                        Operation::Shutdown => self.settle_off_ui(),
                        Operation::RecoverUnstarted => self.recover_off_ui(),
                    }))
                })
                .map(|thread| thread.join())
        });
        match result {
            Ok(Ok(Ok(Ok(())))) => {
                self.status = Status::Complete;
                Ok(())
            }
            Ok(Ok(Ok(Err(reason)))) => {
                self.status = reason;
                Err(reason)
            }
            Ok(Err(payload)) | Ok(Ok(Err(payload))) => {
                self.panic = Some(payload);
                self.status = Status::Panicked;
                Err(Status::Panicked)
            }
            Err(error) => {
                self.spawn_error.get_or_insert(error);
                self.status = Status::Spawn;
                Err(Status::Spawn)
            }
        }
    }
    fn recover_off_ui(&mut self) -> Result<(), Status> {
        if self.live.ever_started
            || self.live.transport.is_some()
            || self.live.update.is_some()
            || self.live.start_failure.is_some()
            || self.live.prepare_refusal.is_some()
            || self.live.failure.is_some()
            || self.live.wrap_retry.is_some()
            || self.live.wrap_retry_refusal.is_some()
            || self.live.reset.is_some()
            || self.live.reset_refusal.is_some()
            || self.live.delivery_failure.is_some()
            || self.bound.is_some()
            || self.bind_refusal.is_some()
            || self.binding_error.is_some()
            || self.retry_binding
            || self.retry_log_retirement
            || self.replacements.iter().any(Option::is_some)
            || self.search_refusal.is_some()
            || self.search_proof_refusal.is_some()
            || self.workspace.is_none()
            || self.auxiliary_error.is_some()
            || self.panic_payload().is_some()
            || !matches!(&self.frontend_error, Some(super::GuiError::Unavailable(_) | super::GuiError::NoFont))
        {
            return Err(Status::UnstartedProofRefused);
        }
        let deadline = self.deadline.ok_or(Status::UnstartedProofRefused)?;
        if Instant::now() >= deadline {
            return Err(Status::RecoveryTimedOut);
        }
        // Production creates this pair together and never calls the sole
        // noncloneable producer while App owns it. Its only exposure is take +
        // on_external_ready after document startup. Thus Some proves no caller
        // can race this otherwise-advisory bit; None must always refuse.
        // Close first, but retain BOTH owners on pending/refusal. No accepted
        // request is drained or discarded to manufacture a recovery proof.
        let inbox = self.external_inbox.as_ref().ok_or(Status::ExternalPending)?;
        if !self.external_sealed {
            inbox.close();
            if self.external_producer.is_none() || inbox.has_pending() {
                return Err(Status::ExternalPending);
            }
            drop(self.external_producer.take());
            self.external_sealed = true;
        }
        if inbox.has_pending() {
            return Err(Status::ExternalPending);
        }
        self.settle_source_hosts(deadline)?;
        if !self.workspace.as_ref().unwrap().wrap_cleanup_ready()
            || self.wrap.as_ref().is_some_and(|host| !host.document_shutdown_ready())
        {
            return Err(Status::WrapPending);
        }
        drop(self.wrap.take());
        self.close_search()?;
        self.finish_search(deadline)?;
        if let Err(error) = self.workspace.as_mut().unwrap().rearm_unstarted_frontend() {
            self.rearm_error.get_or_insert(error);
            return Err(Status::UnstartedRearmRefused);
        }
        if Instant::now() >= deadline { Err(Status::RecoveryTimedOut) } else { Ok(()) }
    }
    fn settle_source_hosts(&mut self, deadline: Instant) -> Result<(), Status> {
        let mut source_failure = None;
        drop(self.font_size.take());
        drop(self.chrome_font_size.take());
        if let Some(font) = &mut self.font {
            match font.settle_until(deadline) {
                Ok(()) => drop(self.font.take()),
                Err(reason) => {
                    source_failure.get_or_insert(Status::EditorFont(reason));
                }
            }
        }
        if let Some(font) = &mut self.chrome_font {
            match font.settle_until(deadline) {
                Ok(()) => drop(self.chrome_font.take()),
                Err(reason) => {
                    source_failure.get_or_insert(Status::ChromeFont(reason));
                }
            }
        }
        if let Some(host) = &mut self.markdown {
            match host.settle_until(deadline) {
                Ok(()) => drop(self.markdown.take()),
                Err(reason) => {
                    source_failure.get_or_insert(Status::Markdown(reason));
                }
            }
        }
        if let Some(host) = &mut self.logs {
            match host.settle_until(deadline) {
                Ok(()) => drop(self.logs.take()),
                Err(reason) => {
                    source_failure.get_or_insert(Status::Logs(reason));
                }
            }
        }
        if let Some(host) = &mut self.syntax {
            match host.settle_until(deadline) {
                Ok(()) => drop(self.syntax.take()),
                Err(reason) => {
                    source_failure.get_or_insert(Status::Syntax(reason));
                }
            }
        }
        if let Some(reason) = source_failure { Err(reason) } else { Ok(()) }
    }
    fn settle_off_ui(&mut self) -> Result<(), Status> {
        let deadline = *self.deadline.get_or_insert_with(|| Instant::now() + Duration::from_secs(35));
        if std::mem::take(&mut self.retry_log_retirement) {
            self.logs
                .as_mut()
                .ok_or(Status::ReceiptIdentity)?
                .retry_retirement_until(deadline)
                .map_err(Status::LogRetirementRetry)?;
        }
        self.settle_source_hosts(deadline)?;
        if std::mem::take(&mut self.retry_binding)
            && let Some(refusal) = self.bind_refusal.take()
        {
            let DocumentShutdownBindRefusal { workspace, transport, reason } = refusal;
            if self.binding_error.is_none() {
                self.binding_error = Some(reason);
            }
            self.workspace = Some(workspace);
            self.live.transport = Some(transport);
        }
        if self.bind_refusal.is_some() {
            return Err(Status::BindRefused);
        }
        if self.no_document_worker {
            return self.finish_search(deadline);
        }
        if let Some(workspace) = &self.workspace
            && !workspace.wrap_cleanup_ready()
        {
            return Err(Status::WrapPending);
        }
        if self.wrap.as_ref().is_some_and(|host| !host.document_shutdown_ready()) {
            return Err(Status::WrapPending);
        }
        // Prepared layout/cursor owners retire OFF UI before document roots.
        drop(self.wrap.take());
        drop(self.live.update.take());
        if self.bound.is_none() {
            if self.live.transport.is_none()
                && (self.live.start_failure.is_some()
                    || self.frontend_error.is_some()
                    || self.event_loop_error.is_some())
            {
                // Startup failed before the document lane existed. Returning a
                // typed failure must still return the original Workspace.
                return Err(Status::NotStarted);
            }
            let Some(workspace) = self.workspace.take() else {
                return Err(Status::Live);
            };
            let Some(transport) = self.live.transport.take() else {
                // No document worker ever started. Even here, release search
                // inputs before physical document cleanup and retain the exact
                // Workspace if closing/proving those inputs refuses.
                self.workspace = Some(workspace);
                self.close_search()?;
                if let Some(worker) = &self.search_worker {
                    worker
                        .wait_inputs_released(deadline.saturating_duration_since(Instant::now()))
                        .map_err(Status::SearchInputs)?;
                }
                drop(self.workspace.take());
                self.no_document_worker = true;
                return self.finish_search(deadline);
            };
            match workspace.into_document_shutdown(transport) {
                std::ops::ControlFlow::Continue(bundle) => self.bound = Some(bundle),
                std::ops::ControlFlow::Break(refusal) => {
                    self.bind_refusal = Some(refusal);
                    return Err(Status::BindRefused);
                }
            }
        }
        if !self.search_bound {
            let binding = self
                .search
                .as_ref()
                .and_then(|session| session.source_release_key())
                .map_or(DocumentSearchBinding::NoSearchWorker, DocumentSearchBinding::SearchWorker);
            if self.bound.as_mut().unwrap().bind_search_sources(binding).is_err() {
                return Err(Status::ReceiptIdentity);
            }
            self.search_bound = true;
        }
        // Only a new explicit retry invocation requests producer work. This
        // reuses its original slot/control cell and the original deadline.
        if let Some(status) = self.bound.as_ref().map(WorkspaceDocumentShutdown::status)
            && status.failure.is_some()
            && !status.retry_pending
            && !status.retry_acknowledgement_required
        {
            self.live.wrap_retry = None;
        }
        if self.live.wrap_retry.is_none()
            && self.live.wrap_retry_refusal.is_none()
            && matches!(
                self.status,
                Status::Document(DocumentShutdownReason::Transport(
                    kwil_view_model::document_worker::DocumentResultSettlementReason::WrapSettlementFailed
                        | kwil_view_model::document_worker::DocumentResultSettlementReason::WrapThreadExitPending
                        | kwil_view_model::document_worker::DocumentResultSettlementReason::WrapNativeExit(_)
                ))
            )
        {
            self.live.wrap_retry = Some(
                self.bound
                    .as_mut()
                    .unwrap()
                    .request_wrap_retry(deadline.saturating_duration_since(Instant::now()))
                    .map_err(|reason| Status::Document(DocumentShutdownReason::Transport(reason)))?,
            );
        }
        loop {
            let status = self.bound.as_mut().unwrap().settle_until(deadline).map_err(Status::Document)?;
            match status {
                DocumentShutdownStatus::ReplacementReceiptPending => {
                    let receipt = self.bound.as_ref().unwrap().replacement_receipt().ok_or(Status::ReceiptIdentity)?;
                    let slot = self
                        .replacements
                        .iter_mut()
                        .find(|slot| **slot == Some((receipt.key, receipt.operation)))
                        .ok_or(Status::ReceiptIdentity)?;
                    if !self.bound.as_mut().unwrap().acknowledge_replacement_receipt(receipt) {
                        return Err(Status::ReceiptIdentity);
                    }
                    *slot = None;
                    // No SearchPanel sync, source snapshot or search submission.
                }
                DocumentShutdownStatus::SourcesReleased => {
                    self.close_search()?;
                    if !self.search_inputs_released {
                        if let Some(worker) = &self.search_worker {
                            let proof = worker
                                .wait_inputs_released(deadline.saturating_duration_since(Instant::now()))
                                .map_err(Status::SearchInputs)?;
                            if let Err(refusal) = self.bound.as_mut().unwrap().acknowledge_search_inputs_released(proof)
                            {
                                self.search_proof_refusal = Some(refusal);
                                return Err(Status::ReceiptIdentity);
                            }
                        }
                        self.search_inputs_released = true;
                    }
                }
                DocumentShutdownStatus::Complete => {
                    self.document_complete = true;
                    self.finish_search(deadline)?;
                    drop(self.bound.take());
                    return Ok(());
                }
                DocumentShutdownStatus::WrapRetryReceiptPending => {
                    let token = self.live.wrap_retry.take().ok_or(Status::ReceiptIdentity)?;
                    if let Err(refusal) = self.bound.as_mut().unwrap().acknowledge_wrap_retry(token) {
                        let reason = refusal.reason;
                        self.live.wrap_retry_refusal = Some(refusal);
                        return Err(Status::Document(DocumentShutdownReason::Transport(reason)));
                    }
                }
                DocumentShutdownStatus::PrepublicationResetPending => {
                    let token = self.live.reset.take().ok_or(Status::ReceiptIdentity)?;
                    if let Err(refusal) = self.bound.as_mut().unwrap().acknowledge_prepublication_reset(token) {
                        let reason = refusal.reason;
                        self.live.reset_refusal = Some(refusal);
                        return Err(Status::Document(DocumentShutdownReason::Transport(reason)));
                    }
                }
            }
        }
    }
    fn close_search(&mut self) -> Result<(), Status> {
        if self.search_refusal.is_some() {
            return Err(Status::SearchCloseRefused);
        }
        if self.search_worker.is_none()
            && let Some(session) = &mut self.search
        {
            match self.panel.as_mut().unwrap().take_shutdown(session) {
                Ok(worker) => self.search_worker = Some(worker),
                Err(owner) => {
                    self.search_refusal = Some(owner);
                    return Err(Status::SearchCloseRefused);
                }
            }
        }
        // Their rejected Snapshot owners were transferred to the worker above.
        drop(self.panel.take());
        drop(self.search.take());
        Ok(())
    }
    fn finish_search(&mut self, deadline: Instant) -> Result<(), Status> {
        if let Some(worker) = &mut self.search_worker {
            worker.settle_until(deadline).map_err(Status::Search)?;
            if Instant::now() >= deadline {
                return Err(Status::Search(kwil_view_model::search_session::WorkerSettlementReason::TimedOut));
            }
        }
        drop(self.search_worker.take());
        Ok(())
    }
}
pub(super) type Job = Box<dyn FnOnce() -> Result<(), String>>;

pub(super) fn drain<const N: usize>(jobs: [Option<Job>; N]) -> Result<(), String> {
    let mut errors = Vec::new();
    for job in jobs.into_iter().flatten() {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => errors.push(error),
            Err(_) => errors.push("shutdown lane panicked".into()),
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};
    #[test]
    fn unstarted_fallback_retains_accepted_external_input_in_original_inbox() {
        use kwil_core_workspace::instance::{OpenFile, OpenFiles};
        let (app, _events) = crate::search_ui::host_dispatch_tests::host("original");
        let requested = std::env::temp_dir().join("kwil-unstarted-accepted-input.txt");
        app.external_open.as_ref().unwrap()(OpenFiles {
            files: vec![OpenFile { path: requested.clone(), line: Some(7), column: Some(3) }],
        })
        .unwrap();
        let super::super::App {
            document: mut failure,
            view,
            wrap,
            markdown,
            logs,
            highlighting,
            search_panel,
            search,
            external_inbox,
            external_open,
            ..
        } = app;
        failure.frontend_error = Some(super::super::GuiError::Unavailable("isolated unavailable GUI".into()));
        failure.attach(Owners {
            view,
            wrap,
            markdown,
            logs,
            highlighting,
            search_panel,
            search,
            external_inbox,
            external_open,
        });
        let address = (&*failure) as *const Failure;
        let deadline = Instant::now() + Duration::from_secs(5);
        let failure = match failure.recover_unstarted(deadline) {
            std::ops::ControlFlow::Break(failure) => failure,
            std::ops::ControlFlow::Continue(_) => panic!("accepted inbox input must prevent fallback"),
        };
        assert_eq!((&*failure) as *const Failure, address, "same preallocated failure box");
        assert_eq!(failure.status(), Status::ExternalPending);
        assert!(failure.external_requests_pending());
        assert!(failure.external_producer.is_some(), "pending refusal retains the original producer");
        let rejected = OpenFiles { files: vec![OpenFile { path: requested.clone(), line: None, column: None }] };
        let error = failure.external_producer.as_ref().unwrap()(rejected).unwrap_err();
        assert!(matches!(error, std::sync::mpsc::TrySendError::Disconnected(_)));
        // The test becomes the explicit owner of this accepted scalar request;
        // production recovery never drains or silently discards it.
        let requests = failure.external_inbox.as_ref().unwrap().drain();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].files[0].path, requested);
        assert_eq!((requests[0].files[0].line, requests[0].files[0].column), (Some(7), Some(3)));
        // This adversarial fixture borrow-called its producer, so it does not
        // qualify for the production unexposed proof on any later retry. The
        // test explicitly owns cleanup of its never-started source off UI.
        std::thread::spawn(move || drop(failure)).join().unwrap();
    }
    #[test]
    fn exposed_producer_racing_in_actual_wake_cannot_prove_unstarted_recovery() {
        use kwil_core_workspace::instance::{OpenFile, OpenFiles};
        let (mut app, _events) = crate::search_ui::host_dispatch_tests::host("original");
        let (entered, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = std::sync::Mutex::new(release_rx);
        let (producer, inbox) = kwil_view_model::document_worker::external_open_channel(move || {
            entered.send(()).map_err(std::io::Error::other)?;
            release_rx.lock().unwrap().recv_timeout(Duration::from_secs(5)).map_err(std::io::Error::other)
        });
        // Replace the untouched test pair together, then expose the sole real
        // producer exactly by take. No producer remains in the failure owner.
        app.external_inbox = inbox;
        app.external_open = Some(producer);
        let producer = app.external_open.take().unwrap();
        let requested = std::env::temp_dir().join("kwil-unstarted-racing-input.txt");
        let path = requested.clone();
        let sender = std::thread::spawn(move || {
            producer(OpenFiles { files: vec![OpenFile { path, line: Some(9), column: None }] }).unwrap();
            producer
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let super::super::App {
            document: mut failure,
            view,
            wrap,
            markdown,
            logs,
            highlighting,
            search_panel,
            search,
            external_inbox,
            external_open,
            ..
        } = app;
        failure.frontend_error = Some(super::super::GuiError::Unavailable("isolated unavailable GUI".into()));
        failure.attach(Owners {
            view,
            wrap,
            markdown,
            logs,
            highlighting,
            search_panel,
            search,
            external_inbox,
            external_open,
        });
        let address = (&*failure) as *const Failure;
        let refused = failure.recover_unstarted(Instant::now() + Duration::from_secs(5));
        // Release and join the actual caller even if the assertion later fails.
        release.send(()).unwrap();
        let producer = sender.join().unwrap();
        let failure = match refused {
            std::ops::ControlFlow::Break(failure) => failure,
            std::ops::ControlFlow::Continue(_) => panic!("exposed/racing producer must prevent fallback"),
        };
        assert_eq!((&*failure) as *const Failure, address);
        assert_eq!(failure.status(), Status::ExternalPending);
        assert!(failure.external_producer.is_none());
        assert!(failure.external_requests_pending());
        let requests = failure.external_inbox.as_ref().unwrap().drain();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].files[0].path, requested);
        assert_eq!(requests[0].files[0].line, Some(9));
        // Explicit fixture ownership, not a recovered Workspace or settlement.
        std::thread::spawn(move || {
            drop(producer);
            drop(failure);
        })
        .join()
        .unwrap();
    }
    #[test]
    fn marker_and_preparation_errors_do_not_skip_settings_or_retirement() {
        let order = Rc::new(RefCell::new(Vec::new()));
        let jobs: [Option<Job>; 4] = std::array::from_fn(|index| {
            let order = order.clone();
            Some(Box::new(move || {
                order.borrow_mut().push(index);
                match index {
                    0 => Err("marker write failed; retained temporary".into()),
                    1 => panic!("preparation panic"),
                    2 => Ok(()), // accepted settings B must still settle
                    _ => Err("retirement error".into()),
                }
            }) as Job)
        });
        let error = drain(jobs).unwrap_err();
        assert_eq!(*order.borrow(), vec![0, 1, 2, 3]);
        for message in ["marker write failed", "retained temporary", "panicked", "retirement error"] {
            assert!(error.contains(message));
        }
    }
}
