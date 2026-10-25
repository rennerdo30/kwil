//! Sealed document publication. The transport owns this value until the
//! existing producer retires it OFF UI. No source-bearing callback is used.
use super::*;
#[cfg(test)]
use crate::Document;
use crate::document_worker::{SaveResumeKey, result_transport::PrepublicationResetKind};
use crate::editor::save::{SaveCompletionState, SaveRetentionRefusal};
use crate::{Damage, Editor, View, ViewBodyPolicy};
use kwil_core_workspace::session::{MAX_TABS, Session};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DocumentCommitPhase {
    Received,
    Prepared,
    Installed,
    FollowUp,
    Complete,
}

/// This is the partial value of the ORIGINAL transport slot, not a second lane.
/// Every displaced owner stays here through unwind and successful publication.
pub(crate) struct DocumentCommit {
    result: Option<DocumentResult>,
    info: ResultInfo,
    phase: DocumentCommitPhase,
    installed_fields: u16,
    shell: Option<View>,
    prepared: Option<PreparedMerge>,
    saved: Option<SaveCompletionState>,
    save_target: Option<SaveResumeKey>,
    save_preparation_stale: bool,
    // Fixed destinations for EXISTING Editor residuals. These are inside the
    // one charged transport slot, not a retirement queue or a second lane.
    retired_saves: Vec<Option<SaveCompletionState>>,
    retired_save_charge: Option<crate::visual_rows::MetadataReservation>,
    charge: Option<crate::visual_rows::MetadataReservation>,
    update_charge: Option<crate::visual_rows::MetadataReservation>,
    next_live_charge: Option<crate::visual_rows::MetadataReservation>,
    retired_live_charge: Option<crate::visual_rows::MetadataReservation>,
    replacement: Option<ExactReplacementReceipt>,
    barrier: Option<crate::document_worker::result_transport::DocumentBarrierReceipt>,
    discard_checked: bool,
    damage: Damage,
    #[cfg(test)]
    fault: CommitFault,
    #[cfg(test)]
    last_refusal: Option<CommitRefusal>,
}

/// Test diagnostics retain scalar classification only. The actual nested error
/// remains owned by the existing transport and is borrowed through its Inspector.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitRefusal {
    Admission,
    Preparation { kind: io::ErrorKind, stale_identity: bool },
    ForeignWorkspace,
    Identity,
    PartialCommit,
    Receipt(crate::editor::receipts::ReceiptDeliveryRefusal),
}
#[cfg(test)]
impl CommitRefusal {
    fn from_error(error: &WorkspaceDocumentAcceptError) -> Self {
        match error {
            WorkspaceDocumentAcceptError::Admission(_) => Self::Admission,
            WorkspaceDocumentAcceptError::Preparation(error) => {
                Self::Preparation { kind: error.kind(), stale_identity: stale_save_preparation(error) }
            }
            WorkspaceDocumentAcceptError::ForeignWorkspace => Self::ForeignWorkspace,
            WorkspaceDocumentAcceptError::Identity => Self::Identity,
            WorkspaceDocumentAcceptError::PartialCommit => Self::PartialCommit,
            WorkspaceDocumentAcceptError::Receipt(error) => Self::Receipt(*error),
        }
    }
}

struct PreparedMerge {
    session: Session,
    views: HashMap<DocumentId, View>,
    mapping: HashMap<DocumentId, DocumentId>,
    moves: Vec<(DocumentId, DocumentId)>,
    recovery_next: usize,
    moved_next: usize,
    deferred: bool,
    defer_kind: Option<RecoveryDeferKind>,
    deferred_shell: Option<Box<crate::document_worker::PreparedWorkspace>>,
    detached: Vec<DocumentId>,
    detached_originals: Vec<kwil_core_text::TextFile>,
}

impl DocumentCommit {
    pub(crate) fn new(result: DocumentResult) -> Self {
        Self {
            result: Some(result),
            info: ResultInfo {
                kind: ResultKind::Capacity,
                document: None,
                revision: None,
                operation: None,
                generation: None,
                delivery: None,
            },
            phase: DocumentCommitPhase::Received,
            installed_fields: 0,
            shell: None,
            prepared: None,
            saved: None,
            save_target: None,
            save_preparation_stale: false,
            retired_saves: Vec::new(),
            retired_save_charge: None,
            charge: None,
            update_charge: None,
            next_live_charge: None,
            retired_live_charge: None,
            replacement: None,
            barrier: None,
            discard_checked: false,
            damage: Damage::default(),
            #[cfg(test)]
            fault: CommitFault::None,
            #[cfg(test)]
            last_refusal: None,
        }
    }
    pub(crate) fn info(&self) -> ResultInfo {
        self.info
    }
    pub(crate) fn phase(&self) -> DocumentCommitPhase {
        if self.saved.as_ref().is_some_and(SaveCompletionState::publication_started) {
            self.phase.max(DocumentCommitPhase::Installed)
        } else {
            self.phase
        }
    }
    pub(crate) fn is_complete(&self) -> bool {
        self.phase == DocumentCommitPhase::Complete
    }
    pub(crate) fn installed_fields(&self) -> u16 {
        self.installed_fields
            | if self.saved.as_ref().is_some_and(SaveCompletionState::publication_started) { 16 } else { 0 }
    }
    pub(crate) fn pending(&self) -> Option<&DocumentResult> {
        self.result.as_ref()
    }
    /// Only an unpromoted, untouched carrier may return its original result.
    pub(crate) fn take_received_result(&mut self) -> Option<DocumentResult> {
        if self.phase() != DocumentCommitPhase::Received || self.installed_fields() != 0 {
            return None;
        }
        self.result.take()
    }
    pub(crate) fn prepublication_reset_kind(&self) -> Option<PrepublicationResetKind> {
        (self.phase == DocumentCommitPhase::Prepared
            && self.installed_fields() == 0
            && self.save_target.is_some()
            && self.save_preparation_stale
            && self.saved.as_ref().is_some_and(|state| !state.publication_started()))
        .then_some(PrepublicationResetKind::SavePreparation)
    }
    /// Original producer ONLY, under its RAII slot guard and outside all locks.
    /// The outer publication phase, target, saved owner and charges stay put.
    pub(crate) fn reset_prepublication_on_producer(
        &mut self,
        kind: PrepublicationResetKind,
    ) -> Result<(), SaveRetentionRefusal> {
        if self.phase() >= DocumentCommitPhase::Installed || self.installed_fields() != 0 {
            return Err(SaveRetentionRefusal::PublicationStarted);
        }
        if self.prepublication_reset_kind() != Some(kind) {
            return Err(SaveRetentionRefusal::EmptyInput);
        }
        #[cfg(test)]
        self.fault(CommitFault::ResetPrepublicationPanic);
        self.saved.as_mut().ok_or(SaveRetentionRefusal::EmptyInput)?.reset_preparation_for_retry()?;
        self.save_preparation_stale = false;
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn set_fault(&mut self, fault: CommitFault) {
        self.fault = fault;
    }
    #[cfg(test)]
    pub(crate) fn last_refusal(&self) -> Option<CommitRefusal> {
        self.last_refusal
    }
    fn advance(&mut self, phase: DocumentCommitPhase) {
        debug_assert!(phase >= self.phase);
        self.phase = phase;
    }
    // A partial commit is never an implicit retry authority. Transport records
    // CommitPanicked before another acceptance can be attempted.
    fn fault(&self, _at: CommitFault) {
        #[cfg(test)]
        if self.fault == _at {
            panic!("document commit controlled fault: {_at:?}");
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CommitFault {
    #[default]
    None,
    BeforePublication,
    AfterInstallation,
    #[cfg(test)]
    AfterSavePreparation,
    #[cfg(test)]
    ResetPrepublicationPanic,
}

pub(crate) fn result_info(result: &DocumentResult) -> ResultInfo {
    use DocumentResult::*;
    let mut info = ResultInfo {
        kind: ResultKind::Capacity,
        document: None,
        revision: None,
        operation: None,
        generation: None,
        delivery: None,
    };
    match result {
        WrapSettled(_) => info.kind = ResultKind::Wrap,
        ResumeSaveCompletion { key } => {
            info.kind = ResultKind::Saved;
            info.document = Some(key.document);
            info.revision = Some(key.revision);
            info.generation = Some(key.generation);
        }
        BinaryOffer(offer) | BinaryRefused { offer, .. } => {
            info.kind = ResultKind::Binary;
            info.document = Some(offer.ticket.document());
            info.revision = Some(offer.ticket.revision());
            info.generation = Some(offer.ticket.generation());
        }
        CheckedPreview { ticket, .. } | CheckedRawPreview { ticket, .. } | CheckedOpened { ticket, .. } => {
            info.kind = if matches!(result, CheckedOpened { .. }) { ResultKind::Opened } else { ResultKind::Preview };
            info.document = Some(ticket.document());
            info.revision = Some(ticket.revision());
            info.generation = Some(ticket.generation());
        }
        RawReopened { document, generation, .. } | ReopenValidated { document, generation, .. } => {
            info.kind = ResultKind::RawReopened;
            info.document = Some(*document);
            info.generation = Some(*generation);
        }
        HexPage { document, request, .. } => {
            info.kind = ResultKind::HexPage;
            info.document = Some(*document);
            info.revision = Some(request.source.revision);
            info.generation = Some(request.generation);
        }
        HexEdited { document, completion, .. } => {
            info.kind = ResultKind::HexEdited;
            info.document = Some(*document);
            info.operation = Some(completion.operation);
        }
        Replaced { key, operation, .. } => {
            info.kind = ResultKind::Replaced;
            info.document = Some(DocumentId(key.document_id));
            info.revision = Some(key.revision);
            info.operation = Some(*operation);
        }
        Prepared(_) => info.kind = ResultKind::Prepared,
        Persisted(_) => info.kind = ResultKind::Persisted,
        Recovered { document, revision, .. }
        | Indexed { document, revision, .. }
        | Opened { document, revision, .. }
        | Preview { document, revision, .. }
        | RawPreview { document, revision, .. }
        | Saved { document, revision, .. }
        | JournalQueued { document, revision, .. }
        | JournalDurable { document, revision, .. } => {
            info.document = Some(*document);
            info.revision = Some(*revision);
            info.kind = match result {
                Recovered { .. } => ResultKind::Recovered,
                Indexed { .. } => ResultKind::Indexed,
                Opened { .. } => ResultKind::Opened,
                Preview { .. } | RawPreview { .. } => ResultKind::Preview,
                Saved { .. } => ResultKind::Saved,
                _ => ResultKind::Journal,
            };
        }
        JournalClosed { document, .. } => {
            info.kind = ResultKind::Journal;
            info.document = Some(*document);
        }
        Stopped(_) => info.kind = ResultKind::Stopped,
        SourceBarrier(_) => info.kind = ResultKind::Barrier,
        _ => {}
    }
    info
}

impl Workspace {
    #[cfg(test)]
    pub(crate) fn set_document_commit_fault_for_test(&mut self, fault: CommitFault) {
        self.files.next_document_commit_fault = fault;
    }

    pub(crate) fn validate_prepublication_reset_ack(
        &self,
        delivery: crate::document_worker::result_transport::DocumentDeliveryId,
        commit: &DocumentCommit,
    ) -> Result<(), WorkspaceDocumentAcceptError> {
        if !self.files.worker.as_ref().is_some_and(|worker| worker.matches_delivery(delivery)) {
            return Err(WorkspaceDocumentAcceptError::ForeignWorkspace);
        }
        if commit.info.delivery != Some(delivery)
            || commit.phase() != DocumentCommitPhase::Prepared
            || commit.installed_fields() != 0
            || commit.save_preparation_stale
            || commit.saved.as_ref().is_none_or(SaveCompletionState::publication_started)
        {
            return Err(WorkspaceDocumentAcceptError::PartialCommit);
        }
        let key = commit.save_target.ok_or(WorkspaceDocumentAcceptError::Identity)?;
        if commit.info.document != Some(key.document) || commit.info.revision != Some(key.revision) {
            return Err(WorkspaceDocumentAcceptError::Identity);
        }
        self.validate_save_target(key)
    }

    fn validate_save_target(&self, key: SaveResumeKey) -> Result<(), WorkspaceDocumentAcceptError> {
        let flight = self.files.saves.get(&key.document).ok_or(WorkspaceDocumentAcceptError::Identity)?;
        let editor =
            self.views.get(&key.document).and_then(View::editor).ok_or(WorkspaceDocumentAcceptError::Identity)?;
        if flight.resume_key(key.document) != key
            || flight.resume != super::super::SaveResumePhase::Transport
            || editor.document().source_epoch != key.source_epoch
        {
            return Err(WorkspaceDocumentAcceptError::Identity);
        }
        Ok(())
    }
    pub(crate) fn commit_document_delivery(
        &mut self,
        delivery: crate::document_worker::result_transport::DocumentDeliveryId,
        slot: &mut crate::document_worker::result_transport::DocumentSlot,
        budget: &AllocationBudget,
        permit: Option<&crate::document_worker::DocumentContinuationPermit>,
    ) -> Result<DocumentCommitOutcome, DocumentCommitError> {
        if !self.files.worker.as_ref().is_some_and(|worker| worker.matches_delivery(delivery)) {
            return Err(WorkspaceDocumentAcceptError::ForeignWorkspace);
        }
        // The original owner stays in the same inline slot. Only its scalar
        // promotion tag changes, after the exact worker check above.
        let commit = slot.promote();
        commit.info = commit.info.with_delivery(delivery);
        #[cfg(test)]
        {
            if commit.phase == DocumentCommitPhase::Received {
                commit.fault = std::mem::take(&mut self.files.next_document_commit_fault);
            }
        }
        let outcome = self.commit_document(commit, budget, permit);
        #[cfg(test)]
        {
            commit.last_refusal = outcome.as_ref().err().map(CommitRefusal::from_error);
        }
        outcome.map(DocumentCommitOutcome::Completed)
    }

    pub(crate) fn commit_document(
        &mut self,
        commit: &mut DocumentCommit,
        budget: &AllocationBudget,
        permit: Option<&crate::document_worker::DocumentContinuationPermit>,
    ) -> Result<WorkspaceDocumentUpdate, WorkspaceDocumentAcceptError> {
        let resume_save = commit.phase == DocumentCommitPhase::Prepared && commit.saved.is_some();
        if commit.phase != DocumentCommitPhase::Received && !resume_save {
            return Err(WorkspaceDocumentAcceptError::PartialCommit);
        }
        if commit.phase == DocumentCommitPhase::Received {
            // Promotion has already installed this residual in guarded storage.
            // Inspect only scalar tags/identities after that ownership boundary.
            let delivery = commit.info.delivery;
            commit.info = result_info(commit.result.as_ref().ok_or(WorkspaceDocumentAcceptError::PartialCommit)?);
            commit.info.delivery = delivery;
        }
        if self
            .views
            .values()
            .chain(self.closed_views.values())
            .any(|view| view.editor().is_some_and(Editor::has_completed_save_completion))
            && commit.retired_saves.is_empty()
        {
            const MAX: usize = kwil_core_workspace::session::MAX_TABS + kwil_core_workspace::session::MAX_CLOSED;
            let bytes =
                MAX * size_of::<Option<SaveCompletionState>>() + 2 * size_of::<Vec<Option<SaveCompletionState>>>();
            commit.retired_save_charge =
                Some(budget.reserve_metadata(bytes).map_err(WorkspaceDocumentAcceptError::Admission)?);
            commit.retired_saves.try_reserve_exact(MAX).map_err(allocation)?;
            if commit.retired_saves.capacity() > MAX {
                return Err(WorkspaceDocumentAcceptError::Admission(WrapError::Admission(
                    "completed save retirement capacity",
                )));
            }
            commit.retired_saves.resize_with(MAX, || None);
        }
        for view in self.views.values_mut().chain(self.closed_views.values_mut()) {
            let Some(editor) = view.editor_mut() else { continue };
            if editor.has_completed_save_completion() {
                let destination = commit
                    .retired_saves
                    .iter_mut()
                    .find(|slot| slot.is_none())
                    .ok_or(WorkspaceDocumentAcceptError::PartialCommit)?;
                editor
                    .take_completed_save_completion(destination)
                    .map_err(|_| WorkspaceDocumentAcceptError::PartialCommit)?;
            }
        }
        // Unknown observer compatibility cannot consume an exact production
        // receipt. Refuse before touching the completion or any source owner.
        for view in self.views.values_mut().chain(self.closed_views.values_mut()) {
            if let Some(editor) = view.editor_mut() {
                match editor.flush_edit_receipt_sealed(permit) {
                    Ok(()) | Err(crate::editor::receipts::ReceiptDeliveryRefusal::Full) => {}
                    Err(error) => return Err(WorkspaceDocumentAcceptError::Receipt(error)),
                }
            }
        }
        if matches!(commit.result, Some(DocumentResult::WrapSettled(_))) {
            // Wrap has its own sealed exact-owner gate, below.
            return self.commit_document_wrap(commit, budget);
        }
        if resume_save {
            self.finish_guarded_document_save(commit, budget)?;
        } else {
            self.prepare_document_commit(commit, budget)?;
            commit.advance(DocumentCommitPhase::Prepared);
            commit.fault(CommitFault::BeforePublication);
            self.install_document_commit(commit, budget)?;
        }
        commit.advance(DocumentCommitPhase::Installed);
        commit.fault(CommitFault::AfterInstallation);
        // From this point onward, fallible reads/formatting/observers borrow
        // owners already installed in Workspace or retained in this commit.
        commit.advance(DocumentCommitPhase::FollowUp);
        self.follow_document_commit(commit, permit)?;
        commit.advance(DocumentCommitPhase::Complete);
        Ok(WorkspaceDocumentUpdate {
            damage: std::mem::take(&mut commit.damage),
            replacement: commit.replacement.take(),
            barrier: commit.barrier.take(),
            _charge: commit.update_charge.take().unwrap(),
        })
    }

    fn prepare_document_commit(
        &mut self,
        commit: &mut DocumentCommit,
        budget: &AllocationBudget,
    ) -> Result<(), WorkspaceDocumentAcceptError> {
        // Includes stack transfer overlap, empty Editor/View shells, bounded
        // recovery plan, path/remap tables and retained damage. Payload roots
        // retain the worker's existing source accounting.
        let count = match commit.result.as_ref().unwrap() {
            DocumentResult::Prepared(Ok(prepared)) => prepared.session.tabs().len() + self.views.len(),
            _ => 1,
        };
        if count > MAX_TABS * 2 {
            return Err(WorkspaceDocumentAcceptError::Identity);
        }
        if self.views.values().any(|view| view.rows() > 4096) {
            return Err(WorkspaceDocumentAcceptError::Admission(WrapError::Admission("document viewport metadata")));
        }
        let mut bytes = count
            .checked_mul(size_of::<View>() + size_of::<Editor>() + size_of::<crate::Cursor>() + 128)
            .and_then(|n| n.checked_add(2 * size_of::<DocumentCommit>() + 128 * 1024))
            .ok_or(WorkspaceDocumentAcceptError::Identity)?;
        if let Some(settings) = &self.editor_settings {
            let settings_bytes =
                settings_metadata_bytes(settings).map_err(WorkspaceDocumentAcceptError::Preparation)?;
            bytes = bytes
                .checked_add(settings_bytes.checked_mul(count).ok_or(WorkspaceDocumentAcceptError::Identity)?)
                .ok_or(WorkspaceDocumentAcceptError::Identity)?;
        }
        if let Some(DocumentResult::Prepared(Ok(prepared))) = &commit.result {
            // Actual source-free metadata is known before constructing shells;
            // the former unconditional 18 MiB estimate could starve a second
            // normal preparation under old+new live-charge overlap.
            let metadata = session_metadata(&self.session)
                .and_then(|a| session_metadata(&prepared.session).and_then(|b| a.checked_add(b)))
                .and_then(|n| n.checked_mul(4))
                .ok_or(WorkspaceDocumentAcceptError::Identity)?;
            bytes = bytes.checked_add(metadata).ok_or(WorkspaceDocumentAcceptError::Identity)?;
            bytes = bytes.checked_add(512 * size_of::<View>()).ok_or(WorkspaceDocumentAcceptError::Identity)?;
        }
        if commit.charge.is_none() {
            commit.charge = Some(budget.reserve_metadata(bytes).map_err(WorkspaceDocumentAcceptError::Admission)?);
        }
        if commit.update_charge.is_none() {
            commit.update_charge = Some(
                budget
                    .reserve_metadata(size_of::<WorkspaceDocumentUpdate>() + 8192 * size_of::<usize>())
                    .map_err(WorkspaceDocumentAcceptError::Admission)?,
            );
        }
        if commit.next_live_charge.is_none() {
            let live = self
                .document_live_metadata_bytes(commit.result.as_ref(), None)
                .map_err(WorkspaceDocumentAcceptError::Preparation)?;
            commit.next_live_charge =
                Some(budget.reserve_metadata(live).map_err(WorkspaceDocumentAcceptError::Admission)?);
        }
        match commit.result.as_ref().unwrap() {
            DocumentResult::Replaced { key, operation, .. } => {
                let id = DocumentId(key.document_id);
                if !self.files.loans.get(&id).is_some_and(|loan| loan.key == *key && loan.operation == *operation)
                    || self.views.get(&id).is_none_or(|view| view.editor.is_some())
                {
                    return Err(WorkspaceDocumentAcceptError::Identity);
                }
                self.files.replacement_journals.try_reserve(1).map_err(allocation)?;
                commit.shell = Some(View::new(None));
            }
            DocumentResult::RawReopened { document, .. }
            | DocumentResult::HexPage { document, .. }
            | DocumentResult::HexEdited { document, .. } => {
                use super::super::workspace_hex::Loan;
                let identity = match commit.result.as_ref().unwrap() {
                    DocumentResult::RawReopened { generation, .. } => {
                        matches!(self.hex.loans.get(document),Some(Loan::Reopen {generation:current,..}) if current==generation)
                    }
                    DocumentResult::HexPage { request, .. } => {
                        matches!(self.hex.loans.get(document),Some(Loan::Page(current)) if current==request)
                    }
                    DocumentResult::HexEdited { completion, .. } => {
                        matches!(self.hex.loans.get(document),Some(Loan::Edit {source,operation,input_epoch}) if *source==completion.before&&*operation==completion.operation&&*input_epoch==completion.input_epoch)
                    }
                    _ => false,
                };
                if self.views.get(document).is_none_or(|view| view.editor.is_some()) || !identity {
                    return Err(WorkspaceDocumentAcceptError::Identity);
                }
                self.files.replacement_journals.try_reserve(1).map_err(allocation)?;
                commit.shell = Some(View::new(None));
            }
            DocumentResult::Recovered { document, result: Ok(_), .. }
            | DocumentResult::Indexed { document, result: Ok(_), .. } => {
                if self.document_publication_target(commit.info).is_some() {
                    commit.shell = Some(View::new(None));
                }
                let _ = document;
            }
            DocumentResult::CheckedOpened { ticket, result: Ok(Some(_)) } => {
                if !self.binary_current(*ticket) {
                    if self.binary.flights.get(&ticket.document()).is_some_and(|flight| flight.ticket == *ticket) {
                        commit.discard_checked = true;
                        return Ok(());
                    }
                    return Err(WorkspaceDocumentAcceptError::Identity);
                }
                if self.views.get(&ticket.document()).is_none_or(|view| view.editor.is_some())
                    || self.files.loans.contains_key(&ticket.document())
                    || self.hex.editor_away(ticket.document())
                {
                    return Err(WorkspaceDocumentAcceptError::Identity);
                }
                commit.shell = Some(View::new(None));
            }
            DocumentResult::Opened { result: Ok(_), .. } | DocumentResult::Preview { .. } => {
                let mut shell = View::new(None);
                shell.editor = None;
                commit.shell = Some(shell);
            }
            DocumentResult::Saved { result: Ok(_), .. } | DocumentResult::ResumeSaveCompletion { .. } => {
                let mut shell = View::new(None);
                shell.editor = None;
                commit.shell = Some(shell);
            }
            DocumentResult::Prepared(Ok(_)) => self.prepare_recovery_merge(commit)?,
            _ => {}
        }
        Ok(())
    }

    fn document_publication_target(&self, info: ResultInfo) -> Option<DocumentId> {
        let id = info.document?;
        let view = self.views.get(&id)?;
        if view.editor.is_some() || self.files.loans.contains_key(&id) || self.hex.editor_away(id) {
            return None;
        }
        match info.kind {
            ResultKind::Recovered => {
                self.session.tabs().iter().any(|tab| tab.id == id && Some(tab.revision) == info.revision).then_some(id)
            }
            _ => (info.revision == Some(0)).then_some(id),
        }
    }

    fn install_document_commit(
        &mut self,
        commit: &mut DocumentCommit,
        budget: &AllocationBudget,
    ) -> Result<(), WorkspaceDocumentAcceptError> {
        // Publish the full live reservation with the destination BEFORE the
        // first source field moves. A later panic leaves Workspace charged;
        // the old reservation follows displaced owners in this same residual.
        if commit.next_live_charge.is_some() {
            std::mem::swap(&mut self.files.live_document_metadata, &mut commit.next_live_charge);
            commit.retired_live_charge = commit.next_live_charge.take();
        }
        let intended = self.intended_wrap_publication();
        if matches!(
            commit.info.kind,
            ResultKind::Recovered | ResultKind::Indexed | ResultKind::Opened | ResultKind::Preview
        ) {
            self.wrap.path_changed();
        }
        #[cfg(test)]
        let panic_after_field = commit.fault == CommitFault::AfterInstallation;
        match commit.result.as_mut().unwrap() {
            DocumentResult::SourceBarrier(receipt) => {
                commit.barrier = Some(*receipt);
            }
            DocumentResult::Replaced { key, operation, editor, result } => {
                let id = DocumentId(key.document_id);
                // The destination exists and is admitted before either swap.
                let shell = commit.shell.as_mut().unwrap();
                std::mem::swap(&mut **editor, shell.editor.as_mut().unwrap());
                std::mem::swap(&mut self.views.get_mut(&id).unwrap().editor, &mut shell.editor);
                commit.phase = DocumentCommitPhase::Installed;
                commit.installed_fields |= 1;
                #[cfg(test)]
                if panic_after_field {
                    panic!("controlled panic after first source field");
                }
                commit.replacement =
                    Some(ExactReplacementReceipt { key: *key, operation: *operation, succeeded: result.is_ok() });
                self.files.loans.remove(&id);
                self.input_epoch = self.input_epoch.wrapping_add(1);
                let installed = self.views.get_mut(&id).unwrap();
                let editor = installed.editor.as_mut().unwrap();
                if let Some(settings) = &self.editor_settings {
                    editor.settings = settings.clone();
                }
                let sequence = editor.next_edit_sequence().saturating_sub(1);
                if editor.document().revision() != key.revision {
                    self.files.replacement_journals.insert(id, sequence);
                }
                super::super::prepare_view_publication(installed, intended, false);
                installed.set_notice(match result {
                    Ok(_) => "Replacement complete".into(),
                    Err(error) => format!("Replacement failed: {error}"),
                });
                installed.refresh_after_edit(Damage { full: true, status: true, ..Default::default() });
                if self.files.hot_exit {
                    self.files.queue.push_back(DocumentRequest::FlushJournals);
                }
                commit.damage = Damage { full: self.session.active() == Some(id), status: true, ..Default::default() };
            }
            DocumentResult::Indexed { document, result: Ok(source), .. }
            | DocumentResult::Recovered { document, result: Ok(source), .. } => {
                let id = *document;
                if let Some(shell) = &mut commit.shell {
                    std::mem::swap(&mut **source, &mut shell.editor.as_mut().unwrap().document);
                    std::mem::swap(&mut self.views.get_mut(&id).unwrap().editor, &mut shell.editor);
                    commit.phase = DocumentCommitPhase::Installed;
                    commit.installed_fields |= 1;
                    #[cfg(test)]
                    if panic_after_field {
                        panic!("controlled panic after first source field");
                    }
                    std::mem::swap(&mut self.views.get_mut(&id).unwrap().file, &mut shell.file);
                    commit.installed_fields |= 2;
                    self.finish_installed_document(id, commit.info.kind, intended);
                    commit.damage =
                        Damage { full: self.session.active() == Some(id), status: true, ..Default::default() };
                }
            }
            DocumentResult::CheckedOpened { ticket, result: Ok(Some(source)) } => {
                let id = ticket.document();
                if commit.discard_checked {
                    self.binary.flights.remove(&id);
                    self.hex_forget_binary_open(id);
                    commit.phase = DocumentCommitPhase::Installed;
                    return Ok(());
                }
                let shell = commit.shell.as_mut().unwrap();
                std::mem::swap(&mut **source, &mut shell.editor.as_mut().unwrap().document);
                std::mem::swap(&mut self.views.get_mut(&id).unwrap().editor, &mut shell.editor);
                commit.phase = DocumentCommitPhase::Installed;
                commit.installed_fields |= 1;
                #[cfg(test)]
                if panic_after_field {
                    panic!("controlled panic after first source field");
                }
                std::mem::swap(&mut self.views.get_mut(&id).unwrap().file, &mut shell.file);
                commit.installed_fields |= 2;
                self.binary.flights.remove(&id);
                self.finish_installed_document(id, ResultKind::Indexed, intended);
                commit.damage = Damage { full: self.session.active() == Some(id), status: true, ..Default::default() };
            }
            DocumentResult::RawReopened { document, editor, .. }
            | DocumentResult::HexPage { document, editor, .. }
            | DocumentResult::HexEdited { document, editor, .. } => {
                let shell = commit.shell.as_mut().unwrap();
                std::mem::swap(&mut **editor, shell.editor.as_mut().unwrap());
                std::mem::swap(&mut self.views.get_mut(document).unwrap().editor, &mut shell.editor);
                commit.phase = DocumentCommitPhase::Installed;
                commit.installed_fields |= 1;
                #[cfg(test)]
                if panic_after_field {
                    panic!("controlled panic after first source field");
                }
                if matches!(commit.info.kind, ResultKind::RawReopened) {
                    std::mem::swap(&mut self.views.get_mut(document).unwrap().file, &mut shell.file);
                    commit.installed_fields |= 2;
                }
                commit.phase = DocumentCommitPhase::Installed;
                // Specialized finishers borrow the already restored editor.
            }
            DocumentResult::Preview { document, revision, path, file, preview } => {
                if self.hex_text_preview_pending(*document) {
                    self.hex_text_preview(*document, *revision, std::mem::take(&mut preview.text));
                } else if self.document_publication_target(commit.info).is_some() {
                    let view = self.views.get_mut(document).unwrap();
                    view.path = Some(path.clone());
                    // Original handle remains in this commit through unwind.
                    std::mem::swap(&mut view.file, &mut commit.shell.as_mut().unwrap().file);
                    view.file = Some(file.clone());
                    view.preview = Some(std::mem::take(&mut preview.text));
                    commit.phase = DocumentCommitPhase::Installed;
                    super::super::prepare_view_publication(view, intended, true);
                    view.set_rows(view.rows());
                    view.set_notice("Preparing decoded document…".into());
                    self.files.failed_opens.remove(document);
                    commit.damage =
                        Damage { full: self.session.active() == Some(*document), status: true, ..Default::default() };
                }
            }
            DocumentResult::Opened { document, result: Ok((path, file)), .. } => {
                if self.document_publication_target(commit.info).is_some() {
                    let view = self.views.get_mut(document).unwrap();
                    view.path = Some(path.clone());
                    let shell = commit.shell.as_mut().unwrap();
                    std::mem::swap(&mut view.file, &mut shell.file);
                    view.file = Some(file.clone());
                    commit.phase = DocumentCommitPhase::Installed;
                    super::super::prepare_view_publication(view, intended, false);
                    view.set_rows(view.rows());
                    self.files.failed_opens.remove(document);
                    commit.damage =
                        Damage { full: self.session.active() == Some(*document), status: true, ..Default::default() };
                }
            }
            DocumentResult::Prepared(Ok(_)) => self.publish_recovery_merge(commit)?,
            DocumentResult::ResumeSaveCompletion { key } => {
                let key = *key;
                let flight = self.files.saves.get(&key.document).ok_or(WorkspaceDocumentAcceptError::Identity)?;
                if flight.resume_key(key.document) != key
                    || flight.resume != super::super::SaveResumePhase::Queued
                    || flight.preparation_error.is_none()
                    || self
                        .views
                        .get(&key.document)
                        .and_then(View::editor)
                        .is_none_or(|editor| editor.document().source_epoch != key.source_epoch)
                {
                    return Err(WorkspaceDocumentAcceptError::Identity);
                }
                // Destination shell and metadata are admitted BEFORE the swap.
                // The scalar carrier stays in this same residual throughout.
                self.views
                    .get_mut(&key.document)
                    .and_then(View::editor_mut)
                    .unwrap()
                    .take_unpublished_save_completion(&mut commit.saved)
                    .map_err(|_| WorkspaceDocumentAcceptError::PartialCommit)?;
                commit.save_target = Some(key);
                commit.save_preparation_stale = true;
                let flight = self.files.saves.get_mut(&key.document).unwrap();
                flight.resume = super::super::SaveResumePhase::Transport;
                let error = flight.preparation_error.take().ok_or(WorkspaceDocumentAcceptError::Identity)?;
                return Err(WorkspaceDocumentAcceptError::Preparation(error));
            }
            DocumentResult::Saved { document, revision, result: Ok(saved) } => {
                if saved.revision != *revision {
                    return Err(WorkspaceDocumentAcceptError::Identity);
                }
                if !self.files.saves.get(document).is_some_and(|save| save.revision == *revision) {
                    return Ok(());
                }
                let editor =
                    self.views.get(document).and_then(View::editor).ok_or(WorkspaceDocumentAcceptError::Identity)?;
                let flight = self.files.saves.get_mut(document).unwrap();
                if flight.source_epoch != editor.document().source_epoch
                    || flight.path.as_ref() != Some(&saved.path)
                    || flight.resume != super::super::SaveResumePhase::Local
                {
                    return Err(WorkspaceDocumentAcceptError::Identity);
                }
                commit.save_target = Some(flight.resume_key(*document));
                flight.resume = super::super::SaveResumePhase::Transport;
                // Constructor is nonallocating. The residual is installed before
                // any save preparation/source read or observer can run.
                let owned = commit.result.take().unwrap();
                let DocumentResult::Saved { result: Ok(saved), .. } = owned else { unreachable!() };
                commit.saved = Some(SaveCompletionState::new(saved));
                self.finish_guarded_document_save(commit, budget)?;
            }
            DocumentResult::Saved { document, revision, result: Err(error) } => {
                if self.files.saves.get(document).is_some_and(|save| save.revision == *revision)
                    && let Some(editor) = self.views.get_mut(document).and_then(View::editor_mut)
                {
                    let current = editor.document().revision() == *revision;
                    commit.phase = DocumentCommitPhase::Installed;
                    editor.fail_save_completion();
                    // The ORIGINAL error stays in guarded storage. The normal
                    // status path receives a source-free formatting result.
                    let notice = io::Error::new(error.kind(), error.to_string());
                    commit.damage = self.complete_save(*document, current, Err(notice));
                }
            }
            _ => {
                // These variants contain no source owners. Mark before entry so
                // an unwind cannot replay their accepted state transitions.
                commit.phase = DocumentCommitPhase::Installed;
                match self.apply_document_result_with_receipts(commit.result.take().unwrap(), false) {
                    ControlFlow::Continue(damage) => commit.damage = damage,
                    ControlFlow::Break(result) => {
                        commit.result = Some(result);
                        return Err(WorkspaceDocumentAcceptError::PartialCommit);
                    }
                }
            }
        }
        Ok(())
    }

    fn finish_guarded_document_save(
        &mut self,
        commit: &mut DocumentCommit,
        budget: &AllocationBudget,
    ) -> Result<(), WorkspaceDocumentAcceptError> {
        let key = commit.save_target.ok_or(WorkspaceDocumentAcceptError::Identity)?;
        self.validate_save_target(key)?;
        if commit.save_preparation_stale {
            // Only the original producer can remove stale cached preparation.
            return Err(WorkspaceDocumentAcceptError::PartialCommit);
        }
        let editor = self
            .views
            .get_mut(&key.document)
            .and_then(View::editor_mut)
            .ok_or(WorkspaceDocumentAcceptError::Identity)?;
        let state = commit.saved.as_mut().unwrap();
        state.admit_retention(budget).map_err(WorkspaceDocumentAcceptError::Preparation)?;
        if state.publication_started() {
            return Err(WorkspaceDocumentAcceptError::PartialCommit);
        }
        let mut current = false;
        let completed = editor.prepare_save_completion(state, budget).and_then(|()| {
            #[cfg(test)]
            if commit.fault == CommitFault::AfterSavePreparation {
                commit.fault = CommitFault::None;
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "controlled prepared save pause"));
            }
            // Recompute immediately before successful model publication, rather
            // than reusing the revision observed before a stale/deferred retry.
            current = editor.document().revision() == key.revision;
            editor.commit_save_completion(state)
        });
        let completed = match completed {
            Ok(completed) => completed,
            Err(error) => {
                commit.save_preparation_stale =
                    stale_save_preparation(&error) && !commit.saved.as_ref().unwrap().publication_started();
                if error
                    .get_ref()
                    .and_then(|error| error.downcast_ref::<crate::editor::receipts::ReceiptDeliveryRefusal>())
                    == Some(&crate::editor::receipts::ReceiptDeliveryRefusal::Full)
                {
                    editor
                        .retain_save_completion(&mut commit.saved)
                        .map_err(|_| WorkspaceDocumentAcceptError::PartialCommit)?;
                    self.files.saves.get_mut(&key.document).unwrap().resume = super::super::SaveResumePhase::Local;
                    commit.phase = DocumentCommitPhase::Installed;
                    commit.damage = Damage { status: true, ..Default::default() };
                    return Ok(());
                }
                return Err(WorkspaceDocumentAcceptError::Preparation(error));
            }
        };
        commit.phase = DocumentCommitPhase::Installed;
        commit.installed_fields |= 16;
        if let Some(shell) = &mut commit.shell {
            std::mem::swap(&mut self.views.get_mut(&commit.info.document.unwrap()).unwrap().file, &mut shell.file);
        }
        commit.damage = self.complete_save(key.document, current, Ok(completed));
        Ok(())
    }

    fn finish_installed_document(&mut self, id: DocumentId, kind: ResultKind, intended: bool) {
        let metadata = self.session.tabs().iter().find(|tab| tab.id == id).cloned();
        let view = self.views.get_mut(&id).unwrap();
        let editor = view.editor.as_mut().unwrap();
        if let Some(settings) = &self.editor_settings {
            editor.settings = settings.clone();
        }
        let raw = editor.document().representation() == crate::hex_session::Representation::RawBytes;
        if kind == ResultKind::Recovered
            && let Some(tab) = &metadata
        {
            if editor.document().path != tab.path || (editor.document().path.is_none() && tab.recovery_path.is_some()) {
                view.path.clone_from(&editor.document().path);
                super::super::sync_tab_path(self.session.tab_mut(id).unwrap(), &editor.document().path);
                view.notice = super::super::recovery_notice(tab);
            }
            if !raw {
                let len = editor.document().hex_source(id.0).len;
                let _ = editor.set_cursors(
                    vec![crate::Cursor {
                        head: tab.cursor.min(len),
                        anchor: tab.anchor.min(len),
                        desired_column: None,
                    }],
                    0,
                );
            }
        }
        if raw {
            view.file = editor.document().original.clone();
        }
        super::super::prepare_view_publication(view, intended, false);
        if let Some(tab) = metadata.filter(|_| kind == ResultKind::Recovered) {
            if view.body_policy() == ViewBodyPolicy::LogicalLines {
                view.editor.as_mut().unwrap().set_viewport(tab.first_visible_line, view.rows);
                view.top = tab.first_visible_line;
            }
            if !raw {
                view.set_horizontal_window(tab.first_byte_column, 64 << 10);
                view.refresh_after_edit(Damage { full: true, ..Default::default() });
            }
        } else {
            if view.body_policy() == ViewBodyPolicy::LogicalLines {
                view.editor.as_mut().unwrap().set_viewport(view.top, view.rows);
            }
            if !raw {
                view.index_ready();
            }
            view.set_notice(String::new());
        }
        self.hex_indexed(id);
    }

    fn follow_document_commit(
        &mut self,
        commit: &mut DocumentCommit,
        permit: Option<&crate::document_worker::DocumentContinuationPermit>,
    ) -> Result<(), WorkspaceDocumentAcceptError> {
        if let Some(result) = commit.result.as_mut() {
            match result {
                DocumentResult::RawReopened { document, generation, result, .. } => {
                    commit.damage = self.hex_reopened_installed(
                        *document,
                        *generation,
                        std::mem::replace(result, Ok(crate::hex_session::OpenOutcome::Ready)),
                    );
                }
                DocumentResult::HexPage { document, request, result, .. } => {
                    commit.damage = self.hex_page_installed(
                        *document,
                        *request,
                        std::mem::replace(result, Err(io::Error::other("completion installed"))),
                    );
                }
                DocumentResult::HexEdited { document, completion, .. } => {
                    // Completion carries only bounded page/scalar state; the
                    // Editor remains in Workspace throughout its UI update.
                    let placeholder = crate::hex_session::EditCompletion {
                        before: completion.before,
                        after: completion.after,
                        operation: completion.operation,
                        input_epoch: completion.input_epoch,
                        result: Ok(()),
                    };
                    let completion = std::mem::replace(completion, placeholder);
                    commit.damage = self.hex_edit_installed(*document, completion);
                }
                _ => {}
            }
        }
        for view in self.views.values_mut().chain(self.closed_views.values_mut()) {
            if let Some(editor) = view.editor_mut() {
                // Full keeps the receipt inside the installed Editor. It is a
                // pending continuation, never permission to replay this commit.
                if let Err(error) = editor.flush_edit_receipt_sealed(permit)
                    && !matches!(error, crate::editor::receipts::ReceiptDeliveryRefusal::Full)
                {
                    return Err(WorkspaceDocumentAcceptError::Receipt(error));
                }
            }
        }
        if matches!(
            commit.info.kind,
            ResultKind::Prepared
                | ResultKind::Recovered
                | ResultKind::Indexed
                | ResultKind::Replaced
                | ResultKind::Saved
                | ResultKind::Opened
                | ResultKind::Preview
        ) {
            self.bind_raw_sessions();
            if matches!(
                commit.info.kind,
                ResultKind::Prepared
                    | ResultKind::Recovered
                    | ResultKind::Indexed
                    | ResultKind::Replaced
                    | ResultKind::Saved
            ) {
                self.checkpoint_session();
            }
            self.refresh_save_destinations();
            self.check_hot_exit();
            self.pump_files();
            self.pump_hex();
        }
        if !self.files.document_shutdown
            && matches!(commit.info.kind, ResultKind::RawReopened | ResultKind::HexPage | ResultKind::HexEdited)
        {
            // These specialized finishers restored the exact Editor above.
            // Retry the latest viewport, not the completed/stale page, through
            // the existing live lane. Checkpoint policy stays with its original
            // owners; bound shutdown uses only its private continuation permit.
            self.pump_files();
            self.pump_hex();
        }
        // The update keeps the actual bounded row allocation and its charge;
        // preserve copy/exposed-row semantics instead of widening damage.
        if commit.damage.rows.capacity() > 8192 {
            return Err(WorkspaceDocumentAcceptError::Admission(WrapError::Admission("document damage metadata")));
        }
        Ok(())
    }
}

impl Workspace {
    fn commit_document_wrap(
        &mut self,
        commit: &mut DocumentCommit,
        budget: &AllocationBudget,
    ) -> Result<WorkspaceDocumentUpdate, WorkspaceDocumentAcceptError> {
        // A failed settlement keeps its exact nested ticket in the original
        // transport slot. Its explicit retry is transport-owned, never replayed.
        if !matches!(commit.result, Some(DocumentResult::WrapSettled(Ok(_)))) {
            return Err(WorkspaceDocumentAcceptError::Identity);
        }
        let charge = budget
            .reserve_metadata(size_of::<WorkspaceDocumentUpdate>())
            .map_err(WorkspaceDocumentAcceptError::Admission)?;
        let Some(DocumentResult::WrapSettled(result)) = commit.result.take() else { unreachable!() };
        match self.accept_wrap_settlement(result) {
            ControlFlow::Continue(damage) => {
                commit.advance(DocumentCommitPhase::Complete);
                Ok(WorkspaceDocumentUpdate { damage, replacement: None, barrier: None, _charge: charge })
            }
            ControlFlow::Break(result) => {
                commit.result = Some(DocumentResult::WrapSettled(result));
                Err(WorkspaceDocumentAcceptError::Identity)
            }
        }
    }

    fn prepare_recovery_merge(&mut self, commit: &mut DocumentCommit) -> Result<(), WorkspaceDocumentAcceptError> {
        self.sync_all();
        let Some(DocumentResult::Prepared(Ok(prepared))) = &commit.result else { unreachable!() };
        let additional = self
            .session
            .tabs()
            .iter()
            .filter(|tab| {
                let view = &self.views[&tab.id];
                let dirty = view.editor().is_some_and(|e| e.document().is_dirty());
                let blank =
                    view.path.is_none() && view.editor().is_some_and(|e| e.document().snapshot().is_empty()) && !dirty;
                (!blank
                    || self.files.saves.contains_key(&tab.id)
                    || self.files.prompt.as_ref().is_some_and(|p| p.document == tab.id))
                    && (dirty
                        || view.path.is_none()
                        || !prepared.session.tabs().iter().any(|t| t.path.is_some() && t.path == view.path))
            })
            .count();
        let structural = !self.closed_views.is_empty() || prepared.session.tabs().len() + additional > MAX_TABS;
        let deferred = !self.hex.loans.is_empty() || structural;
        let mut plan = PreparedMerge {
            session: prepared.session.clone(),
            views: HashMap::new(),
            mapping: HashMap::new(),
            moves: Vec::new(),
            recovery_next: 0,
            moved_next: 0,
            deferred,
            defer_kind: if structural {
                Some(RecoveryDeferKind::Structural)
            } else if deferred {
                Some(RecoveryDeferKind::HexLoans)
            } else {
                None
            },
            deferred_shell: None,
            detached: Vec::new(),
            detached_originals: Vec::new(),
        };
        if deferred {
            plan.deferred_shell = Some(Box::new(crate::document_worker::PreparedWorkspace {
                session: Session::default(),
                documents: Vec::new(),
            }));
            commit.prepared = Some(plan);
            return Ok(());
        }
        plan.views.try_reserve((prepared.session.tabs().len() + additional).max(1)).map_err(allocation)?;
        plan.mapping.try_reserve(self.views.len()).map_err(allocation)?;
        plan.moves.try_reserve_exact(self.views.len()).map_err(allocation)?;
        plan.detached.try_reserve_exact(prepared.documents.len()).map_err(allocation)?;
        plan.detached_originals.try_reserve_exact(prepared.documents.len()).map_err(allocation)?;
        let intended = self.intended_wrap_publication();
        for (id, document) in &prepared.documents {
            let tab = plan
                .session
                .tabs()
                .iter()
                .find(|tab| tab.id == *id)
                .cloned()
                .ok_or(WorkspaceDocumentAcceptError::Identity)?;
            let mut shell = View::new(None);
            shell.path = tab.path.clone();
            if let Ok(document) = document {
                if document.path != tab.path || (document.path.is_none() && tab.recovery_path.is_some()) {
                    shell.path.clone_from(&document.path);
                    super::super::sync_tab_path(plan.session.tab_mut(*id).unwrap(), &document.path);
                    shell.notice = super::super::recovery_notice(&tab);
                }
                if let Some(settings) = &self.editor_settings {
                    shell.editor.as_mut().unwrap().settings = settings.clone();
                }
            } else {
                shell.editor = None;
            }
            super::super::prepare_view_publication(&mut shell, intended, false);
            plan.views.insert(*id, shell);
        }
        let old_active = self.active_id();
        let mut active = None;
        for tab in self.session.tabs() {
            let Some(view) = self.views.get(&tab.id) else { continue };
            let dirty = view.editor().is_some_and(|e| e.document().is_dirty());
            let blank =
                view.path.is_none() && view.editor().is_some_and(|e| e.document().snapshot().is_empty()) && !dirty;
            if blank
                && !plan.views.is_empty()
                && !self.files.saves.contains_key(&tab.id)
                && self.files.prompt.as_ref().is_none_or(|p| p.document != tab.id)
            {
                continue;
            }
            if let Some(existing) =
                plan.session.tabs().iter().find(|t| t.path.is_some() && t.path == view.path).map(|t| t.id)
            {
                if !dirty {
                    plan.mapping.insert(tab.id, existing);
                    if tab.id == old_active {
                        active = Some(existing);
                    }
                    // Navigation is performed only after restored owners are
                    // installed; the old view stays guarded in plan.views.
                    continue;
                }
                let metadata = plan.session.tab_mut(existing).unwrap();
                super::super::sync_tab_path(metadata, &None);
                let recovered = plan.views.get_mut(&existing).unwrap();
                recovered.path = None;
                recovered.notice = super::super::recovery_notice(metadata);
                plan.detached.push(existing);
            }
            let id = plan.session.open(view.path.clone()).map_err(WorkspaceDocumentAcceptError::Preparation)?;
            plan.mapping.insert(tab.id, id);
            plan.moves.push((tab.id, id));
            if tab.id == old_active {
                active = Some(id);
            }
            let mut shell = View::new(None);
            shell.editor = None;
            plan.views.insert(id, shell);
        }
        if let Some(id) = active {
            plan.session.activate(id);
        }
        if plan.views.is_empty() {
            let id = plan.session.open(None).map_err(WorkspaceDocumentAcceptError::Preparation)?;
            plan.views.insert(id, View::new(None));
        }
        // All map/session destinations exist before a source moves.
        commit.prepared = Some(plan);
        Ok(())
    }

    fn publish_recovery_merge(&mut self, commit: &mut DocumentCommit) -> Result<(), WorkspaceDocumentAcceptError> {
        let plan = commit.prepared.as_mut().unwrap();
        let Some(DocumentResult::Prepared(Ok(prepared))) = commit.result.as_mut() else { unreachable!() };
        if plan.deferred {
            std::mem::swap(prepared, &mut **plan.deferred_shell.as_mut().unwrap());
            std::mem::swap(&mut self.files.deferred_recovery, &mut plan.deferred_shell);
            commit.phase = DocumentCommitPhase::Installed;
            self.files.preparing = false;
            self.files.storage_ready = false;
            self.files.deferred_recovery_kind = plan.defer_kind;
            self.set_notice("Recovery retained; restore when active work permits".into());
            self.queue_initial_indexes();
            return Ok(());
        }
        // Each recovery root moves directly from guarded result into an already
        // constructed, guarded destination. No fallible operation lies between.
        for (id, document) in prepared.documents.iter_mut().skip(plan.recovery_next) {
            if let Ok(document) = document {
                std::mem::swap(document, &mut plan.views.get_mut(id).unwrap().editor.as_mut().unwrap().document);
            }
            plan.recovery_next += 1;
        }
        for &(old, new) in plan.moves.iter().skip(plan.moved_next) {
            std::mem::swap(self.views.get_mut(&old).unwrap(), plan.views.get_mut(&new).unwrap());
            plan.moved_next += 1;
        }
        std::mem::swap(&mut self.session, &mut plan.session);
        commit.installed_fields |= 8;
        std::mem::swap(&mut self.views, &mut plan.views);
        commit.installed_fields |= 4;
        commit.phase = DocumentCommitPhase::Installed;
        self.files.preparing = false;
        self.files.storage_ready = true;
        self.files.deferred_recovery_kind = None;
        self.wrap.path_changed();
        self.hex = super::super::HexFlow::default();
        self.input_epoch = self.input_epoch.wrapping_add(1);
        // From here every source owner is in Workspace or the guarded retired
        // view map. Validation/navigation/settings may unwind without loss.
        let recovered_ids: Vec<_> = prepared.documents.iter().map(|(id, _)| *id).collect();
        for id in recovered_ids {
            if self.views.get(&id).is_some_and(|view| view.editor.is_some()) {
                if plan.detached.contains(&id) {
                    let editor = self.views.get_mut(&id).unwrap().editor.as_mut().unwrap();
                    editor.document.path = None;
                    if let Some(original) = editor.document.original.take() {
                        plan.detached_originals.push(original);
                    }
                    editor.document.mark_unsaved();
                }
                self.finish_installed_document(id, ResultKind::Recovered, self.intended_wrap_publication());
            } else if let Some((_, Err(error))) = prepared.documents.iter().find(|(document, _)| *document == id) {
                self.views.get_mut(&id).unwrap().set_notice(format!("Recovery failed: {error}; journal retained"));
            }
        }
        for request in &mut self.files.queue {
            match request {
                DocumentRequest::OpenChecked { ticket, .. } => {
                    if let Some(id) = plan.mapping.get(&ticket.document) {
                        ticket.document = *id;
                    }
                }
                DocumentRequest::Open { document, .. }
                | DocumentRequest::Index { document, .. }
                | DocumentRequest::Save { document, .. } => {
                    if let Some(id) = plan.mapping.get(document) {
                        *document = *id;
                    }
                }
                _ => {}
            }
        }
        // Duplicate checked startup sources retire through their existing lane.
        for request in &mut self.files.queue {
            if let DocumentRequest::OpenChecked { ticket, file, .. } = request
                && self.views.get(&ticket.document).is_some_and(|view| view.editor.is_some())
                && let Some(file) = file.take()
            {
                *request = DocumentRequest::RetireSource { file };
            }
        }
        self.binary.flights = std::mem::take(&mut self.binary.flights)
            .into_iter()
            .filter_map(|(old, mut flight)| {
                let id = plan.mapping.get(&old).copied()?;
                if self.views.get(&id).is_none_or(|view| view.editor.is_some()) {
                    return None;
                }
                flight.ticket.document = id;
                flight.return_to = flight.return_to.and_then(|old| plan.mapping.get(&old).copied());
                Some((id, flight))
            })
            .collect();
        self.files.saves = std::mem::take(&mut self.files.saves)
            .into_iter()
            .map(|(id, save)| (*plan.mapping.get(&id).unwrap_or(&id), save))
            .collect();
        self.files.failed_opens = std::mem::take(&mut self.files.failed_opens)
            .into_iter()
            .filter_map(|id| plan.mapping.get(&id).copied())
            .collect();
        if let Some(prompt) = &mut self.files.prompt
            && let Some(id) = plan.mapping.get(&prompt.document)
        {
            prompt.document = *id;
        }
        for id in self.files.close.iter_mut().chain(self.files.save_all.iter_mut()) {
            if let Some(mapped) = plan.mapping.get(id) {
                *id = *mapped;
            }
        }
        // Replay only metadata navigation, after all source fields are owned.
        for (old, view) in &plan.views {
            if let Some((line, column, extend)) = view.pending_goto
                && let Some(id) = plan.mapping.get(old).copied()
                && !self.hex_owns_document(id)
                && let Some(recovered) = self.views.get_mut(&id)
            {
                let _ = recovered.execute(
                    "navigation.goToLine",
                    &crate::CommandArgs { line: Some(line), column: Some(column), extend, ..Default::default() },
                    1,
                );
            }
        }
        self.queue_initial_indexes();
        commit.damage = Damage { full: true, status: true, ..Default::default() };
        Ok(())
    }
}

fn allocation(error: std::collections::TryReserveError) -> WorkspaceDocumentAcceptError {
    WorkspaceDocumentAcceptError::Preparation(io::Error::other(error))
}

/// Actual metadata capacities; independently charged source roots, edit recipes
/// and decoded/display payload storage keep their original accounting.
pub(crate) fn settings_metadata_bytes(settings: &kwil_core_commands::Settings) -> io::Result<usize> {
    let mut bytes = size_of::<kwil_core_commands::Settings>();
    fn add(total: &mut usize, value: usize) -> io::Result<()> {
        *total = total.checked_add(value).ok_or_else(|| io::Error::other("document metadata overflow"))?;
        Ok(())
    }
    fn strings(total: &mut usize, values: &Vec<String>) -> io::Result<()> {
        add(
            total,
            values
                .capacity()
                .checked_mul(size_of::<String>())
                .ok_or_else(|| io::Error::other("settings metadata overflow"))?,
        )?;
        for value in values {
            add(total, value.capacity())?;
        }
        Ok(())
    }
    add(&mut bytes, settings.font.family.capacity())?;
    add(&mut bytes, settings.theme.name.capacity())?;
    for map in [&settings.theme.colors, &settings.log.highlight_rules] {
        for (key, value) in map {
            add(&mut bytes, 1024)?;
            add(&mut bytes, key.capacity())?;
            add(&mut bytes, value.capacity())?;
        }
    }
    strings(&mut bytes, &settings.search.include)?;
    strings(&mut bytes, &settings.search.exclude)?;
    add(
        &mut bytes,
        settings
            .bindings
            .capacity()
            .checked_mul(size_of::<kwil_core_commands::Binding>())
            .ok_or_else(|| io::Error::other("binding metadata overflow"))?,
    )?;
    for binding in &settings.bindings {
        add(&mut bytes, binding.keys.capacity())?;
        add(&mut bytes, binding.command.capacity())?;
    }
    for (key, formatter) in &settings.formatters {
        add(&mut bytes, 1024 + 16 * size_of::<kwil_core_commands::settings::Formatter>())?;
        add(&mut bytes, key.capacity())?;
        add(&mut bytes, formatter.executable.capacity())?;
        strings(&mut bytes, &formatter.args)?;
    }
    Ok(bytes)
}

fn session_metadata(session: &Session) -> Option<usize> {
    use kwil_core_workspace::session::{MAX_CLOSED, MAX_RECENT, Tab};
    let mut bytes = size_of::<Session>()
        + MAX_TABS * size_of::<Tab>()
        + MAX_CLOSED * size_of::<Tab>()
        + MAX_RECENT * size_of::<PathBuf>();
    for tab in session.tabs().iter().chain(session.closed().iter()) {
        for path in [tab.path.as_ref(), tab.recovery_path.as_ref()].into_iter().flatten() {
            bytes = bytes.checked_add(path.capacity())?;
        }
    }
    for path in session.recent() {
        bytes = bytes.checked_add(path.capacity())?;
    }
    Some(bytes)
}

impl Workspace {
    /// ROOT also calls this before retained metadata growth outside sealed
    /// acceptance, using its existing destination/future-settings admission.
    pub(crate) fn document_live_metadata_bytes(
        &self,
        incoming: Option<&DocumentResult>,
        future_settings: Option<&kwil_core_commands::Settings>,
    ) -> io::Result<usize> {
        fn overflow() -> io::Error {
            io::Error::other("live document metadata overflow")
        }
        fn add(total: &mut usize, value: usize) -> io::Result<()> {
            *total = total.checked_add(value).ok_or_else(overflow)?;
            Ok(())
        }
        fn map(total: &mut usize, capacity: usize, entry: usize) -> io::Result<()> {
            // Retain old buckets plus one bounded growth generation. Small new
            // maps additionally use the fixed per-document control allowance.
            add(total, capacity.checked_mul(4).and_then(|n| n.checked_mul(entry + 16)).ok_or_else(overflow)?)
        }
        fn view(total: &mut usize, view: &View) -> io::Result<()> {
            add(total, view.notice.capacity())?;
            if let Some(path) = &view.path {
                add(total, path.capacity())?;
            }
            add(total, view.lines.capacity().checked_mul(size_of::<Vec<u8>>()).ok_or_else(overflow)?)?;
            add(total, view.line_offsets.capacity().checked_mul(size_of::<u64>()).ok_or_else(overflow)?)?;
            add(
                total,
                view.line_sources.capacity().checked_mul(size_of::<std::ops::Range<u64>>()).ok_or_else(overflow)?,
            )?;
            // One source publication can rebuild these bounded cache headers.
            add(
                total,
                view.rows()
                    .checked_mul(size_of::<Vec<u8>>() + size_of::<u64>() + size_of::<std::ops::Range<u64>>())
                    .ok_or_else(overflow)?,
            )?;
            if let Some(editor) = view.editor() {
                add(total, editor.document_metadata_bytes()?)?;
                add(total, settings_metadata_bytes(&editor.settings)?)?;
            }
            Ok(())
        }
        // A worker may still hold the previous accepted Persist snapshot while
        // its completion is guarded. Session's established 9 MiB hard maximum
        // is the finite future allowance for that otherwise invisible metadata.
        // It authorizes no source reads or source payload allocations.
        let mut bytes = 9 * 1024 * 1024 + size_of::<Workspace>() + size_of::<super::super::FileFlow>();
        let max_records = kwil_core_workspace::session::MAX_TABS + kwil_core_workspace::session::MAX_CLOSED;
        add(
            &mut bytes,
            2 * max_records
                * (size_of::<super::super::SaveFlight>()
                    + size_of::<super::super::EditorLoan>()
                    + size_of::<String>()
                    + 8 * size_of::<(DocumentId, u64, u64)>()
                    + 128),
        )?;
        map(&mut bytes, self.views.capacity(), size_of::<(DocumentId, View)>())?;
        map(&mut bytes, self.closed_views.capacity(), size_of::<(DocumentId, View)>())?;
        for view_state in self.views.values().chain(self.closed_views.values()) {
            view(&mut bytes, view_state)?;
        }
        let session = session_metadata(&self.session).ok_or_else(overflow)?;
        add(&mut bytes, session.checked_mul(3).ok_or_else(overflow)?)?;
        add(&mut bytes, self.files.queue.capacity().checked_mul(size_of::<DocumentRequest>()).ok_or_else(overflow)?)?;
        map(&mut bytes, self.files.saves.capacity(), size_of::<(DocumentId, super::super::SaveFlight)>())?;
        map(&mut bytes, self.files.loans.capacity(), size_of::<(DocumentId, super::super::EditorLoan)>())?;
        map(&mut bytes, self.files.observed.capacity(), size_of::<(DocumentId, (u64, u64))>())?;
        map(&mut bytes, self.files.durable.capacity(), size_of::<(DocumentId, (u64, u64))>())?;
        map(&mut bytes, self.files.replacement_journals.capacity(), size_of::<(DocumentId, u64)>())?;
        map(&mut bytes, self.files.failed_opens.capacity(), size_of::<DocumentId>())?;
        map(&mut bytes, self.files.seeded.capacity(), size_of::<DocumentId>())?;
        map(&mut bytes, self.files.journal_failed.capacity(), size_of::<(DocumentId, String)>())?;
        map(&mut bytes, self.binary.flights.capacity(), size_of::<(DocumentId, super::super::BinaryFlight)>())?;
        add(
            &mut bytes,
            self.binary.retiring.capacity().checked_mul(size_of::<(DocumentId, Box<Editor>)>()).ok_or_else(overflow)?,
        )?;
        for flight in self.binary.flights.values() {
            add(&mut bytes, flight.path.capacity())?;
        }
        for (_, editor) in &self.binary.retiring {
            add(&mut bytes, editor.document_metadata_bytes()?)?;
            add(&mut bytes, settings_metadata_bytes(&editor.settings)?)?;
        }
        add(&mut bytes, self.hex.document_metadata_bytes()?)?;
        add(
            &mut bytes,
            (self.files.close.capacity() + self.files.save_all.capacity())
                .checked_mul(size_of::<DocumentId>())
                .ok_or_else(overflow)?,
        )?;
        if let Some(prompt) = &self.files.prompt {
            add(&mut bytes, prompt.text.capacity())?;
        }
        for message in self.files.journal_failed.values() {
            add(&mut bytes, message.capacity())?;
        }
        for save in self.files.saves.values() {
            if let Some(path) = &save.path {
                add(&mut bytes, path.capacity())?;
            }
        }
        let accepted_settings = self.editor_settings.as_ref().map(settings_metadata_bytes).transpose()?.unwrap_or(0);
        let future_settings_bytes =
            future_settings.map(settings_metadata_bytes).transpose()?.unwrap_or(accepted_settings);
        let settings_upper = accepted_settings.max(future_settings_bytes).max(4096);
        add(&mut bytes, accepted_settings)?;
        let mut introduced = 0usize;
        if let Some(result) = incoming {
            match result {
                DocumentResult::Prepared(Ok(prepared)) => {
                    add(
                        &mut introduced,
                        session_metadata(&prepared.session)
                            .ok_or_else(overflow)?
                            .checked_mul(4)
                            .ok_or_else(overflow)?,
                    )?;
                    map(
                        &mut introduced,
                        (prepared.session.tabs().len() + self.views.len()).clamp(1, MAX_TABS),
                        size_of::<(DocumentId, View)>(),
                    )?;
                    add(
                        &mut introduced,
                        prepared
                            .documents
                            .len()
                            .checked_mul(size_of::<Editor>() + size_of::<crate::Cursor>() + settings_upper + 4096)
                            .ok_or_else(overflow)?,
                    )?;
                    for (_, document) in &prepared.documents {
                        if let Ok(document) = document
                            && let Some(path) = &document.path
                        {
                            add(&mut introduced, path.capacity().checked_mul(4).ok_or_else(overflow)?)?;
                        }
                    }
                }
                DocumentResult::Replaced { editor, .. }
                | DocumentResult::RawReopened { editor, .. }
                | DocumentResult::HexPage { editor, .. }
                | DocumentResult::HexEdited { editor, .. } => {
                    add(&mut introduced, editor.document_metadata_bytes()?)?;
                    add(&mut introduced, settings_metadata_bytes(&editor.settings)?)?;
                }
                DocumentResult::Indexed { result: Ok(document), .. }
                | DocumentResult::Recovered { result: Ok(document), .. }
                | DocumentResult::CheckedOpened { result: Ok(Some(document)), .. } => {
                    add(&mut introduced, size_of::<Editor>() + size_of::<crate::Cursor>() + 4096)?;
                    if let Some(path) = &document.path {
                        add(&mut introduced, path.capacity().checked_mul(4).ok_or_else(overflow)?)?;
                    }
                }
                DocumentResult::Saved { result: Ok(saved), .. } => {
                    add(&mut introduced, saved.path.capacity().checked_mul(4).ok_or_else(overflow)?)?;
                    let warnings = saved
                        .warnings
                        .iter()
                        .try_fold(0usize, |n, message| n.checked_add(message.capacity()).and_then(|n| n.checked_add(2)))
                        .ok_or_else(overflow)?;
                    add(&mut introduced, warnings.checked_mul(3).ok_or_else(overflow)?)?;
                }
                DocumentResult::Preview { path, .. } | DocumentResult::Opened { result: Ok((path, _)), .. } => {
                    add(&mut introduced, path.capacity().checked_mul(4).ok_or_else(overflow)?)?
                }
                _ => {}
            }
        }
        if introduced > 0 {
            add(&mut introduced, settings_upper)?;
            add(&mut introduced, 128 * 1024)?;
        }
        // An accepted Editor loan can reside in the worker/request lane. Keep
        // the prior aggregate as a floor until it returns; only this commit's
        // finite introduced allocation is added, never another whole baseline.
        if !self.files.loans.is_empty() || !self.hex.loans.is_empty() {
            let old = self.files.live_document_metadata.as_ref().map_or(0, |charge| {
                charge.reserved_bytes().saturating_sub(size_of::<crate::visual_rows::MetadataReservation>())
            });
            bytes = bytes.max(old);
        }
        add(&mut bytes, introduced)?;
        if future_settings.is_some() {
            let extra = future_settings_bytes
                .checked_mul(1 + self.views.len() + self.closed_views.len())
                .ok_or_else(overflow)?;
            add(&mut bytes, extra)?;
        }
        Ok(bytes)
    }
}

#[cfg(test)]
#[path = "commit_tests.rs"]
mod tests;
