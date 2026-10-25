//! KW-21 live GUI adapter for decision 0003. SOURCE ONLY; not execution proof.
//!
//! Document delivery uses the sealed bounded Workspace transport. No source-bearing
//! completion or exceptional document slot belongs to this adapter.

use super::*;
use kwil_core_commands::theme::{Theme as UiTheme, UiRole};
use kwil_view_model::{
    ChargedDamage,
    visual_rows::{
        AllocationBudget, AtomKind, BodyOwner, CaretAffinity, LayoutGeometry, LayoutKey, LayoutPage, MAX_COLUMNS,
        MAX_ROWS, PaintAtom, SourcePoint, TopAnchor, VisualCursor, VisualMotion, VisualPoint, WrapError, WrapResult,
    },
    workspace_wrap::{
        WorkspaceWrap, WorkspaceWrapResultCell, WorkspaceWrapSettlementState, WorkspaceWrapSettlementTicket,
    },
    wrap_host::{WrapHost, WrapHostAvailability, WrapHostContext, WrapHostDrainFailure, WrapHostSync},
    wrap_session::{CancelReason, MutationGate},
};
#[cfg(test)]
use kwil_view_model::{
    visual_rows::WrapSource,
    wrap_host::{WrapHostSettlementTicket, WrapHostSourceOwner, WrapHostSourceRefusal},
};
use std::ops::{ControlFlow as Flow, Range};
#[cfg(test)]
use std::sync::Arc;

pub(super) struct Host {
    budget: AllocationBudget,
    shared: Option<WrapHost>,
    workspace: Option<WorkspaceWrap>,
    result_cell: Option<WorkspaceWrapResultCell>,
    settlement: Option<WorkspaceWrapSettlementTicket>,
    context: Option<WrapHostContext>,
    workspace_epoch: u64,
    stopping: bool,
    font: Option<font::UiFont>,
    font_key: Option<(u64, u64, u32)>,
    // Do not detach the charged cursor-damage allocation while rows are queued.
    cursor_damage: Option<ChargedDamage>,
    exception: Exception,
    deferred_wake: bool,
    #[cfg(test)]
    manual_injection: bool,
}
impl Default for Host {
    fn default() -> Self {
        Self::with_budget(AllocationBudget::default())
    }
}

/// Exact layout drain failure. Document refusals remain in the single sealed
/// transport slot, and never enter a frontend source-bearing exception cell.
#[derive(Default)]
struct Exception {
    drain: Option<WrapHostDrainFailure>,
}

/// ROOT moves this complete bundle into its existing off-UI settlement owner.
#[must_use = "settle the ticket and exceptional owners before document cleanup"]
#[cfg(test)]
pub(super) struct Stop {
    pub ticket: WrapHostSettlementTicket,
    pub failure: Option<WrapHostDrainFailure>,
    pub cursor_damage: Option<ChargedDamage>,
}

/// A borrow of the existing prepared cache, split from other App fields so the
/// normal retained-body renderer keeps its copy/composite/presentation seam.
pub(super) struct Paint<'a> {
    page: Option<&'a LayoutPage>,
    cursors: &'a [VisualCursor],
    font: Option<&'a mut font::UiFont>,
}
impl Host {
    pub(super) fn document_shutdown_ready(&self) -> bool {
        self.shared.is_none() && self.workspace.is_none() && self.settlement.is_none() && self.exception.drain.is_none()
    }
    pub(super) fn with_budget(budget: AllocationBudget) -> Self {
        Self {
            budget,
            shared: None,
            workspace: None,
            result_cell: None,
            settlement: None,
            context: None,
            workspace_epoch: 0,
            stopping: false,
            font: None,
            font_key: None,
            cursor_damage: None,
            exception: Exception::default(),
            deferred_wake: false,
            #[cfg(test)]
            manual_injection: false,
        }
    }
    pub(super) fn budget(&self) -> &AllocationBudget {
        &self.budget
    }
    pub(super) fn context_for_paint(&self) -> Option<WrapHostContext> {
        self.context
    }
    pub(super) fn paint(&mut self, key: Option<LayoutKey>) -> Paint<'_> {
        let page = self.shared.as_ref().and_then(WrapHost::page).filter(|page| Some(page.key) == key);
        let cursors = self.shared.as_ref().filter(|_| page.is_some()).and_then(WrapHost::visual_cursors).unwrap_or(&[]);
        Paint { page, cursors, font: self.font.as_mut().map(font::UiFont::monospace) }
    }
}
impl Paint<'_> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn row(
        &mut self,
        pixels: &mut [u32],
        stride: usize,
        row: usize,
        editor_font: &mut Font,
        theme: &UiTheme,
        spans: &[&kwil_view_model::highlighting::Span],
        hits: impl Iterator<Item = Range<u64>>,
    ) {
        paint_row(
            pixels,
            stride,
            row,
            editor_font.cw,
            editor_font.ch,
            self.page,
            self.font.as_deref_mut(),
            Some(editor_font),
            theme,
            self.cursors,
            spans,
            hits,
        );
    }
}

impl App {
    /// Accepted exit owns body input, but the explicit retry command remains
    /// reachable through its configured chord.
    /// No editing command escapes this gate, even while a receipt is pending.
    pub(super) fn wrap_shutdown_key(&mut self, event: &HostKey<'_>) -> bool {
        if !self.wrap.stopping {
            return false;
        }
        if let Some(chord) = input::chord(&event.logical_key, self.mods) {
            if self.keys.is_none() {
                self.keys =
                    kwil_core_commands::keymap::KeymapEngine::new(self.settings.keymap, &self.settings.bindings).ok();
            }
            if let Some(keys) = &mut self.keys {
                keys.set_context(kwil_core_commands::Context::Editor);
                if let kwil_core_commands::vim::VimResult::Command(call) = keys.feed_with_selection(chord)
                    && call.id == "view.retryWrap"
                {
                    self.wrap_command(call.id, 1, false);
                }
            }
        }
        true
    }

    pub(super) fn wrap_highlights_ready(&mut self) -> Option<Damage> {
        if !self.wrap_owns_text() && !self.view.wrap_input_blocked() {
            return None;
        }
        self.wrap_sync_live();
        let live = self.wrap_live();
        let page = self.wrap.shared.as_ref().filter(|_| live).and_then(WrapHost::page);
        let rows = match (&mut self.highlighting, page, self.wrap.context) {
            (Some(host), Some(page), Some(context)) => host.worker_ready_prepared(&self.view, page, context),
            (Some(host), _, _) => {
                host.suspend_prepared();
                Vec::new()
            }
            _ => Vec::new(),
        };
        Some(Damage { rows, status: true, ..Default::default() })
    }
    pub(super) fn wrap_command(&mut self, id: &str, count: u32, extend: bool) -> bool {
        if id == "view.retryWrap" {
            if !self.wrap_retry_shutdown() {
                if let Err(error) = self.wrap_retry() {
                    self.wrap_notice(format!("Wrapped retry refused: {error}"));
                }
                self.wrap_lend_source();
            }
            return true;
        }
        self.wrap_sync_live();
        if self.wrap_navigate(id, count, extend) {
            return true;
        }
        if ((id.starts_with("edit.") && id != "edit.copy")
            || id.starts_with("motion.")
            || id.starts_with("selection.")
            || id == "navigation.goToLine"
            || id.starts_with("search.replace"))
            && !self.wrap_mutation_allowed()
        {
            return true;
        }
        false
    }

    pub(super) fn wrap_vim_command(&mut self, call: &kwil_core_commands::vim::Invocation) -> bool {
        self.wrap_sync_live();
        if let Some(motion) = call.visual_motion() {
            if self.wrap_owns_text() {
                let extend = matches!(&call.argument, kwil_core_commands::vim::Argument::VisualMotion { extend: true });
                return self.wrap_navigate(motion.id(), call.count, extend);
            }
            return false;
        }
        if call.motion.is_none() && matches!(call.id, "motion.pageUp" | "motion.pageDown") {
            let extend = self.keys.as_ref().is_some_and(|keys| {
                matches!(keys.mode(), kwil_core_commands::Mode::Visual | kwil_core_commands::Mode::VisualLine)
            });
            return self.wrap_command(call.id, call.count, extend);
        }
        !self.wrap_mutation_allowed()
    }

    pub(super) fn wrap_model_damage(&self, damage: Damage) -> Damage {
        if self.wrap_owns_text() { Damage { status: damage.status, ..Default::default() } } else { damage }
    }

    pub(super) fn wrap_update_highlights(&mut self) {
        let prepared = self.wrap_owns_text() || self.view.wrap_input_blocked();
        let live = self.wrap_live();
        let context = self.wrap.context;
        let page = self.wrap.shared.as_ref().filter(|_| live).and_then(WrapHost::page);
        let ordinary =
            !self.view.hex_active() && !self.logs.owns_body() && self.markdown_mode() != markdown_ui::Mode::Preview;
        let mut failure = None;
        if let Some(highlighting) = &mut self.highlighting {
            if prepared {
                if let (Some(page), Some(context)) = (page, context) {
                    highlighting.update_prepared(&self.view, page, context);
                } else {
                    highlighting.suspend_prepared();
                }
            } else if ordinary {
                highlighting.update(&self.view);
            } else {
                highlighting.suspend_prepared();
            }
            if prepared {
                failure = highlighting.prepared_error().map(ToString::to_string);
            }
        }
        if let Some(error) = failure {
            let notice = format!("Wrapped syntax unavailable: {error}");
            if self.view.notice() != notice {
                self.wrap_notice(notice);
            }
        }
    }

    pub(super) fn wrap_status(&self) -> (String, Vec<chrome::Segment>) {
        let mut segments = Vec::new();
        if let Some(recovery) = self.view.recovery_notice() {
            segments.push(chrome::Segment { text: recovery.into(), command: None, warn: true });
        }
        if self.view.editor().is_some_and(|editor| {
            editor.document().compression == kwil_core_workspace::compressed::CompressionFormat::Gzip
        }) {
            segments.push(chrome::Segment { text: "Gzip · decoded text".into(), command: None, warn: false });
        }
        if self.zoom.percent() != 100 {
            segments.push(chrome::Segment {
                text: format!("Zoom {}%", self.zoom.percent()),
                command: Some("view.zoomReset"),
                warn: false,
            });
        }
        if let Some(percent) = self.view.indexing_percent() {
            segments.push(chrome::Segment { text: format!("Indexing {percent}%"), command: None, warn: false });
        } else if let Some(lines) = self.view.line_count() {
            segments.push(chrome::Segment {
                text: format!("{lines} {}", if lines == 1 { "line" } else { "lines" }),
                command: None,
                warn: false,
            });
        }
        if let Some(page) = self.wrap_page()
            && let Some(cursor) = self.view.editor().map(|editor| editor.primary_cursor())
            && let Some(point) = self
                .wrap
                .shared
                .as_ref()
                .and_then(|host| {
                    host.visual_cursors().and_then(|cursors| host.primary_index().and_then(|index| cursors.get(index)))
                })
                .and_then(|visual| page.source_to_screen(cursor.head, visual.head_affinity))
            && let Some(row) = page.rows().get(point.row)
        {
            segments.push(chrome::Segment {
                text: format!("Ln {}, Visual Col {}", row.logical_line + 1, u32::from(point.cell) + 1),
                command: Some("navigation.goToLine"),
                warn: false,
            });
        }
        if self.view.wrap_settlement_state() == WorkspaceWrapSettlementState::Failed
            || self.wrap.shared.as_ref().is_some_and(|host| host.mutation_gate() == MutationGate::RefusedFailed)
        {
            segments.push(chrome::Segment {
                text: "Retry wrapping".into(),
                command: Some("view.retryWrap"),
                warn: true,
            });
        }
        (self.view.notice().into(), segments)
    }
    /// Refresh the shared scalar fence before dispatch. Source ownership and
    /// refusals stay in WorkspaceWrap; the GUI never retains another Arc.
    pub(super) fn wrap_sync_live(&mut self) {
        // Only the explicit WorkerOnly-source fixture sets this. Production
        // and the live key/configured-font controls use the normal manager.
        #[cfg(test)]
        if self.wrap.manual_injection {
            return;
        }
        let policy = if self.wrap_owns_text() || self.view.wrap_input_blocked() {
            kwil_view_model::ViewBodyPolicy::Prepared
        } else {
            kwil_view_model::ViewBodyPolicy::LogicalLines
        };
        if self.view.body_policy() != policy {
            let damage = self.view.set_body_policy(policy);
            self.wrap_queue_damage(damage);
        }
        if self.pending_resize.is_some() {
            return;
        }
        if self.wrap_delivery_blocked() {
            return;
        }
        if self.wrap.stopping || self.view.wrap_input_blocked() {
            self.wrap_submit_shutdown();
            return;
        }
        if self.wrap.shared.is_none() && !self.settings.editor.word_wrap {
            return;
        }
        if self.wrap.shared.is_none() {
            let proxy = self.proxy.clone();
            match WrapHost::with_budget(self.wrap.budget.clone(), move || {
                let _ = proxy.send_event(UserEvent::WrapReady);
            }) {
                Ok(host) => self.wrap.shared = Some(host),
                Err(error) => {
                    self.wrap_notice(format!("Wrapped layout unavailable: {error}"));
                    return;
                }
            }
        }
        if self.wrap.workspace.is_none() {
            if self.wrap.result_cell.is_none() {
                match WorkspaceWrapResultCell::new(self.wrap.shared.as_ref().expect("created host")) {
                    Ok(cell) => self.wrap.result_cell = Some(cell),
                    Err(error) => {
                        self.wrap_notice(format!("Wrapped result admission failed: {error}"));
                        return;
                    }
                }
            }
            match WorkspaceWrap::new(&mut self.view, self.wrap.shared.as_ref().expect("created host")) {
                Ok(owner) => self.wrap.workspace = Some(owner),
                Err(error) => {
                    self.wrap_notice(format!("Wrapped source admission failed: {error}"));
                    return;
                }
            }
        }
        let geometry = match self.wrap_geometry(LayoutGeometry::default()) {
            Ok(geometry) => geometry,
            Err(error) => {
                self.wrap_notice(format!("Wrapped layout unavailable: {error}"));
                return;
            }
        };
        let body = self.wrap_body_owner();
        let context =
            self.wrap.workspace.as_mut().expect("registered owner").prepare_context(&self.view, geometry, body);
        match context.and_then(|context| self.wrap_sync(context)) {
            Ok(_) => {}
            Err(WrapError::Busy) => {} // fixed worker capacity wake owns retry
            Err(error) => self.wrap_notice(format!("Wrapped layout synchronization failed: {error}")),
        }
    }

    pub(super) fn wrap_lend_source(&mut self) {
        if self.view.wrap_input_blocked() || self.wrap_delivery_blocked() {
            return;
        }
        let (Some(owner), Some(host), Some(context)) =
            (&mut self.wrap.workspace, &mut self.wrap.shared, self.wrap.context)
        else {
            return;
        };
        if owner.failed() {
            return;
        }
        match owner.lend_source(&self.view, context, host) {
            Ok(()) | Err(WrapError::Busy) => {}
            Err(error) => self.wrap_notice(format!("Wrapped source refused: {error}")),
        }
    }

    /// Called only once the existing close/save/dialog dispatcher accepts exit.
    /// Submission goes through the document worker while its event loop lives.
    pub(super) fn wrap_shutdown_ready(&mut self) -> bool {
        if self.wrap_delivery_blocked() {
            return false;
        }
        self.wrap.stopping = true;
        self.document.deadline.get_or_insert_with(|| std::time::Instant::now() + std::time::Duration::from_secs(35));
        if self.view.wrap_cleanup_ready() {
            return true;
        }
        self.view.begin_wrap_shutdown();
        if !self.view.wrap_shutdown_can_submit() {
            return false;
        }
        if self.wrap.settlement.is_none() && self.wrap.workspace.is_some() && self.wrap.shared.is_some() {
            let owner = self.wrap.workspace.take().expect("registered owner");
            let host = self.wrap.shared.take().expect("paired host");
            match owner.stop(host) {
                Flow::Continue(ticket) => self.wrap.settlement = Some(ticket),
                Flow::Break((owner, host)) => {
                    self.wrap.workspace = Some(owner);
                    self.wrap.shared = Some(host);
                    self.wrap_notice("Wrapped shutdown refused a mismatched owner".into());
                    return false;
                }
            }
        }
        self.wrap_submit_shutdown();
        false
    }

    fn wrap_submit_shutdown(&mut self) {
        let timeout = self.document.deadline.map_or(std::time::Duration::from_secs(5), |deadline| {
            deadline.saturating_duration_since(std::time::Instant::now()).min(std::time::Duration::from_secs(5))
        });
        if let Some(ticket) = self.wrap.settlement.take()
            && let Flow::Break(ticket) = self.view.submit_wrap_settlement(ticket, timeout)
        {
            self.wrap.settlement = Some(ticket);
        }
    }

    pub(super) fn wrap_document_settled(&mut self) {
        if self.view.wrap_cleanup_ready() {
            self.wrap.cursor_damage = None;
        }
    }
    pub(super) fn wrap_retry_shutdown(&mut self) -> bool {
        if let Some(status) = self.document.live.transport.as_ref().map(|transport| transport.status())
            && status.failure.is_some()
            && !status.retry_pending
            && !status.retry_acknowledgement_required
        {
            self.document.live.wrap_retry = None;
        }
        if self.document.live.transport.as_ref().is_some_and(|transport| {
            matches!(
                transport.status().failure,
                Some(
                    kwil_view_model::document_worker::DocumentResultSettlementReason::WrapSettlementFailed
                        | kwil_view_model::document_worker::DocumentResultSettlementReason::WrapThreadExitPending
                        | kwil_view_model::document_worker::DocumentResultSettlementReason::WrapNativeExit(_)
                )
            )
        }) {
            if self.document.live.wrap_retry.is_none() && self.document.live.wrap_retry_refusal.is_none() {
                let timeout = self.document.deadline.map_or(std::time::Duration::from_secs(5), |deadline| {
                    deadline.saturating_duration_since(std::time::Instant::now()).min(std::time::Duration::from_secs(5))
                });
                match self.document.live.transport.as_mut().unwrap().request_wrap_retry(timeout) {
                    Ok(token) => self.document.live.wrap_retry = Some(token),
                    Err(reason) => self.wrap_notice(format!("Wrapped shutdown retry refused: {reason:?}")),
                }
            }
            return true;
        }
        if self.view.wrap_settlement_state() != WorkspaceWrapSettlementState::Failed {
            return false;
        }
        if let Err(error) = self.view.retry_wrap_settlement(std::time::Duration::from_secs(5)) {
            self.wrap_notice(format!("Wrapped shutdown retry refused: {error}"));
        }
        true
    }

    pub(super) fn wrap_delivery_blocked(&self) -> bool {
        self.wrap.exception.drain.is_some()
            || self.wrap.result_cell.as_ref().is_some_and(WorkspaceWrapResultCell::is_occupied)
    }

    fn wrap_body_owner(&self) -> BodyOwner {
        if self.view.hex_active() || self.binary_blocks_paint() {
            BodyOwner::Raw
        } else if self.markdown_mode() == markdown_ui::Mode::Preview {
            BodyOwner::MarkdownPreview
        } else if self.logs.owns_body() {
            BodyOwner::LogProjection
        } else if self.view.editor().is_none() {
            BodyOwner::EditorLoan
        } else {
            BodyOwner::Text
        }
    }

    /// Uses the text side of Markdown split and the log panel's real inset.
    pub(super) fn wrap_geometry(&self, mut geometry: LayoutGeometry) -> WrapResult<LayoutGeometry> {
        let columns = self.columns() as usize;
        let rows = self.text_rows();
        if columns > MAX_COLUMNS || rows > MAX_ROWS {
            return Err(WrapError::Admission("GUI wrap geometry exceeds admitted extent"));
        }
        geometry.columns = columns as u16;
        geometry.rows = rows as u16;
        geometry.tab_size = u16::from(self.settings.editor.tab_size);
        geometry.font_generation = self.font.as_ref().map_or(0, |font| font.generation);
        geometry.validate()?;
        Ok(geometry)
    }

    pub(super) fn wrap_sync(&mut self, mut context: WrapHostContext) -> WrapResult<WrapHostSync> {
        context.enabled &= self.settings.editor.word_wrap;
        let body = self.wrap_body_owner();
        if body != BodyOwner::Text {
            context.key.body = body;
        }
        context.key.geometry = match self.wrap_geometry(context.key.geometry) {
            Ok(geometry) => geometry,
            Err(error) => {
                if let Some(host) = &mut self.wrap.shared {
                    host.cancel(CancelReason::GeometryChanged);
                }
                self.wrap.context = None;
                self.wheel.clear();
                self.wrap_notice(format!("Wrapped layout unavailable: {error}"));
                return Err(error);
            }
        };
        let changed = self.wrap.context != Some(context);
        // Hide the previous page even if retirement makes sync temporarily Busy.
        self.wrap.context = Some(context);
        self.wrap.workspace_epoch = self.view.input_epoch();
        if self.wrap.shared.is_none()
            && (!context.enabled || context.key.body != BodyOwner::Text || context.key.geometry.is_empty())
        {
            let availability = if !context.enabled {
                WrapHostAvailability::Disabled
            } else if context.key.body != BodyOwner::Text {
                WrapHostAvailability::BodyOwned(context.key.body)
            } else {
                WrapHostAvailability::EmptyGeometry
            };
            return Ok(WrapHostSync { availability, cancelled: None, needs_source: false });
        }
        if self.wrap.shared.is_none() {
            let proxy = self.proxy.clone();
            self.wrap.shared = Some(WrapHost::with_budget(self.wrap.budget.clone(), move || {
                let _ = proxy.send_event(UserEvent::WrapReady);
            })?);
        }
        if changed {
            self.wheel.clear();
            self.horizontal = 0;
            self.wrap_queue_damage(Damage {
                rows: (0..usize::from(context.key.geometry.rows)).collect(),
                status: true,
                ..Default::default()
            });
        }
        let sync = self.wrap.shared.as_mut().expect("created host").sync(self.view.editor(), context)?;
        if sync.cancelled.is_some_and(|cancelled| cancelled.records != 0) {
            self.wrap_notice("Wrapped movement cancelled: source, body or geometry changed".into());
        }
        Ok(sync)
    }

    /// Cheap last-moment fence, including unsynchronized body/tab/revision changes.
    fn wrap_live(&self) -> bool {
        let Some(context) = self.wrap.context.filter(|context| context.enabled) else {
            return false;
        };
        let Some(editor) = self.view.editor() else {
            return false;
        };
        let source = editor.document().hex_source(self.view.active_id().0);
        let cursors_match = self.wrap.shared.as_ref().is_some_and(|host| {
            host.primary_index() == Some(editor.primary_index())
                && host.visual_cursors().is_some_and(|cursors| {
                    cursors.len() == editor.cursors().len()
                        && cursors
                            .iter()
                            .zip(editor.cursors())
                            .all(|(visual, actual)| visual.anchor == actual.anchor && visual.head == actual.head)
                })
        });
        !self.wrap_delivery_blocked()
            && self
                .wrap
                .workspace
                .as_ref()
                .is_none_or(|owner| !owner.failed() && owner.context_is_current(&self.view, context))
            && !self.view.wrap_input_blocked()
            && cursors_match
            && self.settings.editor.word_wrap
            && self.wrap_body_owner() == BodyOwner::Text
            && context.key.body == BodyOwner::Text
            && source.document == context.key.source.document_id
            && source.epoch == context.key.source.source_epoch
            && source.revision == context.key.source.revision
            && self.view.input_epoch() == self.wrap.workspace_epoch
            && (self.wrap.workspace.is_none()
                || editor.external_cursor_generation().is_ok_and(|generation| generation == context.cursor_generation))
            && self.wrap_geometry(context.key.geometry).is_ok_and(|geometry| geometry == context.key.geometry)
    }

    pub(super) fn wrap_owns_text(&self) -> bool {
        self.settings.editor.word_wrap
            && matches!(self.wrap_body_owner(), BodyOwner::Text | BodyOwner::EditorLoan)
            && (self.view.editor().is_some() || self.view.body_policy() == kwil_view_model::ViewBodyPolicy::Prepared)
    }

    pub(super) fn wrap_page(&self) -> Option<&LayoutPage> {
        self.wrap_live().then_some(())?;
        let page = self.wrap.shared.as_ref()?.page()?;
        (Some(page.key) == self.wrap.context.map(|context| context.key)).then_some(page)
    }

    /// Refusal returns the complete original source/request to ROOT. No cache.
    #[cfg(test)]
    pub(super) fn wrap_drive(&mut self, source: Arc<dyn WrapSource>) -> Flow<WrapHostSourceRefusal, ()> {
        if !self.wrap_live() {
            return Flow::Break(WrapHostSourceRefusal {
                owner: WrapHostSourceOwner::from_source(source),
                error: WrapError::Stale,
            });
        }
        self.wrap.shared.as_mut().expect("live host").drive(source)
    }

    /// One bounded drain per worker wake. A queued charged publication owns its
    /// slot until paint; a wake deferred behind it is serviced after that paint.
    pub(super) fn wrap_ready(&mut self) {
        if self.wrap.workspace.is_some() {
            self.wrap_sync_live();
        }
        if self.view.wrap_input_blocked() {
            return;
        }
        if self.wrap.cursor_damage.is_some() {
            self.wrap.deferred_wake = true;
            return;
        }
        if self.wrap_delivery_blocked() {
            return;
        }
        let live = self.wrap_live();
        let Some(host) = &mut self.wrap.shared else {
            return;
        };
        let result = if let Some(owner) = &mut self.wrap.workspace {
            match owner.drain_host(host, if live { self.view.editor_mut() } else { None }) {
                Ok(update) => Flow::Continue(update),
                Err(error) => {
                    self.wrap_notice(format!("Wrapped layout stopped: {error}"));
                    self.wrap_queue_damage(Damage {
                        rows: (0..self.text_rows()).collect(),
                        status: true,
                        ..Default::default()
                    });
                    return;
                }
            }
        } else {
            host.drain(if live { self.view.editor_mut() } else { None })
        };
        let update = match result {
            Flow::Continue(update) => update,
            Flow::Break(failure) => {
                self.wrap_notice(format!("Wrapped layout stopped: {:?}", failure.error));
                let failure = match self.wrap.workspace.as_mut() {
                    Some(owner) => match owner.retain_drain_failure(failure) {
                        Flow::Continue(()) => None,
                        Flow::Break(failure) => Some(failure),
                    },
                    None => Some(failure),
                };
                self.wrap.exception.drain = failure;
                // A post-publication retirement failure may already have
                // committed cursor state. Clear the old body rather than leave
                // stale caret pixels; the complete charged update stays owned
                // by the retained failure until off-UI settlement.
                self.wrap_queue_damage(Damage { full: true, status: true, ..Default::default() });
                return;
            }
        };
        if let Some(publication) = update.publication {
            let mut damage = Damage {
                full: false,
                scroll: if publication.damage.full { 0 } else { publication.damage.scroll },
                rows: publication.damage.changed_rows().collect(),
                status: true,
            };
            if let Some(cursor_damage) = publication.cursor_damage {
                let cursor = cursor_damage.damage();
                if cursor.full {
                    damage
                        .rows
                        .extend(0..self.wrap.context.map_or(0, |context| usize::from(context.key.geometry.rows)));
                }
                damage.status |= cursor.status;
                damage.rows.extend_from_slice(&cursor.rows);
                self.wrap.cursor_damage = Some(cursor_damage);
            }
            self.wrap_queue_damage(damage);
        }
        if let Some(error) = update.failure {
            self.wrap_notice(format!("Wrapped movement failed; retry or Escape: {error}"));
        } else if let Some(progress) = update.progress {
            self.wrap_notice(format!("Wrapping: {} bytes scanned (target {})", progress.scanned, progress.target));
        }
        self.wrap_lend_source();
        self.document_after_wrap_wake();
    }

    /// Return true to service ONE deferred wake after the current paint. This
    /// does not itself request another frame and is never called from idle.
    pub(super) fn wrap_frame_painted(&mut self) -> bool {
        self.wrap.cursor_damage = None;
        std::mem::take(&mut self.wrap.deferred_wake)
    }

    fn wrap_queue_damage(&mut self, damage: Damage) {
        if damage == Damage::default() {
            return;
        }
        let rows = self.wrap.context.map_or(0, |context| usize::from(context.key.geometry.rows));
        if self.document.live.update.is_some() {
            self.document.live.row_shift = self.document.live.row_shift.saturating_sub(damage.scroll);
        }
        queue_damage(&mut self.pending, damage, rows, self.font.as_mut());
        // Many worker wakes before one present must not accumulate duplicate
        // viewport rows into a growing retained Vec.
        self.pending.rows.retain(|row| *row < rows);
        self.pending.rows.sort_unstable();
        self.pending.rows.dedup();
        self.request_frame();
    }

    pub(super) fn wrap_geometry_pending(&mut self) {
        if let Some(host) = &mut self.wrap.shared {
            let cancelled = host.cancel(CancelReason::GeometryChanged);
            if cancelled.records != 0 {
                self.wrap_notice("Wrapped movement cancelled: geometry changed".into());
            }
        }
        self.wrap.context = None;
        self.wheel.clear();
    }

    fn wrap_notice(&mut self, notice: String) {
        let damage = self.view.set_notice(notice);
        self.wrap_queue_damage(damage);
    }

    pub(super) fn wrap_mutation_allowed(&mut self) -> bool {
        if self.document_input_blocked() {
            self.wrap_notice("Document ownership failure is retained; editing is blocked".into());
            return false;
        }
        if self.wrap_delivery_blocked() {
            self.wrap_notice("Wrapped ownership failure is retained; editing is blocked".into());
            return false;
        }
        if self.view.wrap_input_blocked() {
            self.wrap_notice("Wrapped shutdown is settling; editing is blocked".into());
            return false;
        }
        if self.wrap_owns_text() && !self.wrap_live() {
            self.wrap_notice("Wrapped layout is unavailable; wait for synchronization before editing".into());
            return false;
        }
        let Some(host) = self.wrap.shared.as_ref().filter(|_| self.wrap_live()) else {
            return true;
        };
        match host.mutation_gate() {
            MutationGate::Allowed => true,
            MutationGate::RefusedPending => {
                self.wrap_notice("Wrapped movement pending; wait or press Escape before editing".into());
                false
            }
            MutationGate::RefusedFailed => {
                self.wrap_notice("Wrapped movement failed; retry or press Escape before editing".into());
                false
            }
        }
    }

    /// Command IDs arrive AFTER palette/keymap/Vim resolution, preserving binds.
    pub(super) fn wrap_navigate(&mut self, command: &str, count: u32, extend: bool) -> bool {
        let motion = match command {
            "motion.up" => VisualMotion::Up,
            "motion.down" => VisualMotion::Down,
            "motion.pageUp" => VisualMotion::PageUp,
            "motion.pageDown" => VisualMotion::PageDown,
            _ => return false,
        };
        if !self.wrap_owns_text() {
            return false;
        }
        if !self.wrap_live() {
            self.wrap_notice("Wrapped movement awaits current layout synchronization".into());
            return true;
        }
        if let Err(error) = self.wrap.shared.as_mut().expect("live host").navigate(motion, count, extend) {
            self.wrap_notice(format!("Wrapped movement refused: {error:?}"));
        }
        self.wrap_lend_source();
        true
    }

    /// Accept already normalized, signed visual rows from the existing wheel.
    /// Horizontal/Shift-wheel is consumed while wrap owns the text body.
    pub(super) fn wrap_wheel(&mut self, vertical: i64, horizontal: i64) -> bool {
        if !self.wrap_owns_text() || self.markdown_pointer() {
            return false;
        }
        if !self.wrap_live() {
            self.wrap_notice("Wrapped scroll awaits current layout synchronization".into());
            return true;
        }
        if vertical != 0
            && horizontal == 0
            && !self.mods.shift_key()
            && let Err(error) = self.wrap.shared.as_mut().expect("live host").scroll(vertical)
        {
            self.wrap_notice(format!("Wrapped scroll refused: {error:?}"));
        }
        self.wrap_lend_source();
        true
    }

    pub(super) fn wrap_fill(&mut self, anchor: TopAnchor) -> WrapResult<()> {
        if !self.wrap_live() {
            return Err(WrapError::Stale);
        }
        self.wrap.shared.as_mut().expect("live host").request_fill(anchor)?;
        self.wrap_lend_source();
        Ok(())
    }

    pub(super) fn wrap_reveal(&mut self, point: SourcePoint) -> WrapResult<()> {
        if !self.wrap_live() {
            return Err(WrapError::Stale);
        }
        self.wrap.shared.as_mut().expect("live host").request_reveal(point)?;
        self.wrap_lend_source();
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn wrap_hit_test(&self, row: usize, cell: usize) -> Option<SourcePoint> {
        self.wrap_page()?.hit_test(row, cell)
    }

    pub(super) fn wrap_pointer(&mut self, extend: bool) -> bool {
        if !self.wrap_owns_text() || self.markdown_pointer() {
            return false;
        }
        if !self.wrap_live() {
            self.wrap_notice("Wrapped selection awaits current layout synchronization".into());
            return true;
        }
        let body = self.body_area();
        let (cw, ch) = self.font.as_ref().map_or((8, 16), |font| (font.cw.max(1), font.ch.max(1)));
        let text = Rect {
            x: body.x + GUTTER_COLS * cw,
            y: body.y,
            w: body.width.saturating_sub(GUTTER_COLS * cw + self.ui.bar),
            h: self.text_rows() * ch,
        };
        if !text.contains(self.pointer) && !self.selecting {
            return false;
        }
        let Some(page) = self.wrap_page() else {
            self.wrap_notice("Wrapped layout pending; pointer has no prepared source mapping".into());
            return true;
        };
        let row = (self.pointer.1.saturating_sub(text.y) / ch).min(page.rows().len().saturating_sub(1));
        let cell = (self.pointer.0.saturating_sub(text.x) / cw).min(usize::from(page.key.geometry.columns));
        if let Err(error) = self.wrap.shared.as_mut().expect("live host").hit(row, cell, extend) {
            self.wrap_notice(format!("Wrapped selection refused: {error}"));
        }
        self.wrap_lend_source();
        true
    }

    pub(super) fn wrap_cancel(&mut self) -> bool {
        let Some(host) = self.wrap.shared.as_mut() else {
            return false;
        };
        let pending = host.mutation_gate() != MutationGate::Allowed;
        host.cancel(CancelReason::Escape);
        if pending {
            self.wrap_notice("Wrapped movement cancelled".into());
        }
        pending
    }

    pub(super) fn wrap_retry(&mut self) -> WrapResult<()> {
        if !self.wrap_live() {
            return Err(WrapError::Stale);
        }
        self.wrap.shared.as_mut().expect("live host").retry_relative()
    }

    pub(super) fn wrap_caret(&self, byte: u64, affinity: CaretAffinity) -> Option<VisualPoint> {
        self.wrap_page()?.source_to_screen(byte, affinity)
    }

    #[cfg(test)]
    pub(super) fn wrap_selection_cells(&self, row: usize, source: Range<u64>) -> impl Iterator<Item = Range<u16>> + '_ {
        self.wrap_page().into_iter().flat_map(move |page| page.selection_cells(row, source.clone()))
    }

    #[cfg(test)]
    pub(super) fn wrap_syntax_windows(&self) -> &[Range<u64>] {
        self.wrap_page().map_or(&[], LayoutPage::syntax_windows)
    }

    pub(super) fn wrap_paint_identity(&self) -> Option<(LayoutKey, u64)> {
        Some((self.wrap_page()?.key, self.wrap.context?.path_generation))
    }

    #[cfg(test)]
    pub(super) fn wrap_check_syntax_request(&self, source: Range<u64>, margin: usize) -> WrapResult<()> {
        self.wrap_page().ok_or(WrapError::Stale)?.check_syntax_request(source, margin)
    }

    /// Source intervals are never stretched across giant opaque coverage.
    /// Include empty ranges for carets at EOL; affinity is resolved for paint.
    pub(super) fn wrap_damage_source(&self, source: Range<u64>) -> Damage {
        let Some(page) = self.wrap_page() else {
            return Damage::default();
        };
        Damage {
            rows: page
                .rows()
                .iter()
                .enumerate()
                .filter_map(|(row, visual)| {
                    (source.start <= visual.source_coverage.end && visual.source_coverage.start <= source.end)
                        .then_some(row)
                })
                .collect(),
            ..Default::default()
        }
    }

    pub(super) fn wrap_source_scrollbar(&self) -> Option<scrollbar::Bar> {
        let page = self.wrap_page()?;
        let total = self.view.editor()?.document().hex_source(self.view.active_id().0).len;
        let extent = page.scroll_extent(total);
        let end = page.rows().last().map_or(extent.source_position, |row| row.source_coverage.end);
        let visible = end.saturating_sub(extent.source_position).max(1);
        Some(scrollbar::Bar::sized(
            0,
            self.text_rows() * self.ch(),
            visible,
            total.max(visible),
            extent.source_position,
            self.ui.min_thumb,
        ))
    }

    pub(super) fn wrap_font_changed(&mut self) {
        self.wrap.font = None;
        self.wrap.font_key = None;
        if self.wrap_owns_text() {
            self.wrap_queue_damage(Damage {
                rows: (0..self.text_rows()).collect(),
                status: true,
                ..Default::default()
            });
        }
    }

    pub(super) fn wrap_prepare_font(&mut self) {
        let (Some(ui), Some(editor)) = (self.ui_font.as_ref(), self.font.as_ref()) else {
            return;
        };
        let key = (ui.id(), editor.generation, editor.px.to_bits());
        if self.wrap.font_key != Some(key) {
            self.wrap.font = ui.variant(editor.px);
            self.wrap.font_key = Some(key);
        }
    }

    /// Paint into the existing retained body; ROOT copies/composites/presents
    /// exactly its queued damage. Spans already passed the wrapped syntax fence.
    #[cfg(test)]
    pub(super) fn wrap_paint_row(&mut self, row: usize, spans: &[&kwil_view_model::highlighting::Span]) -> bool {
        if !self.wrap_owns_text() {
            return false;
        }
        self.wrap_prepare_font();
        let body = self.body_area();
        let (cw, ch) = self.font.as_ref().map_or((8, 16), |font| (font.cw.max(1), font.ch.max(1)));
        let Some(context) = self.wrap.context else {
            paint_row(
                &mut self.body_pixels,
                body.width,
                row,
                cw,
                ch,
                None,
                None,
                self.font.as_mut(),
                theme_ui::chrome(),
                &[],
                &[],
                std::iter::empty(),
            );
            return true;
        };
        let live = self.wrap_live();
        let page =
            self.wrap.shared.as_ref().filter(|_| live).and_then(WrapHost::page).filter(|page| page.key == context.key);
        let cursors = self.wrap.shared.as_ref().filter(|_| live).and_then(WrapHost::visual_cursors).unwrap_or(&[]);
        let theme = theme_ui::chrome();
        let hits = self.search_panel.visible_hits(self.view.active_id(), context.key.source.revision);
        let font = self.wrap.font.as_mut().map(font::UiFont::monospace);
        paint_row(
            &mut self.body_pixels,
            body.width,
            row,
            cw,
            ch,
            page,
            font,
            self.font.as_mut(),
            theme,
            cursors,
            spans,
            hits.iter().map(|hit| hit.offset..hit.offset.saturating_add(hit.len)),
        );
        true
    }

    #[cfg(test)]
    pub(super) fn wrap_stop(&mut self) -> Option<Stop> {
        // Registered production owners must use pre-exit DocumentWorker
        // settlement. This fallback is only an unregistered, never-lent host.
        if self.wrap.workspace.is_some() {
            return None;
        }
        self.wrap.context = None;
        self.wrap.deferred_wake = false;
        self.wrap.shared.take().map(|host| Stop {
            ticket: host.stop(),
            failure: self.wrap.exception.drain.take(),
            cursor_damage: self.wrap.cursor_damage.take(),
        })
    }
}

/// Atom colour uses absolute source overlap. Opaque/invalid atoms never index
/// syntax using their marker text, whose bytes have no source equivalence.
fn atom_color(atom: &PaintAtom, spans: &[&kwil_view_model::highlighting::Span], fallback: u32) -> u32 {
    if atom.kind != AtomKind::Normal {
        return fallback;
    }
    let first = spans.partition_point(|span| span.range.end as u64 <= atom.source.start);
    spans
        .get(first)
        .filter(|span| (span.range.start as u64) <= atom.source.start && atom.source.start < span.range.end as u64)
        .map_or(fallback, |span| theme_ui::syntax_rgb(kwil_view_model::highlight_session::syntax_rgb(span.class)))
}

#[allow(clippy::too_many_arguments)]
fn paint_row(
    pixels: &mut [u32],
    stride: usize,
    row: usize,
    cw: usize,
    ch: usize,
    page: Option<&LayoutPage>,
    font: Option<&mut font::UiFont>,
    editor_font: Option<&mut Font>,
    theme: &UiTheme,
    cursors: &[VisualCursor],
    spans: &[&kwil_view_model::highlighting::Span],
    hits: impl Iterator<Item = Range<u64>>,
) {
    if stride == 0 || cw == 0 || ch == 0 {
        return;
    }
    let y = row.saturating_mul(ch);
    let height = ch.min((pixels.len() / stride).saturating_sub(y));
    if height == 0 {
        return;
    }
    let gutter = (GUTTER_COLS * cw).min(stride);
    let background = theme_ui::rgb(theme, UiRole::Background);
    let foreground = theme_ui::rgb(theme, UiRole::Foreground);
    fill(pixels, stride, Rect { x: 0, y, w: gutter, h: height }, theme_ui::rgb(theme, UiRole::GutterBackground));
    fill(pixels, stride, Rect { x: gutter, y, w: stride - gutter, h: height }, background);
    let Some(page) = page else { return };
    let Some(visual) = page.rows().get(row) else {
        return;
    };
    let columns = usize::from(page.key.geometry.columns);
    let right = gutter.saturating_add(columns * cw).min(stride);
    if let Some(font) = font {
        let baseline = font.baseline(y, ch);
        // A continuation row has no invented logical line number.
        if !visual.continuation {
            font.draw(
                pixels,
                stride,
                Rect { x: 0, y, w: gutter, h: height },
                0,
                baseline,
                &format!("{:>7} ", visual.logical_line + 1),
                theme_ui::rgb(theme, UiRole::GutterForeground),
            );
        }
    }
    if let Some(font) = editor_font {
        for atom in page.atoms(row) {
            let x = gutter + usize::from(atom.cells.start).min(columns) * cw;
            let end = (gutter + usize::from(atom.cells.end).min(columns) * cw).min(right);
            if x >= end || atom.is_tab() {
                continue;
            }
            let color = atom_color(atom, spans, foreground);
            let width = usize::from(atom.cells.end.saturating_sub(atom.cells.start));
            if atom.text().bytes().all(|byte| (32..127).contains(&byte)) {
                for (index, byte) in atom.text().bytes().take(width).enumerate() {
                    if byte != b' ' {
                        let left = x + index * cw;
                        if left < end {
                            copy_atom_tile(
                                pixels,
                                stride,
                                left,
                                y,
                                cw,
                                ch,
                                (end - left).min(cw),
                                height,
                                font.tile(byte, color, background),
                            );
                        }
                    }
                }
            } else {
                // Existing configured editor face/cache/worker; the display
                // payload is bounded and never laid out into source positions.
                let key = shaping::TileKey { text: atom.text().into(), columns: width, fg: color, bg: background };
                if let Some(tile) = font.unicode_tile(key, y) {
                    copy_atom_tile(pixels, stride, x, y, width * cw, ch, end - x, height, tile);
                } else {
                    copy_atom_tile(
                        pixels,
                        stride,
                        x,
                        y,
                        cw,
                        ch,
                        (end - x).min(cw),
                        height,
                        font.tile(b'?', color, background),
                    );
                }
            }
        }
    }
    for hit in hits {
        for cells in page.selection_cells(row, hit) {
            tint_cells(
                pixels,
                stride,
                gutter,
                y,
                height,
                cw,
                columns,
                cells,
                theme_ui::rgb(theme, UiRole::SearchMatch),
            );
        }
    }
    {
        for cursor in cursors {
            for cells in page.selection_cells(row, cursor.anchor.min(cursor.head)..cursor.anchor.max(cursor.head)) {
                tint_cells(
                    pixels,
                    stride,
                    gutter,
                    y,
                    height,
                    cw,
                    columns,
                    cells,
                    theme_ui::rgb(theme, UiRole::Selection),
                );
            }
        }
        for cursor in cursors {
            if let Some(point) = page.source_to_screen(cursor.head, cursor.head_affinity)
                && point.row == row
                && columns != 0
            {
                let x = (gutter + usize::from(point.cell) * cw).min(right.saturating_sub(2));
                fill(
                    pixels,
                    stride,
                    Rect { x, y, w: 2.min(right.saturating_sub(x)), h: height },
                    theme_ui::rgb(theme, UiRole::Cursor),
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn copy_atom_tile(
    pixels: &mut [u32],
    stride: usize,
    x: usize,
    y: usize,
    tile_width: usize,
    tile_height: usize,
    width: usize,
    height: usize,
    tile: &[u32],
) {
    let width = width.min(tile_width).min(stride.saturating_sub(x));
    let height = height.min(tile_height).min((pixels.len() / stride.max(1)).saturating_sub(y));
    if tile_width == 0 || width == 0 {
        return;
    }
    for line in 0..height.min(tile.len() / tile_width) {
        let target = (y + line) * stride + x;
        pixels[target..target + width].copy_from_slice(&tile[line * tile_width..line * tile_width + width]);
    }
}

#[allow(clippy::too_many_arguments)]
fn tint_cells(
    pixels: &mut [u32],
    stride: usize,
    gutter: usize,
    y: usize,
    height: usize,
    cw: usize,
    columns: usize,
    cells: Range<u16>,
    color: u32,
) {
    let start = (gutter + usize::from(cells.start).min(columns) * cw).min(stride);
    let end = (gutter + usize::from(cells.end).min(columns) * cw).min(stride);
    for line in y..y + height {
        for pixel in &mut pixels[line * stride + start..line * stride + end] {
            *pixel = font::blend(*pixel, color, 100);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn settle_host(
        mut ticket: WrapHostSettlementTicket,
    ) -> Result<(), kwil_view_model::wrap_host::WrapHostSettlementFailure> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match ticket.settle(deadline.saturating_duration_since(Instant::now())) {
                Err(failure) if matches!(failure.error, WrapError::ThreadExitPending) && Instant::now() < deadline => {
                    ticket = failure.ticket;
                    std::thread::park_timeout(
                        Duration::from_millis(1).min(deadline.saturating_duration_since(Instant::now())),
                    );
                }
                result => return result,
            }
        }
    }
    fn settle_workspace(
        mut ticket: WorkspaceWrapSettlementTicket,
    ) -> Result<
        kwil_view_model::workspace_wrap::WorkspaceWrapSettled,
        kwil_view_model::workspace_wrap::WorkspaceWrapSettlementFailure,
    > {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match ticket.settle(deadline.saturating_duration_since(Instant::now())) {
                Err(failure) if matches!(failure.error, WrapError::ThreadExitPending) && Instant::now() < deadline => {
                    ticket = *failure.ticket;
                    std::thread::park_timeout(
                        Duration::from_millis(1).min(deadline.saturating_duration_since(Instant::now())),
                    );
                }
                result => return result,
            }
        }
    }
    use kwil_view_model::visual_rows::{SOURCE_WINDOW, SnapshotSource, SourceIdentity};
    use std::{
        io,
        sync::{
            Condvar, Mutex,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        thread::ThreadId,
        time::{Duration, Instant},
    };

    fn geometry(app: &mut App, columns: usize, rows: usize) {
        app.size = (
            (GUTTER_COLS + columns) * 8 + app.ui.bar,
            app.ui.menu + app.ui.tabs + app.ui.status + app.ui.bar + rows * 16,
        );
        app.view.set_rows(rows);
        let body = app.body_area();
        app.body_pixels.resize(body.width * body.height, 0xabcdef);
    }

    fn context(app: &App) -> WrapHostContext {
        let source = app.view.editor().unwrap().document().hex_source(app.view.active_id().0);
        WrapHostContext {
            enabled: true,
            key: LayoutKey {
                source: SourceIdentity {
                    document_id: source.document,
                    source_epoch: source.epoch,
                    incarnation: 1,
                    revision: source.revision,
                },
                input_epoch: app.view.input_epoch(),
                body: BodyOwner::Text,
                geometry: LayoutGeometry { tab_size: 4, ..Default::default() },
                ..Default::default()
            },
            path_generation: 0,
            cursor_generation: 0,
        }
    }

    fn host(text: &str, columns: usize, rows: usize) -> (App, mpsc::Receiver<UserEvent>) {
        let (mut app, receiver) = crate::search_ui::host_dispatch_tests::host(text);
        app.wrap.manual_injection = true;
        app.settings.editor.word_wrap = true;
        geometry(&mut app, columns, rows);
        app.view.editor_mut().unwrap().set_cursors(vec![kwil_view_model::Cursor::caret(0)], 0).unwrap();
        app.wrap_sync(context(&app)).unwrap();
        (app, receiver)
    }

    /// These tests operate the actual App, fixed host worker and event proxy.
    /// Every source operation that can touch bytes asserts it is OFF the UI.
    struct Source {
        snapshot: SnapshotSource,
        ui: ThreadId,
        reads: AtomicUsize,
        gate: Option<Arc<Gate>>,
    }
    struct Gate {
        entered: mpsc::SyncSender<()>,
        release: Mutex<bool>,
        changed: Condvar,
    }
    impl Gate {
        fn open(&self) {
            *self.release.lock().unwrap() = true;
            self.changed.notify_all();
        }
    }
    impl Drop for Gate {
        fn drop(&mut self) {
            *self.release.get_mut().unwrap() = true;
            self.changed.notify_all();
        }
    }
    impl Source {
        fn capture(app: &App, gate: Option<Arc<Gate>>) -> Arc<Self> {
            let identity = app.wrap.context.unwrap().key.source;
            Arc::new(Self {
                snapshot: SnapshotSource::new(identity, app.view.editor().unwrap().document().snapshot()),
                ui: std::thread::current().id(),
                reads: AtomicUsize::new(0),
                gate,
            })
        }
    }
    impl WrapSource for Source {
        fn identity(&self) -> SourceIdentity {
            self.snapshot.identity()
        }
        fn len(&self) -> u64 {
            self.snapshot.len()
        }
        fn line_count(&self) -> u64 {
            self.snapshot.line_count()
        }
        fn run_layout(
            self: Arc<Self>,
            engine: kwil_view_model::visual_rows::LayoutEngine,
            request: kwil_view_model::visual_rows::TaskRequest,
            driver: &mut dyn kwil_view_model::visual_rows::TaskDriver,
        ) -> kwil_view_model::visual_rows::WorkerExit {
            let identity = self.identity();
            match FixtureSnapshot::try_with_owner(self, identity, engine.budget()) {
                Ok(source) => {
                    kwil_view_model::visual_rows::run_cooperative(FixtureTaskSource(source), engine, request, driver)
                }
                Err(refused) => kwil_view_model::visual_rows::WorkerExit {
                    engine,
                    source: refused.owner,
                    request: Some(request),
                    output: None,
                    end: kwil_view_model::visual_rows::WorkerEnd::Refused(refused.error),
                },
            }
        }
    }
    impl Source {
        fn before_read(&self, length: usize) -> io::Result<()> {
            assert_ne!(std::thread::current().id(), self.ui, "source read performed on GUI thread");
            assert!(length <= SOURCE_WINDOW);
            if self.reads.fetch_add(1, Ordering::AcqRel) == 0
                && let Some(gate) = &self.gate
            {
                let _ = gate.entered.try_send(());
                let (released, timeout) = gate
                    .changed
                    .wait_timeout_while(gate.release.lock().unwrap(), Duration::from_secs(3), |released| !*released)
                    .unwrap();
                if timeout.timed_out() && !*released {
                    return Err(io::Error::other("test read hold timed out"));
                }
            }
            Ok(())
        }
    }
    use kwil_view_model::{
        snapshot_layout_source::{SnapshotLayoutSource as FixtureSnapshot, SnapshotOwner as FixtureSnapshotOwner},
        visual_rows::{
            CooperativeSource as FixtureCooperative, LineSpan as FixtureLineSpan, Provenance as FixtureProvenance,
            Rank as FixtureRank, ReadWork as FixtureWork, SourceMetadata as FixtureMetadata, TaskPoll as FixturePoll,
        },
    };
    use std::sync::atomic::AtomicBool;
    impl FixtureSnapshotOwner for Source {
        fn snapshot(&self) -> &kwil_core_text::Snapshot {
            FixtureSnapshotOwner::snapshot(&self.snapshot)
        }
    }
    struct FixtureTaskSource(FixtureSnapshot<Arc<Source>>);
    impl kwil_view_model::visual_rows::SessionSource for FixtureTaskSource {
        fn into_transport(self) -> Arc<dyn WrapSource> {
            self.0.into_owner()
        }
    }
    impl FixtureCooperative for FixtureTaskSource {
        type LineQuery = <FixtureSnapshot<Arc<Source>> as FixtureCooperative>::LineQuery;
        type RankQuery = <FixtureSnapshot<Arc<Source>> as FixtureCooperative>::RankQuery;
        type ReadQuery = <FixtureSnapshot<Arc<Source>> as FixtureCooperative>::ReadQuery;
        type ProvenanceQuery = <FixtureSnapshot<Arc<Source>> as FixtureCooperative>::ProvenanceQuery;
        fn metadata(&self) -> FixtureMetadata {
            self.0.metadata()
        }
        fn new_line_query(&self, at: u64) -> Self::LineQuery {
            self.0.new_line_query(at)
        }
        fn new_rank_query(&self, at: u64) -> Self::RankQuery {
            self.0.new_rank_query(at)
        }
        fn new_provenance_query(&self, at: u64) -> Self::ProvenanceQuery {
            self.0.new_provenance_query(at)
        }
        fn new_read_query(
            &self,
            at: u64,
            length: usize,
            budget: &AllocationBudget,
        ) -> Result<Self::ReadQuery, WrapError> {
            self.0.new_read_query(at, length, budget)
        }
        fn poll_line(
            &self,
            query: &mut Self::LineQuery,
            work: &mut FixtureWork,
            cancel: &AtomicBool,
        ) -> FixturePoll<FixtureLineSpan> {
            assert_ne!(std::thread::current().id(), self.0.owner().ui, "line lookup performed on GUI thread");
            self.0.poll_line(query, work, cancel)
        }
        fn poll_rank(
            &self,
            query: &mut Self::RankQuery,
            work: &mut FixtureWork,
            cancel: &AtomicBool,
        ) -> FixturePoll<FixtureRank> {
            assert_ne!(std::thread::current().id(), self.0.owner().ui, "offset lookup performed on GUI thread");
            self.0.poll_rank(query, work, cancel)
        }
        fn poll_read(
            &self,
            query: &mut Self::ReadQuery,
            output: &mut [u8],
            work: &mut FixtureWork,
            cancel: &AtomicBool,
        ) -> FixturePoll<usize> {
            if let Err(error) = self.0.owner().before_read(output.len()) {
                return FixturePoll::Failed(kwil_view_model::visual_rows::InputError::Io(error).into());
            }
            self.0.poll_read(query, output, work, cancel)
        }
        fn poll_provenance(
            &self,
            query: &mut Self::ProvenanceQuery,
            work: &mut FixtureWork,
            cancel: &AtomicBool,
        ) -> FixturePoll<FixtureProvenance> {
            assert_ne!(std::thread::current().id(), self.0.owner().ui, "provenance lookup performed on GUI thread");
            self.0.poll_provenance(query, work, cancel)
        }
    }

    fn submit(app: &mut App, source: Arc<dyn WrapSource>) {
        if let Flow::Break(refusal) = app.wrap_drive(source) {
            panic!("unexpected admission refusal: {:?}", refusal.error);
        }
    }

    /// Mirrors the common body copy seam while invoking production row paint;
    /// no native window or font lookup is needed for mapping/overlay assertions.
    fn paint(app: &mut App) {
        let damage = std::mem::take(&mut app.pending);
        let context = app.wrap.context.unwrap();
        let rows = usize::from(context.key.geometry.rows);
        let stride = app.body_area().width;
        let ch = app.ch();
        if damage.scroll != 0 && !damage.full {
            let shift = damage.scroll.unsigned_abs() as usize * ch;
            let keep = rows * ch - shift;
            let (from, to) = if damage.scroll > 0 { (shift, 0) } else { (0, shift) };
            app.body_pixels.copy_within(from * stride..(from + keep) * stride, to * stride);
        }
        let dirty = if damage.full { (0..rows).collect() } else { damage.rows };
        for row in dirty {
            assert!(app.wrap_paint_row(row, &[]));
        }
        if app.wrap_frame_painted() {
            app.wrap_ready();
        }
    }

    fn until(app: &mut App, rx: &mpsc::Receiver<UserEvent>, done: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(app) {
            let event = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("actual worker must wake actual App within five seconds");
            match event {
                UserEvent::WrapReady => app.wrap_ready(),
                _ => panic!("unexpected wake in wrap-only App test"),
            }
            if app.wrap.cursor_damage.is_some() {
                paint(app);
            }
        }
    }

    fn sync(app: &mut App, rx: &mpsc::Receiver<UserEvent>, context: WrapHostContext) -> WrapHostSync {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match app.wrap_sync(context) {
                Ok(sync) => return sync,
                Err(WrapError::Busy) => {
                    match rx
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                        .expect("retirement must wake busy synchronization")
                    {
                        UserEvent::WrapReady => app.wrap_ready(),
                        _ => panic!("unexpected synchronization wake"),
                    }
                }
                Err(error) => panic!("unexpected synchronization failure: {error}"),
            }
        }
    }

    fn fill(app: &mut App, rx: &mpsc::Receiver<UserEvent>) -> Arc<Source> {
        let source = Source::capture(app, None);
        submit(app, source.clone());
        until(app, rx, |app| app.wrap_page().is_some());
        source
    }

    fn stop(app: &mut App) {
        if let Some(stop) = app.wrap_stop() {
            assert!(stop.failure.is_none());
            std::thread::spawn(move || {
                settle_host(stop.ticket).unwrap();
                drop(stop.cursor_damage);
            })
            .join()
            .unwrap();
        }
    }

    fn stop_live(app: &mut App) {
        let owner = app.wrap.workspace.take().expect("live manager");
        let host = app.wrap.shared.take().expect("live host");
        let ticket = match owner.stop(host) {
            Flow::Continue(ticket) => ticket,
            _ => panic!("same host must transfer"),
        };
        let receipt = std::thread::spawn(move || settle_workspace(ticket)).join().unwrap().unwrap();
        assert!(matches!(app.view.acknowledge_wrap_settled(receipt), Flow::Continue(())));
        assert!(app.view.wrap_cleanup_ready());
    }

    // SOURCE ONLY: ROOT owns fail-first proof of foreign receipt preservation.
    // Document transport binding controls live in document_ui's real fixtures.
    #[test]
    fn foreign_wrap_receipt_returns_whole_and_blocks_original_app_cleanup() {
        let (mut app, _rx) = host("original", 4, 3);
        app.wrap.workspace = Some(WorkspaceWrap::new(&mut app.view, app.wrap.shared.as_ref().unwrap()).unwrap());
        let (mut foreign, _foreign_rx) = host("foreign", 4, 3);
        let owner = WorkspaceWrap::new(&mut foreign.view, foreign.wrap.shared.as_ref().unwrap()).unwrap();
        let host = foreign.wrap.shared.take().unwrap();
        let ticket = match owner.stop(host) {
            Flow::Continue(ticket) => ticket,
            _ => panic!("matching foreign owner must transfer"),
        };
        let receipt = std::thread::spawn(move || settle_workspace(ticket)).join().unwrap().unwrap();
        let receipt = match app.view.accept_wrap_settlement(Ok(receipt)) {
            Flow::Break(receipt) => receipt,
            _ => panic!("foreign receipt must return its complete original owner"),
        };
        assert!(!app.view.wrap_cleanup_ready());
        assert!(!app.wrap_shutdown_ready());
        assert!(matches!(foreign.view.accept_wrap_settlement(receipt), Flow::Continue(_)));
        assert!(foreign.view.wrap_cleanup_ready());
        stop_live(&mut app);
    }

    #[test]
    fn real_app_narrow_reflow_unicode_hit_selection_and_exact_affinity() {
        let (mut app, rx) = host("a界e\u{301}\txyz\nsecond", 4, 3);
        let source = fill(&mut app, &rx);
        let page = app.wrap_page().unwrap();
        assert!(page.rows()[1].continuation);
        let wide = page.atoms(0).iter().find(|atom| atom.text() == "界").unwrap();
        let wide_source = wide.source.clone();
        assert_eq!(app.wrap_hit_test(0, usize::from(wide.cells.start) + 1).unwrap().byte, wide_source.start);
        assert_eq!(app.wrap_selection_cells(0, wide_source.clone()).collect::<Vec<_>>(), vec![wide.cells.clone()]);
        let edge = page.rows()[0].source_coverage.end;
        assert_eq!(app.wrap_caret(edge, CaretAffinity::Upstream).unwrap().row, 0);
        assert_eq!(app.wrap_caret(edge, CaretAffinity::Downstream).unwrap().row, 1);
        let reads = source.reads.load(Ordering::Acquire);
        paint(&mut app);
        assert_eq!(source.reads.load(Ordering::Acquire), reads, "paint/hit/selection did no source read");

        let old = app.wrap.context.unwrap();
        geometry(&mut app, 7, 3);
        let mut changed = old;
        changed.key.layout_generation += 1;
        sync(&mut app, &rx, changed);
        assert!(app.wrap_page().is_none(), "old-width page cannot paint during reflow");
        assert!(!app.pending.full, "page reflow does not force a full-window present");
        assert_eq!(app.pending.rows, vec![0, 1, 2], "every visual page row is cleared");
        fill(&mut app, &rx);
        assert_eq!(app.wrap_page().unwrap().key.geometry.columns, 7);
        stop(&mut app);
    }

    #[test]
    fn actual_worker_hold_preserves_accepted_motion_and_busy_edit_gate() {
        let (mut app, rx) = host("abcdefghijklmno", 3, 3);
        fill(&mut app, &rx);
        paint(&mut app);
        assert!(app.wrap_navigate("motion.down", 1, true));
        let (entered, held) = mpsc::sync_channel(1);
        let gate = Arc::new(Gate { entered, release: Mutex::new(false), changed: Condvar::new() });
        let source = Source::capture(&app, Some(gate.clone()));
        let before = app.view.editor().unwrap().primary_cursor().clone();
        let revision = app.view.editor().unwrap().document().revision();
        submit(&mut app, source);
        held.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(!app.wrap_mutation_allowed());
        assert_eq!(app.view.editor().unwrap().primary_cursor(), &before);
        assert_eq!(app.view.editor().unwrap().document().revision(), revision);
        assert!(app.wrap_navigate("motion.down", 1, false));
        assert_eq!(app.wrap.shared.as_ref().unwrap().pending_inputs(), 2);
        gate.open();
        until(&mut app, &rx, |app| app.wrap.shared.as_ref().unwrap().pending_inputs() == 1);
        assert_eq!(app.view.editor().unwrap().primary_cursor().anchor, before.anchor);
        assert!(app.view.editor().unwrap().primary_cursor().head > before.head);
        assert!(!app.wrap_mutation_allowed(), "second accepted movement remains queued");
        let next = Source::capture(&app, None);
        submit(&mut app, next);
        until(&mut app, &rx, |app| app.wrap.shared.as_ref().unwrap().pending_inputs() == 0);
        assert!(app.wrap_mutation_allowed());
        stop(&mut app);
    }

    #[test]
    fn stale_tab_worker_wake_never_publishes_or_edits_new_document() {
        let (mut app, rx) = host("old tab has wrapped source", 4, 3);
        let (entered, held) = mpsc::sync_channel(1);
        let gate = Arc::new(Gate { entered, release: Mutex::new(false), changed: Condvar::new() });
        let source = Source::capture(&app, Some(gate.clone()));
        submit(&mut app, source.clone());
        held.recv_timeout(Duration::from_secs(3)).unwrap();
        let old = app.view.active_id();
        app.view.file_command("file.new").unwrap();
        assert_ne!(app.view.active_id(), old);
        assert!(app.wrap_page().is_none(), "cheap fence hides stale page before sync");
        let before = app.view.editor().unwrap().primary_cursor().clone();
        gate.open();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(UserEvent::WrapReady) => {
                    app.wrap_ready();
                    break;
                }
                Ok(_) => {}
                Err(error) => panic!("stale worker never woke: {error}"),
            }
        }
        assert!(app.wrap_page().is_none());
        assert_eq!(app.view.editor().unwrap().primary_cursor(), &before);
        let next = context(&app);
        sync(&mut app, &rx, next);
        fill(&mut app, &rx);
        assert_eq!(app.wrap_page().unwrap().key.source.document_id, app.view.active_id().0);
        stop(&mut app);
    }

    #[test]
    fn long_cluster_opaque_coverage_never_authorizes_syntax_or_interior_hit() {
        let directory = crate::test_support::directory("kwil-wrap-ui-opaque").unwrap();
        let path = directory.join("cluster.txt");
        let text = format!("a{} tail", "\u{301}".repeat(4096));
        std::fs::write(&path, text.as_bytes()).unwrap();
        let (mut app, rx) = host(&text, 1, 3);
        let source = fill(&mut app, &rx);
        let opaque = app.wrap_page().unwrap().atoms(0).iter().find(|atom| atom.kind == AtomKind::Opaque).unwrap();
        let coverage = opaque.source.clone();
        assert!(coverage.end - coverage.start > 256);
        assert_eq!(app.wrap_hit_test(0, 0).unwrap().byte, coverage.start);
        assert!(app.wrap_caret(coverage.start + 1, CaretAffinity::Downstream).is_none());
        assert!(
            app.wrap_syntax_windows().iter().all(|window| window.end <= coverage.start || window.start >= coverage.end)
        );
        assert!(app.wrap_check_syntax_request(coverage.clone(), 0).is_err());
        assert!(app.wrap_check_syntax_request(coverage.start..coverage.start + 1, 1).is_err());
        let reads = source.reads.load(Ordering::Acquire);
        paint(&mut app);
        assert_eq!(source.reads.load(Ordering::Acquire), reads);
        assert_eq!(std::fs::read(&path).unwrap(), text.as_bytes(), "fixture bytes are preserved");
        stop(&mut app);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn scrolling_inside_one_logical_line_keeps_copy_damage_and_no_idle_frame() {
        let (mut app, rx) = host("abcdefghijklmnopqrstuvwx", 4, 3);
        fill(&mut app, &rx);
        paint(&mut app);
        assert!(app.wrap_wheel(1, 0));
        let source = Source::capture(&app, None);
        submit(&mut app, source.clone());
        until(&mut app, &rx, |app| app.wrap.shared.as_ref().unwrap().pending_inputs() == 0);
        assert!(!app.pending.full);
        assert_eq!(app.pending.scroll, 1);
        assert_eq!(app.pending.rows, vec![2]);
        assert!(app.wrap_page().unwrap().rows().iter().all(|row| row.logical_line == 0));
        paint(&mut app);
        let reads = source.reads.load(Ordering::Acquire);
        app.wrap_ready();
        assert_eq!(app.pending, Damage::default(), "empty drain requests no idle frame");
        assert_eq!(source.reads.load(Ordering::Acquire), reads);
        stop(&mut app);
    }

    #[test]
    fn stale_source_refusal_returns_exact_arc_and_disabled_wrap_starts_no_worker() {
        let (mut app, rx) = host("same source", 4, 3);
        fill(&mut app, &rx);
        let wrong: Arc<dyn WrapSource> = Arc::new(SnapshotSource::new(
            SourceIdentity { revision: u64::MAX, ..app.wrap.context.unwrap().key.source },
            app.view.editor().unwrap().document().snapshot(),
        ));
        let original = wrong.clone();
        app.wrap_fill(TopAnchor::default()).unwrap();
        let refusal = match app.wrap_drive(wrong) {
            Flow::Break(refusal) => refusal,
            _ => panic!("expected stale refusal"),
        };
        assert!(Arc::ptr_eq(refusal.owner.source(), &original));
        drop(refusal); // test-owned immutable source, no document cleanup authority
        stop(&mut app);

        let (mut disabled, _) = crate::search_ui::host_dispatch_tests::host("disabled");
        geometry(&mut disabled, 4, 3);
        let sync = disabled.wrap_sync(context(&disabled)).unwrap();
        assert_eq!(sync.availability, WrapHostAvailability::Disabled);
        assert!(disabled.wrap.shared.is_none());
        assert!(!disabled.wrap_navigate("motion.down", 1, false));
    }

    #[test]
    fn actual_app_pointer_extends_unicode_selection_and_paints_only_prepared_cells() {
        let (mut app, rx) = host("界xyz", 4, 3);
        fill(&mut app, &rx);
        paint(&mut app);
        let body = app.body_area();
        app.pointer = (body.x + (GUTTER_COLS + 2) * 8, body.y + 1);
        assert!(app.wrap_pointer(true));
        assert!(!app.wrap_mutation_allowed());
        let source = Source::capture(&app, None);
        submit(&mut app, source.clone());
        until(&mut app, &rx, |app| app.wrap.shared.as_ref().unwrap().mutation_gate() == MutationGate::Allowed);
        assert_eq!(app.view.editor().unwrap().primary_cursor().anchor, 0);
        assert_eq!(app.view.editor().unwrap().primary_cursor().head, "界".len() as u64);
        let theme = theme_ui::chrome();
        let selected =
            font::blend(theme_ui::rgb(theme, UiRole::Background), theme_ui::rgb(theme, UiRole::Selection), 100);
        assert_eq!(app.body_pixels[GUTTER_COLS * 8], selected);
        assert_eq!(app.body_pixels[(GUTTER_COLS + 1) * 8], selected, "both wide-glyph cells selected");
        assert_eq!(app.wrap_damage_source(0..3).rows, vec![0]);
        stop(&mut app);
    }

    #[test]
    fn markdown_split_uses_actual_text_width_and_preview_excludes_wrap_body() {
        let (mut app, rx) = host("# Markdown\nparagraph", 20, 3);
        app.view.editor_mut().unwrap().set_language(Some("markdown".into()));
        fill(&mut app, &rx);
        let full = app.wrap_page().unwrap().key.geometry.columns;
        assert!(app.dispatch_markdown("view.markdownSplit"));
        let mut split = app.wrap.context.unwrap();
        split.key.layout_generation += 1;
        sync(&mut app, &rx, split);
        assert!(app.wrap_owns_text());
        assert!(app.wrap.context.unwrap().key.geometry.columns < full);
        assert_eq!(u64::from(app.wrap.context.unwrap().key.geometry.columns), app.columns());
        assert!(app.dispatch_markdown("view.markdownPreview"));
        let preview = app.wrap.context.unwrap();
        let sync = sync(&mut app, &rx, preview);
        assert_eq!(sync.availability, WrapHostAvailability::BodyOwned(BodyOwner::MarkdownPreview));
        assert!(!app.wrap_owns_text());
        assert!(app.wrap_page().is_none());
        assert!(!app.wrap_pointer(false));
        assert!(!app.wrap_wheel(1, 0));
        assert!(!app.wrap_paint_row(0, &[]));
        stop(&mut app);
    }

    #[test]
    fn full_fifo_refuses_only_newest_app_input_and_escape_cancels_explicitly() {
        let (mut app, rx) = host("abcdefgh", 2, 3);
        fill(&mut app, &rx);
        for _ in 0..kwil_view_model::wrap_session::RELATIVE_CAPACITY {
            assert!(app.wrap_navigate("motion.down", 1, false));
        }
        let accepted = app.wrap.shared.as_ref().unwrap().pending_inputs();
        assert!(app.wrap_navigate("motion.up", 1, false));
        assert_eq!(app.wrap.shared.as_ref().unwrap().pending_inputs(), accepted);
        assert!(app.view.notice().contains("Full"));
        assert!(!app.wrap_mutation_allowed());
        assert!(app.wrap_cancel());
        assert_eq!(app.wrap.shared.as_ref().unwrap().pending_inputs(), 0);
        assert!(app.wrap_mutation_allowed());
        stop(&mut app);
    }

    #[test]
    fn short_eof_page_clears_exposed_blank_row_after_scroll_copy() {
        let (mut app, rx) = host("abcdefghijklmnopq", 4, 4);
        fill(&mut app, &rx);
        paint(&mut app);
        for step in 0..2 {
            if step == 1 {
                let stride = app.body_area().width;
                let start = 3 * app.ch() * stride + GUTTER_COLS * 8;
                // Distinct retained pixels make failure to clear observable
                // without reading a system font or creating a native window.
                app.body_pixels[start..start + 4 * 8].fill(0xabcdef);
            }
            assert!(app.wrap_wheel(1, 0));
            let source = Source::capture(&app, None);
            submit(&mut app, source);
            until(&mut app, &rx, |app| app.wrap.shared.as_ref().unwrap().pending_inputs() == 0);
            assert!(!app.pending.full);
            assert_eq!(app.pending.scroll, 1);
            assert!(app.pending.rows.contains(&3), "blank exposed row is damage even when page has fewer rows");
            paint(&mut app);
        }
        assert_eq!(app.wrap_page().unwrap().rows().len(), 3);
        let stride = app.body_area().width;
        let blank = 3 * app.ch() * stride + GUTTER_COLS * 8;
        assert_eq!(app.body_pixels[blank], bg());
        stop(&mut app);
    }

    #[test]
    fn prepared_atom_ascii_uses_the_installed_editor_font_tiles() {
        let (mut app, rx) = crate::search_ui::host_dispatch_tests::host("ABCD");
        app.settings.editor.word_wrap = true;
        app.view.set_editor_settings(&app.settings);
        app.view.editor_mut().unwrap().set_cursors(vec![kwil_view_model::Cursor::caret(0)], 0).unwrap();
        assert!(!app.wrap.manual_injection);
        let (ready, loaded) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let _ = ready.send(Font::load_named(14.0, "monospace"));
        });
        let font =
            loaded.recv_timeout(Duration::from_secs(5)).unwrap().expect("GUI test needs its supported monospace face");
        worker.join().unwrap();
        let (cw, ch, generation) = (font.cw, font.ch, font.generation);
        app.font = Some(font);
        app.size =
            ((GUTTER_COLS + 4) * cw + app.ui.bar, app.ui.menu + app.ui.tabs + app.ui.status + app.ui.bar + 3 * ch);
        let body = app.body_area();
        app.body_pixels.resize(body.width * body.height, 0);
        app.wrap_sync_live();
        app.wrap_lend_source();
        until(&mut app, &rx, |app| app.wrap_page().is_some());
        assert!(app.wrap.workspace.is_some());
        let expected = app.font.as_mut().unwrap().tile(b'B', fg(), bg()).to_vec();
        paint(&mut app);
        for y in 0..ch {
            let start = y * body.width + (GUTTER_COLS + 1) * cw;
            assert_eq!(&app.body_pixels[start..start + cw], &expected[y * cw..(y + 1) * cw]);
        }
        assert_eq!(app.font.as_ref().unwrap().generation, generation);
        assert_eq!(app.wrap_page().unwrap().key.geometry.font_generation, generation);
        assert_eq!(app.view.body_policy(), kwil_view_model::ViewBodyPolicy::Prepared);
        stop_live(&mut app);
    }

    #[test]
    fn actual_key_dispatch_distinguishes_vim_logical_j_from_counted_visual_gj() {
        let (mut app, rx) = crate::search_ui::host_dispatch_tests::host("abcdefghijkl\nXY");
        app.settings.editor.word_wrap = true;
        app.settings.keymap = kwil_core_commands::Preset::Vim;
        assert!(!app.wrap.manual_injection);
        app.view.set_editor_settings(&app.settings);
        geometry(&mut app, 3, 3);
        app.view.editor_mut().unwrap().set_cursors(vec![kwil_view_model::Cursor::caret(0)], 0).unwrap();
        app.keys = Some(kwil_core_commands::keymap::KeymapEngine::new(app.settings.keymap, &[]).unwrap());
        app.wrap_sync_live();
        app.wrap_lend_source();
        until(&mut app, &rx, |app| app.wrap_page().is_some());
        paint(&mut app);
        let press = |app: &mut App, value: &str| {
            app.key_after_settings(
                None,
                &HostKey { logical_key: Key::Character(value.into()), text: None, repeat: false },
            );
        };
        press(&mut app, "j");
        assert_eq!(app.view.editor().unwrap().primary_cursor().head, 13, "plain Vim j keeps logical line semantics");
        app.view.editor_mut().unwrap().set_cursors(vec![kwil_view_model::Cursor::caret(0)], 0).unwrap();
        app.wrap_sync_live();
        app.keep_caret_visible();
        app.wrap_lend_source();
        until(&mut app, &rx, |app| app.wrap_page().is_some());
        for key in ["2", "g", "j"] {
            press(&mut app, key);
        }
        until(&mut app, &rx, |app| app.wrap.shared.as_ref().unwrap().pending_inputs() == 0);
        assert_eq!(app.view.editor().unwrap().primary_cursor().head, 6, "counted gj follows prepared rows");
        assert_eq!(app.view.body_policy(), kwil_view_model::ViewBodyPolicy::Prepared);
        assert!(app.view.line(0).is_none());
        stop_live(&mut app);
    }
}
