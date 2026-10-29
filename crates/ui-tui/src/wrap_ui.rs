//! Prepared normal-text TUI adapter and event-loop ownership.
//!
//! Host paints prepared atoms and exact source spans; Runtime coordinates the
//! shared WorkspaceWrap owner, body policy, syntax fences and typed settlement.
//! Source reads, snapshot admission and retirement stay in the shared owners.
//! The existing document worker settles tickets before document cleanup.
//! No polling timer or additional worker is created. Production document wakes
//! carry no owner; the frontend's one bounded receiver restores settlement.

use super::theme_ui;
#[cfg(test)]
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use kwil_view_model::workspace_wrap::{WorkspaceWrap, WorkspaceWrapSettlementState, WorkspaceWrapSettlementTicket};
use kwil_view_model::{
    Editor,
    highlighting::Span,
    visual_rows::{
        AllocationBudget, AtomKind, CaretAffinity, LayoutKey, LayoutPage, SourcePoint, TopAnchor, VisualMotion,
        WrapError, WrapResult,
    },
    wrap_host::{
        WrapHost, WrapHostAvailability, WrapHostContext, WrapHostInputError, WrapHostSettlementTicket, WrapHostSync,
    },
    wrap_session::{CancelReason, CancelledInputs, MutationGate},
};
#[cfg(test)]
use kwil_view_model::{
    visual_rows::WrapSource,
    wrap_host::{WrapHostDrainFailure, WrapHostSourceRefusal, WrapHostUpdate},
};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
};
use std::ops::{ControlFlow, Range};
#[cfg(test)]
use std::sync::Arc;

const GUTTER: u16 = 8;
const SELECTION: Color = Color::Rgb(0x30, 0x58, 0x80);
const SEARCH: Color = Color::Rgb(0x65, 0x51, 0x20);
const CURRENT_SEARCH: Color = Color::Rgb(0x8a, 0x63, 0x20);

/// Rect excludes tabs, status and any preview or search side panel. These are
/// terminal cells; the shared model receives the exact remaining text width.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Geometry {
    pub body: Rect,
    pub text: Rect,
}
impl Geometry {
    pub fn new(body: Rect) -> Self {
        let gutter = body.width.min(GUTTER);
        Self { body, text: Rect::new(body.x.saturating_add(gutter), body.y, body.width - gutter, body.height) }
    }
    fn hit(self, x: u16, y: u16) -> Option<(usize, usize)> {
        (x >= self.text.x && x < self.text.right() && y >= self.text.y && y < self.text.bottom())
            .then(|| (usize::from(y - self.text.y), usize::from(x - self.text.x)))
    }
}

/// Borrowed decorations have the SAME layout identity as the prepared page.
/// Syntax and search spans are sorted, non-overlapping, visible-window results
/// from their existing workers. No source extents are inferred from row
/// coverage or displayed strings, and no whole-file hit list is scanned here.
pub(super) struct Paint<'a> {
    pub key: LayoutKey,
    pub syntax: &'a [&'a Span],
    pub search: &'a [Range<u64>],
    pub current_search: Option<Range<u64>>,
}
impl Paint<'_> {
    pub fn empty(key: LayoutKey) -> Self {
        Self { key, syntax: &[], search: &[], current_search: None }
    }
}

#[derive(Debug)]
pub(super) enum Input {
    /// An earlier body/modal owner, unwrapped mode, or an unrelated action.
    Pass,
    Accepted,
    Refused(WrapHostInputError),
}
impl Input {
    pub fn notice(&self) -> Option<&'static str> {
        match self {
            Self::Pass | Self::Accepted => None,
            Self::Refused(WrapHostInputError::Queue(kwil_view_model::wrap_session::QueueError::Full)) => {
                Some("Wrapped movement queue is full; this input was not accepted")
            }
            Self::Refused(_) => Some("Wrapped movement is busy or failed; retry or Escape to cancel"),
        }
    }
}

pub(super) fn mutation_notice(gate: MutationGate) -> Option<&'static str> {
    match gate {
        MutationGate::Allowed => None,
        MutationGate::RefusedPending => Some("Wrapped movement is pending; wait or Escape to cancel before editing"),
        MutationGate::RefusedFailed => Some("Wrapped movement failed; retry or Escape to cancel before editing"),
    }
}

pub(super) struct Host {
    shared: WrapHost,
    geometry: Geometry,
    context: Option<WrapHostContext>,
    reflow_anchor: Option<TopAnchor>,
    input_blocked: bool,
    synchronized: bool,
}
impl Host {
    pub fn shared_mut(&mut self) -> &mut WrapHost {
        &mut self.shared
    }
    pub fn shared(&self) -> &WrapHost {
        &self.shared
    }
    pub fn into_shared(self) -> WrapHost {
        self.shared
    }
    #[cfg(test)]
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> WrapResult<Self> {
        Self::with_budget(AllocationBudget::default(), wake)
    }
    pub fn with_budget(budget: AllocationBudget, wake: impl Fn() + Send + Sync + 'static) -> WrapResult<Self> {
        Ok(Self {
            shared: WrapHost::with_budget(budget, wake)?,
            geometry: Geometry::default(),
            context: None,
            reflow_anchor: None,
            input_blocked: false,
            synchronized: false,
        })
    }
    /// Call before every input/draw/wake. The shared host hides late results
    /// during raw ownership and an Editor loan; modal ownership only gates
    /// input, so a palette can still paint over the unchanged text body.
    pub fn sync(
        &mut self,
        editor: Option<&Editor>,
        mut context: WrapHostContext,
        body: Rect,
        input_blocked: bool,
    ) -> WrapResult<WrapHostSync> {
        let geometry = Geometry::new(body);
        context.key.geometry.columns = geometry.text.width;
        context.key.geometry.rows = geometry.text.height;
        self.input_blocked = input_blocked;
        self.geometry = geometry;
        if let Some(old) = self.context {
            let same_source = old.enabled
                && context.enabled
                && old.key.source == context.key.source
                && old.key.body == context.key.body
                && old.key.body_generation == context.key.body_generation
                && old.key.input_epoch == context.key.input_epoch
                && old.path_generation == context.path_generation
                && old.cursor_generation == context.cursor_generation;
            if !same_source {
                self.reflow_anchor = None;
            } else if old.key.geometry != context.key.geometry
                || old.key.layout_generation != context.key.layout_generation
            {
                self.reflow_anchor = self.shared.page().map(|page| page.anchor).or(self.reflow_anchor);
            }
        }
        // Record the fence even on Busy. No old page can draw at new geometry.
        self.context = Some(context);
        self.synchronized = false;
        let mut update = self.shared.sync(editor, context)?;
        if update.availability == WrapHostAvailability::Ready {
            if let Some(anchor) = self.reflow_anchor {
                self.shared.request_fill(anchor)?;
                self.reflow_anchor = None;
                update.needs_source = self.shared.needs_source();
            }
        } else {
            self.reflow_anchor = None;
        }
        self.synchronized = true;
        Ok(update)
    }
    pub fn geometry(&self) -> Geometry {
        self.geometry
    }
    #[cfg(test)]
    pub fn needs_source(&self) -> bool {
        self.synchronized && self.shared.needs_source()
    }
    pub fn owns_body(&self) -> bool {
        self.context
            .is_some_and(|context| context.enabled && context.key.body == kwil_view_model::visual_rows::BodyOwner::Text)
            && !matches!(
                self.shared.availability(),
                WrapHostAvailability::BodyOwned(_) | WrapHostAvailability::EditorUnavailable
            )
    }
    pub fn page(&self) -> Option<&LayoutPage> {
        if !self.synchronized {
            return None;
        }
        let page = self.shared.page()?;
        (self.context.is_some_and(|context| context.key == page.key)
            && page.key.geometry.columns == self.geometry.text.width
            && page.key.geometry.rows == self.geometry.text.height)
            .then_some(page)
    }
    /// Never stores source, even temporarily in an adapter retry slot.
    #[must_use = "return the complete refusal owner to the off-UI retirement lane"]
    #[cfg(test)]
    pub fn drive(&mut self, source: Arc<dyn WrapSource>) -> ControlFlow<WrapHostSourceRefusal, ()> {
        if !self.synchronized {
            return ControlFlow::Break(WrapHostSourceRefusal {
                owner: kwil_view_model::wrap_host::WrapHostSourceOwner::from_source(source),
                error: WrapError::Stale,
            });
        }
        self.shared.drive(source)
    }
    #[must_use = "retain complete failure owners; publication contains charged damage"]
    #[cfg(test)]
    pub fn ready(&mut self, editor: Option<&mut Editor>) -> ControlFlow<WrapHostDrainFailure, WrapHostUpdate> {
        self.shared.drain(if self.synchronized { editor } else { None })
    }
    pub fn mutation_gate(&self) -> MutationGate {
        if self.owns_body() && !self.synchronized { MutationGate::RefusedPending } else { self.shared.mutation_gate() }
    }
    pub fn cancel(&mut self) -> CancelledInputs {
        self.shared.cancel(CancelReason::Escape)
    }
    pub fn retry_relative(&mut self) -> WrapResult<()> {
        self.shared.retry_relative()
    }
    #[cfg(test)]
    pub fn request_fill(&mut self, anchor: TopAnchor) -> WrapResult<()> {
        self.shared.request_fill(anchor)
    }
    pub fn request_reveal(&mut self, point: SourcePoint) -> WrapResult<()> {
        self.shared.request_reveal(point)
    }
    pub fn key(&self) -> Option<LayoutKey> {
        self.context.map(|context| context.key)
    }
    pub fn context(&self) -> Option<WrapHostContext> {
        self.context
    }
    #[cfg(test)]
    pub fn syntax_windows(&self) -> &[Range<u64>] {
        self.page().map_or(&[], LayoutPage::syntax_windows)
    }
    pub fn check_syntax_request(&self, range: Range<u64>, margin: usize) -> WrapResult<()> {
        self.page().ok_or(WrapError::Stale)?.check_syntax_request(range, margin)
    }
    #[cfg(test)]
    pub fn selection_cells(&self, row: usize, source: Range<u64>) -> impl Iterator<Item = Range<u16>> + '_ {
        self.page().into_iter().flat_map(move |page| page.selection_cells(row, source.clone()))
    }
    /// Call only for a resolved STANDALONE invocation. Vim operators retain
    /// their existing logical motion semantics and go through mutation_gate.
    pub fn command(&mut self, id: &str, count: u32, extend: bool) -> Input {
        if self.input_blocked || !self.owns_body() {
            return Input::Pass;
        }
        let motion = match id {
            "motion.up" => VisualMotion::Up,
            "motion.down" => VisualMotion::Down,
            "motion.pageUp" => VisualMotion::PageUp,
            "motion.pageDown" => VisualMotion::PageDown,
            _ => return Input::Pass,
        };
        if !self.synchronized {
            return Input::Refused(WrapHostInputError::Wrap(WrapError::Stale));
        }
        match self.shared.navigate(motion, count.max(1), extend) {
            Ok(_) => Input::Accepted,
            Err(error) => Input::Refused(error),
        }
    }
    /// Existing fallback keys, only after configurable keymap dispatch returns
    /// Unbound. This does not steal a rebound Up key, Vim count or modal input.
    #[cfg(test)]
    pub fn fallback_key(&mut self, key: KeyEvent) -> Input {
        if key.kind == KeyEventKind::Release || self.input_blocked || !self.owns_body() {
            return Input::Pass;
        }
        if key.code == KeyCode::Esc && self.mutation_gate() != MutationGate::Allowed {
            self.cancel();
            return Input::Accepted;
        }
        if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) {
            return Input::Pass;
        }
        let id = match key.code {
            KeyCode::Up => "motion.up",
            KeyCode::Down => "motion.down",
            KeyCode::PageUp => "motion.pageUp",
            KeyCode::PageDown => "motion.pageDown",
            _ => return Input::Pass,
        };
        self.command(id, 1, key.modifiers.contains(KeyModifiers::SHIFT))
    }
    pub fn mouse(&mut self, event: MouseEvent) -> Input {
        if self.input_blocked || !self.owns_body() {
            return Input::Pass;
        }
        let Some((row, cell)) = self.geometry.hit(event.column, event.row) else {
            return Input::Pass;
        };
        if !self.synchronized {
            return Input::Refused(WrapHostInputError::Wrap(WrapError::Stale));
        }
        match event.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let delta = if event.kind == MouseEventKind::ScrollDown { 3 } else { -3 };
                match self.shared.scroll(delta) {
                    Ok(_) => Input::Accepted,
                    Err(error) => Input::Refused(error),
                }
            }
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left) => {
                let extend =
                    event.modifiers.contains(KeyModifiers::SHIFT) || matches!(event.kind, MouseEventKind::Drag(_));
                match self.shared.hit(row, cell, extend) {
                    Ok(()) => Input::Accepted,
                    Err(error) => Input::Refused(WrapHostInputError::Wrap(error)),
                }
            }
            _ => Input::Pass,
        }
    }
    /// Caret and selections use the shared PUBLISHED visual cursor set and
    /// exact affinities, never reconstructed logical positions. Returns false
    /// when another body owner must draw instead.
    pub fn draw(&self, frame: &mut ratatui::Frame, paint: &Paint<'_>) -> bool {
        if !self.owns_body() {
            return false;
        }
        let area = self.geometry.body.intersection(frame.area());
        let buffer = frame.buffer_mut();
        // Cell reset removes obsolete wide-glyph tails/selection after reflow;
        // ratatui's existing previous-frame diff still emits only changed cells.
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                buffer[(x, y)].reset();
                buffer[(x, y)].set_style(theme_ui::base());
            }
        }
        let Some(page) = self.page() else {
            return true;
        };
        let valid_paint = paint.key == page.key;
        for (row, prepared) in page.rows().iter().enumerate().take(usize::from(self.geometry.text.height)) {
            let y = self.geometry.text.y.saturating_add(row as u16);
            if y < area.y || y >= area.bottom() {
                continue;
            }
            let gutter_width = self.geometry.text.x - self.geometry.body.x;
            if gutter_width > 0 && self.geometry.body.x >= area.x && self.geometry.body.x < area.right() {
                let label = gutter_label(prepared.logical_line, prepared.continuation);
                buffer.set_stringn(
                    self.geometry.body.x,
                    y,
                    std::str::from_utf8(&label).expect("ASCII gutter"),
                    usize::from(gutter_width.min(area.right() - self.geometry.body.x)),
                    Style::default().fg(theme_ui::color(
                        kwil_core_commands::theme::UiRole::GutterForeground,
                        Color::Rgb(0x5b, 0x6b, 0x85),
                    )),
                );
            }
            for atom in page.atoms(row) {
                let x = self.geometry.text.x.saturating_add(atom.cells.start);
                let width = atom.cells.end.saturating_sub(atom.cells.start).min(area.right().saturating_sub(x));
                if width == 0 || x < area.x || x >= area.right() {
                    continue;
                }
                let mut style = theme_ui::base();
                if valid_paint
                    && atom.kind == AtomKind::Normal
                    && self.check_syntax_request(atom.source.clone(), 0).is_ok()
                {
                    let index = paint.syntax.partition_point(|span| span.range.end as u64 <= atom.source.start);
                    if let Some(span) =
                        paint.syntax.get(index).filter(|span| (span.range.start as u64) < atom.source.end)
                    {
                        let rgb = theme_ui::syntax(span.class);
                        style = style.fg(Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8));
                    }
                }
                if valid_paint {
                    let index = paint.search.partition_point(|range| range.end <= atom.source.start);
                    if paint.search[index..]
                        .iter()
                        .take_while(|range| range.start < atom.source.end)
                        .any(|range| overlaps(range, &atom.source))
                    {
                        style = style.bg(SEARCH);
                    }
                    if paint.current_search.as_ref().is_some_and(|range| overlaps(range, &atom.source)) {
                        style = style.bg(CURRENT_SEARCH);
                    }
                }
                if let Some(cursors) = self.shared.visual_cursors() {
                    let index = cursors.partition_point(|cursor| cursor.anchor.max(cursor.head) <= atom.source.start);
                    if cursors[index..]
                        .iter()
                        .take_while(|cursor| cursor.anchor.min(cursor.head) < atom.source.end)
                        .any(|cursor| {
                            overlaps(&(cursor.anchor.min(cursor.head)..cursor.anchor.max(cursor.head)), &atom.source)
                        })
                    {
                        style = style.bg(theme_ui::color(kwil_core_commands::theme::UiRole::Selection, SELECTION));
                    }
                }
                // Each atom starts at its PREPARED cell. set_stringn only
                // paints that bounded payload; it never determines source hits
                // or the next atom's position and never wraps a Paragraph.
                if !atom.is_tab() {
                    let (end, _) = buffer.set_stringn(x, y, atom.text(), usize::from(width), style);
                    if end == x {
                        buffer[(x, y)].set_symbol("?").set_style(style);
                    }
                }
                // Include tab cells and a wide glyph's continuation cells.
                for cell in x..x + width {
                    buffer[(cell, y)].set_style(style);
                }
            }
        }
        if !self.input_blocked
            && let Some(primary) = self
                .shared
                .primary_index()
                .and_then(|index| self.shared.visual_cursors().and_then(|cursors| cursors.get(index)))
            && let Some(point) = self.shared.caret(primary.head, primary.head_affinity)
        {
            let x = self.geometry.text.x.saturating_add(point.cell.min(self.geometry.text.width.saturating_sub(1)));
            let y = self.geometry.text.y.saturating_add(point.row as u16);
            // Render a full-width upstream endpoint at the final cell
            // of THIS row. The source point/affinity remain unchanged.
            if x >= self.geometry.text.x
                && x < self.geometry.text.right()
                && y >= area.y
                && y < area.bottom()
                && x >= area.x
                && x < area.right()
            {
                frame.set_cursor_position((x, y));
            }
        }
        true
    }
    /// Existing search results are absolute byte ranges. Recolour prepared
    /// cells directly; never ask the logical-line/grid painter to map them.
    pub fn paint_search(&self, frame: &mut ratatui::Frame, matches: impl Fn(Range<u64>) -> bool) {
        let Some(page) = self.page() else { return };
        let clip = self.geometry.body.intersection(frame.area());
        for (row, _) in page.rows().iter().enumerate() {
            let y = self.geometry.text.y.saturating_add(row as u16);
            if y < clip.y || y >= clip.bottom() {
                continue;
            }
            for atom in page.atoms(row) {
                if !matches(atom.source.clone()) {
                    continue;
                }
                for column in atom.cells.clone() {
                    let x = self.geometry.text.x.saturating_add(column);
                    if x >= clip.x && x < clip.right() {
                        let cell = &mut frame.buffer_mut()[(x, y)];
                        if cell.bg == Color::Reset {
                            cell.set_bg(SEARCH);
                        }
                    }
                }
            }
        }
    }
    #[must_use = "settle on ROOT's existing off-UI cleanup lane BEFORE documents"]
    pub fn stop(self) -> WrapHostSettlementTicket {
        self.shared.stop()
    }
}

fn overlaps(a: &Range<u64>, b: &Range<u64>) -> bool {
    a.start < a.end && a.start < b.end && b.start < a.end
}

fn gutter_label(logical_line: u64, continuation: bool) -> [u8; 8] {
    let mut label = [b' '; 8];
    if continuation {
        label[6] = b'>';
        return label;
    }
    let Some(mut number) = logical_line.checked_add(1) else {
        return *b"####### ";
    };
    for index in (0..7).rev() {
        label[index] = b'0' + (number % 10) as u8;
        number /= 10;
        if number == 0 {
            return label;
        }
    }
    // Explicit overflow marker instead of a truncated, incorrect line number.
    *b"####### "
}

/// Event-loop owner transferred WHOLE into the existing ShutdownTicket. Source
/// refusal and terminal drain owners live only in the shared WorkspaceWrap.
pub(super) struct Runtime {
    host: Option<Host>,
    source: Option<WorkspaceWrap>,
    pending_stop: Option<WorkspaceWrapSettlementTicket>,
    rollback: Option<WrapHostSettlementTicket>,
    rollback_error: Option<WrapError>,
    publication: Option<kwil_view_model::wrap_host::WrapHostPublication>,
    syntax_fence: Option<(WrapHostContext, TopAnchor, kwil_view_model::highlighting::LanguageChoice)>,
    progress: Option<(u64, u64)>,
    closed: bool,
}
impl Runtime {
    pub fn new(host: Host) -> Self {
        Self {
            host: Some(host),
            source: None,
            pending_stop: None,
            rollback: None,
            rollback_error: None,
            publication: None,
            syntax_fence: None,
            progress: None,
            closed: false,
        }
    }
    pub fn attach(&mut self, view: &mut kwil_view_model::Workspace) -> WrapResult<()> {
        self.source = Some(WorkspaceWrap::new(view, self.host.as_ref().ok_or(WrapError::Closed)?.shared())?);
        Ok(())
    }
    pub fn host(&self) -> Option<&Host> {
        self.host.as_ref()
    }
    pub fn suspend_syntax(&mut self) {
        self.syntax_fence = None;
    }
    pub fn syntax_update_needed(&mut self, choice: kwil_view_model::highlighting::LanguageChoice) -> bool {
        let next = self.host().and_then(|host| Some((host.context()?, host.page()?.anchor, choice)));
        if self.syntax_fence == next {
            return false;
        }
        self.syntax_fence = next;
        next.is_some()
    }
    pub fn owns_body(&self, view: &kwil_view_model::Workspace) -> bool {
        view.wrap_input_blocked()
            || (!view.hex_active()
                && (self.host.as_ref().is_some_and(Host::owns_body)
                    || view.body_policy() == kwil_view_model::ViewBodyPolicy::Prepared))
    }
    pub fn gate(&self) -> MutationGate {
        if self.closed {
            MutationGate::RefusedFailed
        } else {
            self.host.as_ref().map_or(MutationGate::RefusedPending, Host::mutation_gate)
        }
    }
    pub fn closed(&self) -> bool {
        self.closed || self.host.is_none()
    }
    fn feedback(view: &mut kwil_view_model::Workspace, error: &WrapError) -> bool {
        let message = format!("Wrap: {error}");
        if view.notice() == message {
            false
        } else {
            view.set_notice(message);
            true
        }
    }
    pub fn sync(&mut self, view: &mut kwil_view_model::Workspace, body: Rect, input_blocked: bool) -> bool {
        if view.wrap_input_blocked() || self.closed {
            return false;
        }
        if !view.hex_active() {
            let prepared =
                view.editor().map_or(view.body_policy() == kwil_view_model::ViewBodyPolicy::Prepared, |editor| {
                    editor.settings.editor.word_wrap
                });
            view.set_body_policy(if prepared {
                kwil_view_model::ViewBodyPolicy::Prepared
            } else {
                kwil_view_model::ViewBodyPolicy::LogicalLines
            });
        }
        let (Some(host), Some(source)) = (&mut self.host, &mut self.source) else { return false };
        let text = Geometry::new(body).text;
        let geometry = kwil_view_model::visual_rows::LayoutGeometry {
            columns: text.width,
            rows: text.height,
            tab_size: 4,
            width_policy_generation: 1,
            ..Default::default()
        };
        let context = match source.prepare_context(view, geometry, kwil_view_model::visual_rows::BodyOwner::Text) {
            Ok(context) => context,
            Err(error) => return Self::feedback(view, &error),
        };
        let external_reveal = host.context.is_some_and(|old| {
            old.key.source == context.key.source && old.cursor_generation != context.cursor_generation
        });
        let sync = match host.sync(view.editor(), context, body, input_blocked) {
            Ok(sync) => sync,
            Err(error) => return Self::feedback(view, &error),
        };
        if external_reveal
            && sync.availability == WrapHostAvailability::Ready
            && let Some(editor) = view.editor()
            && let Err(error) = host
                .request_reveal(SourcePoint { byte: editor.primary_cursor().head, affinity: CaretAffinity::Downstream })
        {
            return Self::feedback(view, &error);
        }
        let mut changed = false;
        if let Some(cancelled) = sync.cancelled.filter(|cancelled| cancelled.records != 0) {
            view.set_notice(format!(
                "Wrapped movement cancelled ({:?}): {} queued input(s)",
                cancelled.reason, cancelled.records
            ));
            changed = true;
        }
        if let Err(error) = source.lend_source(view, context, host.shared_mut()) {
            changed |= Self::feedback(view, &error);
        }
        changed
    }
    pub fn ready(&mut self, view: &mut kwil_view_model::Workspace) -> bool {
        if self.closed || view.wrap_input_blocked() {
            return false;
        }
        let (Some(host), Some(source)) = (&mut self.host, &mut self.source) else { return false };
        match source.drain_host(host.shared_mut(), view.editor_mut()) {
            Ok(update) => {
                let mut changed = update.publication.is_some() || update.progress.is_some();
                if update.publication.is_some() {
                    if let Some((scanned, target)) = self.progress.take()
                        && view.notice() == format!("Wrapping: {scanned} / {target} bytes")
                    {
                        view.set_notice(String::new());
                    }
                } else if let Some(progress) = update.progress {
                    self.progress = Some((progress.scanned, progress.target));
                    view.set_notice(format!("Wrapping: {} / {} bytes", progress.scanned, progress.target));
                }
                self.publication = update.publication.or(self.publication.take());
                if let Some(error) = update.failure {
                    changed |= Self::feedback(view, &error);
                }
                changed
            }
            Err(error) => {
                self.closed = source.failed();
                Self::feedback(view, &error)
            }
        }
    }
    pub fn command(&mut self, id: &str, count: u32, extend: bool) -> Input {
        self.host
            .as_mut()
            .map_or(Input::Refused(WrapHostInputError::Wrap(WrapError::Closed)), |host| host.command(id, count, extend))
    }
    pub fn cancel(&mut self) {
        if let Some(host) = &mut self.host {
            host.cancel();
        }
    }
    pub fn retry_relative(&mut self) -> WrapResult<()> {
        self.host.as_mut().ok_or(WrapError::Closed)?.retry_relative()
    }
    pub fn painted(&mut self) {
        self.publication = None;
    }
    pub fn rollback_off_ui(&mut self, timeout: std::time::Duration) -> Result<(), &WrapError> {
        if self.source.is_some() || self.pending_stop.is_some() {
            return Ok(());
        }
        if let Some(host) = self.host.take() {
            self.rollback = Some(host.stop());
        }
        if let Some(ticket) = self.rollback.take()
            && let Err(failure) = ticket.settle(timeout)
        {
            self.rollback = Some(failure.ticket);
            self.rollback_error.get_or_insert(failure.error);
            return Err(self.rollback_error.as_ref().unwrap());
        }
        self.rollback_error = None;
        Ok(())
    }
    pub fn mouse(&mut self, event: MouseEvent) -> Input {
        self.host.as_mut().map_or(Input::Pass, |host| host.mouse(event))
    }
    pub fn status(&self, view: &kwil_view_model::Workspace) -> String {
        let mut text = view.tab_title(view.active_id());
        if let Some(host) = &self.host
            && let Some(primary) =
                host.shared.primary_index().and_then(|index| host.shared.visual_cursors()?.get(index))
            && let Some(point) = host.shared.caret(primary.head, primary.head_affinity)
            && let Some(row) = host.page().and_then(|page| page.rows().get(point.row))
        {
            text.push_str(&format!(" | Ln {} | wrapped cell {}", row.logical_line + 1, point.cell + 1));
        }
        if !view.notice().is_empty() {
            text.push_str(" | ");
            text.push_str(view.notice());
        }
        text
    }
    /// Begin only after an accepted exit/save decision. Full keeps the one
    /// exact ticket here and retries only when the existing event loop wakes.
    pub fn shutdown_step(&mut self, view: &mut kwil_view_model::Workspace, timeout: std::time::Duration) -> bool {
        view.begin_wrap_shutdown();
        if !view.wrap_shutdown_can_submit() {
            return false;
        }
        if self.pending_stop.is_none()
            && let (Some(host), Some(source)) = (self.host.take(), self.source.take())
        {
            let (geometry, context, reflow_anchor, input_blocked, synchronized) =
                (host.geometry(), host.context, host.reflow_anchor, host.input_blocked, host.synchronized);
            let shared = host.into_shared();
            match source.stop(shared) {
                ControlFlow::Continue(ticket) => self.pending_stop = Some(ticket),
                ControlFlow::Break((source, shared)) => {
                    self.source = Some(source);
                    self.host = Some(Host { shared, geometry, context, reflow_anchor, input_blocked, synchronized });
                    self.closed = true;
                    return Self::feedback(view, &WrapError::Stale);
                }
            }
        }
        if let Some(ticket) = self.pending_stop.take()
            && let ControlFlow::Break(ticket) = view.submit_wrap_settlement(ticket, timeout)
        {
            self.pending_stop = Some(ticket);
        }
        false
    }
    /// Existing OFF-UI shutdown lane; retain this Runtime and Workspace on any
    /// failed phase. The document worker's result is processed by that lane.
    pub fn shutdown_ready(&self, view: &kwil_view_model::Workspace) -> bool {
        view.wrap_settlement_state() == WorkspaceWrapSettlementState::Ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn settle_host(
        mut ticket: kwil_view_model::wrap_host::WrapHostSettlementTicket,
    ) -> Result<(), kwil_view_model::wrap_host::WrapHostSettlementFailure> {
        let deadline = Instant::now() + DEADLINE;
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
    use kwil_view_model::{
        Cursor, Document,
        visual_rows::{BodyOwner, LayoutGeometry, SourceIdentity},
    };
    use ratatui::{Terminal, backend::TestBackend};
    use std::{
        io,
        sync::{
            Condvar, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread::ThreadId,
        time::{Duration, Instant},
    };

    const DEADLINE: Duration = Duration::from_secs(5);

    /// The actual frontend dispatcher, Workspace adapter, document worker and
    /// TestBackend, sharing one aggregate even when wrapping is disabled.
    struct BoundedFrontend {
        view: kwil_view_model::Workspace,
        search: crate::search_ui::SearchHost,
        keys: kwil_core_commands::keymap::KeymapEngine,
        document: crate::DocumentLane,
        runtime: Option<Runtime>,
        events: std::sync::mpsc::Receiver<crate::Msg>,
        wake: std::sync::mpsc::Sender<crate::Msg>,
        root: std::path::PathBuf,
        budget: AllocationBudget,
    }
    impl BoundedFrontend {
        fn starting() -> Self {
            let budget = AllocationBudget::default();
            let mut document = crate::DocumentLane::new(&budget).unwrap();
            let mut view = kwil_view_model::Workspace::new(crate::View::new(None)).unwrap();
            let root = crate::test_support::directory("kwil-tui-bounded").unwrap();
            let (wake, events) = std::sync::mpsc::channel();
            let sender = wake.clone();
            document
                .started(view.start_bounded_document_worker_at(Some(root.clone()), &budget, move || {
                    sender
                        .send(crate::Msg::DocumentReady)
                        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "bounded fixture wake closed"))
                }))
                .unwrap();
            Self {
                view,
                document,
                search: Default::default(),
                keys: kwil_core_commands::keymap::KeymapEngine::new(kwil_core_commands::Preset::VsCode, &[]).unwrap(),
                runtime: None,
                events,
                wake,
                root,
                budget,
            }
        }
        fn new() -> Self {
            let mut fixture = Self::starting();
            fixture.until(|fixture| fixture.view.recovery_ready());
            fixture
        }
        fn event(&mut self, deadline: Instant) {
            match self.events.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap() {
                crate::Msg::DocumentReady => {
                    self.document.ready(&mut self.view, &mut self.search, &mut self.keys).unwrap();
                }
                crate::Msg::WrapReady => {
                    self.runtime.as_mut().unwrap().ready(&mut self.view);
                }
                _ => panic!("bounded fixture received a payload or unexpected event"),
            }
        }
        fn until(&mut self, predicate: impl Fn(&Self) -> bool) {
            let deadline = Instant::now() + DEADLINE;
            while !predicate(self) {
                self.event(deadline);
            }
        }
        fn enable(&mut self) {
            self.view.editor_mut().unwrap().settings.editor.word_wrap = true;
            self.view.set_body_policy(kwil_view_model::ViewBodyPolicy::Prepared);
            let wake = self.wake.clone();
            let mut runtime = Runtime::new(
                Host::with_budget(self.budget.clone(), move || {
                    let _ = wake.send(crate::Msg::WrapReady);
                })
                .unwrap(),
            );
            runtime.attach(&mut self.view).unwrap();
            self.runtime = Some(runtime);
        }
        fn finish(self) {
            // A fixture may observe the real worker's final notification just
            // before thread exit. Keep the exact ticket and perform explicit
            // test-only retries under one original deadline; production never
            // arms this bounded teardown backoff.
            let Self { view, search, document, runtime, events, root, .. } = self;
            let ticket = crate::ShutdownTicket {
                document: Some(document),
                highlighting: None,
                search_binding: None,
                search_release_refusal: None,
                terminal_panic: None,
                wrap: runtime,
                view: Some(view),
                search: Some(search),
                worker: None,
                rx: Some(events),
                stopped: None,
                document_started: false,
                terminal_failure: None,
            };
            let deadline = Instant::now() + DEADLINE;
            std::thread::spawn(move || {
                let ticket = crate::test_support::expect_cleanup(crate::test_support::finish_bounded_ticket_off_ui(
                    ticket, deadline,
                ));
                assert!(ticket.document.as_ref().unwrap().settled);
                assert!(ticket.view.is_none());
            })
            .join()
            .unwrap();
            assert!(root.starts_with(std::env::temp_dir()));
            assert!(root.file_name().unwrap().to_string_lossy().starts_with("kwil-tui-bounded-"));
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn wrap_tui_bounded_actual_worker_one_slot_event_only_and_exception_preadmission() {
        let mut fixture = BoundedFrontend::starting();
        assert!(!fixture.view.editor().unwrap().settings.editor.word_wrap);
        // The first real result remains in the ONE original worker slot until
        // an owner-free wake admits one receive. No sleeps/status polling.
        assert!(matches!(fixture.events.recv_timeout(DEADLINE).unwrap(), crate::Msg::DocumentReady));
        // False admission must leave the exact charged original slot intact;
        // the frontend has no source-bearing exception cell.
        let before = fixture.document.transport.as_ref().unwrap().status();
        let info = fixture.document.transport.as_ref().unwrap().result_info();
        assert!(matches!(
            fixture.document.transport.as_mut().unwrap().receive(&mut fixture.view, false),
            Err(crate::DocumentResultSettlementReason::ConsumerRefused)
        ));
        assert_eq!(fixture.document.transport.as_ref().unwrap().result_info(), info);
        assert_eq!(fixture.document.transport.as_ref().unwrap().status().occupied, before.occupied);
        fixture.document.ready(&mut fixture.view, &mut fixture.search, &mut fixture.keys).unwrap();
        fixture.until(|fixture| fixture.view.recovery_ready());
        // TestBackend paints only cached/prepared cells, never consumes a
        // document payload or starts a frame/poll loop during idle.
        fixture.enable();
        fixture.runtime.as_mut().unwrap().sync(&mut fixture.view, Rect::new(0, 1, 12, 3), false);
        fixture.until(|fixture| fixture.runtime.as_ref().unwrap().host().unwrap().page().is_some());
        let mut terminal = Terminal::new(TestBackend::new(12, 5)).unwrap();
        terminal
            .draw(|frame| {
                let host = fixture.runtime.as_ref().unwrap().host().unwrap();
                assert!(host.draw(frame, &Paint::empty(host.key().unwrap())));
            })
            .unwrap();
        assert!(fixture.view.line(0).is_none());
        fixture.finish();
    }

    #[test]
    fn wrap_tui_bounded_startup_refusal_keeps_exact_request_transport_and_workspace_worker() {
        let mut fixture = BoundedFrontend::new();
        let transport = fixture.document.transport.take().unwrap();
        let request = Box::new(crate::DocumentRequest::Prepare { root: Some(fixture.root.clone()) });
        let identity = &*request as *const crate::DocumentRequest;
        // Exercise the public started-failure owner at the frontend boundary;
        // model fixtures separately inject the actual post-spawn refusal.
        let error = fixture
            .document
            .started(Err(kwil_view_model::workspace_document::WorkspaceDocumentStartFailure::PrepareRefused(
                kwil_view_model::workspace_document::WorkspaceDocumentPrepareRefusal {
                    transport,
                    error: std::sync::mpsc::TrySendError::Full(request),
                },
            )))
            .unwrap_err();
        assert!(error.to_string().contains("exact started owner retained"));
        let Some(std::sync::mpsc::TrySendError::Full(request)) = &fixture.document.refused_prepare else {
            panic!("lost exact request")
        };
        assert_eq!(&**request as *const crate::DocumentRequest, identity);
        assert!(fixture.document.transport.is_some());
        assert!(matches!(
            fixture.view.start_bounded_document_worker_at(Some(fixture.root.clone()), &fixture.budget, || Ok(())),
            Err(kwil_view_model::workspace_document::WorkspaceDocumentStartFailure::NotStarted {
                error: kwil_view_model::workspace_document::WorkspaceDocumentStartError::WorkerAlreadyPresent,
                ..
            })
        ));
        fixture.finish();
    }

    #[test]
    fn wrap_tui_bounded_failed_wrap_retry_accepts_workspace_receipt_before_token_ack_outside_body_gate() {
        let mut fixture = BoundedFrontend::new();
        fixture.enable();
        let runtime = fixture.runtime.as_mut().unwrap();
        let host = runtime.host.as_mut().unwrap();
        let context = runtime
            .source
            .as_mut()
            .unwrap()
            .prepare_context(
                &fixture.view,
                LayoutGeometry { columns: 4, rows: 3, tab_size: 4, ..Default::default() },
                BodyOwner::Text,
            )
            .unwrap();
        assert!(host.sync(fixture.view.editor(), context, Rect::new(0, 1, 12, 3), false).unwrap().needs_source);
        let source = Arc::new(Bytes::new(context.key.source, b"abcdef"));
        source.gate.arm();
        assert!(matches!(host.drive(source.clone()), ControlFlow::Continue(())));
        source.gate.entered();
        // An accepted exit lives outside body/modal gates. Force the existing
        // worker's wrap timeout while its real source remains held.
        fixture.view.begin_wrap_shutdown();
        fixture.until(|fixture| fixture.view.wrap_shutdown_can_submit());
        fixture.runtime.as_mut().unwrap().shutdown_step(&mut fixture.view, Duration::ZERO);
        fixture.until(|fixture| fixture.document.wrap_failed());
        assert!(fixture.view.wrap_input_blocked());
        assert!(!fixture.view.wrap_cleanup_ready());
        assert!(fixture.document.retry.is_none());
        let kwil_core_commands::vim::VimResult::Command(retry) = fixture.keys.invoke_named("view.retryWrap") else {
            panic!("registered retry command")
        };
        let retry = crate::invoke(&mut fixture.view, retry, false, Duration::ZERO, kwil_core_commands::Preset::VsCode);
        assert!(matches!(retry, crate::KeyResult::RetryWrap), "body fence must not hide explicit retry");
        let kwil_core_commands::vim::VimResult::Command(quit) = fixture.keys.invoke_named("file.quit") else {
            panic!("registered quit command")
        };
        assert!(
            matches!(
                crate::invoke(&mut fixture.view, quit, false, Duration::ZERO, kwil_core_commands::Preset::VsCode),
                crate::KeyResult::Quit
            ),
            "accepted exit command stays outside the body gate"
        );
        let intent = crate::resolve_bounded_wrap(
            &mut fixture.document,
            &mut fixture.view,
            fixture.runtime.as_mut(),
            retry,
            Duration::ZERO,
            kwil_core_commands::Preset::VsCode,
        );
        assert!(matches!(intent, crate::KeyResult::Changed(true)));
        assert!(fixture.document.retry.is_some());
        assert!(!fixture.view.wrap_cleanup_ready(), "retry request is not a matching Workspace receipt");
        source.gate.release();
        // The accepted receipt still has a producer-owned carrier. Consume
        // scalar events through its retirement/capacity wake before requiring
        // token ACK; the original deadline and all receipt assertions remain.
        let deadline = Instant::now() + DEADLINE;
        while !fixture.view.wrap_cleanup_ready() || fixture.document.retry.is_some() {
            let status = fixture.document.transport.as_ref().unwrap().status();
            if status.failure == Some(crate::DocumentResultSettlementReason::WrapThreadExitPending)
                && !status.retry_pending
                && fixture.document.retry.is_none()
            {
                // Explicit test caller retry, not a production/native-exit
                // wake. Each attempt retains the same original deadline.
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero(), "wrap retry original deadline");
                std::thread::park_timeout(Duration::from_millis(1).min(remaining));
                fixture.document.retry_wrap(deadline.saturating_duration_since(Instant::now())).unwrap();
            }
            fixture.event(deadline);
        }
        assert!(fixture.document.retry.is_none(), "only accepted matching receipt consumes the private ACK token");
        assert!(!fixture.document.transport.as_ref().unwrap().status().retry_acknowledgement_required);
        assert!(fixture.document.transport.is_some(), "document cleanup cannot precede receipt ACK");
        fixture.finish();
    }

    #[derive(Default)]
    struct Wakes {
        count: Mutex<u64>,
        ready: Condvar,
    }
    impl Wakes {
        fn wake(&self) {
            let mut count = self.count.lock().unwrap();
            *count += 1;
            self.ready.notify_all();
        }
        fn wait(&self, seen: &mut u64, deadline: Instant) {
            let count = self.count.lock().unwrap();
            let remaining = deadline.saturating_duration_since(Instant::now());
            let (count, _) = self.ready.wait_timeout_while(count, remaining, |count| *count == *seen).unwrap();
            assert_ne!(*count, *seen, "bounded test deadline waiting for the actual wrap worker");
            *seen = *count;
        }
    }

    #[derive(Default)]
    struct ReadState {
        armed: bool,
        entered: bool,
        released: bool,
    }
    #[derive(Default)]
    struct ReadGate {
        state: Mutex<ReadState>,
        changed: Condvar,
    }
    impl ReadGate {
        fn arm(&self) {
            *self.state.lock().unwrap() = ReadState { armed: true, entered: false, released: false };
        }
        fn release(&self) {
            self.state.lock().unwrap().released = true;
            self.changed.notify_all();
        }
        fn entered(&self) {
            let state = self.state.lock().unwrap();
            let (state, _) = self.changed.wait_timeout_while(state, DEADLINE, |state| !state.entered).unwrap();
            assert!(state.entered, "worker did not reach controlled source read");
        }
        fn read(&self) -> io::Result<()> {
            let mut state = self.state.lock().unwrap();
            if !state.armed {
                return Ok(());
            }
            state.entered = true;
            self.changed.notify_all();
            let (state, _) = self.changed.wait_timeout_while(state, DEADLINE, |state| !state.released).unwrap();
            if state.released {
                Ok(())
            } else {
                Err(io::Error::new(io::ErrorKind::TimedOut, "controlled read deadline"))
            }
        }
    }

    /// All metadata is cached. Every actual read asserts it is on the one
    /// shared worker, including reads triggered by draw, hit and resize tests.
    struct Bytes {
        key: SourceIdentity,
        bytes: Vec<u8>,
        lines: Vec<u64>,
        ui: ThreadId,
        worker: Mutex<Option<ThreadId>>,
        reads: AtomicUsize,
        fail_next: AtomicBool,
        gate: Arc<ReadGate>,
    }
    impl Bytes {
        fn new(key: SourceIdentity, text: &[u8]) -> Self {
            let mut lines = vec![0];
            lines.extend(text.iter().enumerate().filter_map(|(i, b)| (*b == b'\n').then_some(i as u64 + 1)));
            Self {
                key,
                bytes: text.to_vec(),
                lines,
                ui: std::thread::current().id(),
                worker: Mutex::new(None),
                reads: AtomicUsize::new(0),
                fail_next: AtomicBool::new(false),
                gate: Arc::new(ReadGate::default()),
            }
        }
        fn worker_only(&self) {
            let current = std::thread::current().id();
            assert_ne!(self.ui, current, "UI attempted source I/O/coordinate lookup");
            let mut worker = self.worker.lock().unwrap();
            if let Some(previous) = *worker {
                assert_eq!(previous, current, "source escaped the single shared worker");
            } else {
                *worker = Some(current);
            }
        }
    }
    impl WrapSource for Bytes {
        fn identity(&self) -> SourceIdentity {
            self.key
        }
        fn len(&self) -> u64 {
            self.bytes.len() as u64
        }
        fn line_count(&self) -> u64 {
            self.lines.len() as u64
        }
        fn run_layout(
            self: Arc<Self>,
            engine: kwil_view_model::visual_rows::LayoutEngine,
            request: kwil_view_model::visual_rows::TaskRequest,
            driver: &mut dyn kwil_view_model::visual_rows::TaskDriver,
        ) -> kwil_view_model::visual_rows::WorkerExit {
            kwil_view_model::visual_rows::run_cooperative(FixtureTaskSource(self), engine, request, driver)
        }
    }
    impl Bytes {
        fn line_start(&self, line: u64) -> io::Result<Option<u64>> {
            self.worker_only();
            Ok(self.lines.get(line as usize).copied())
        }
        fn line_of(&self, byte: u64) -> io::Result<u64> {
            self.worker_only();
            Ok(self.lines.partition_point(|start| *start <= byte).saturating_sub(1) as u64)
        }
        fn read(&self, at: u64, output: &mut [u8]) -> io::Result<usize> {
            self.worker_only();
            self.gate.read()?;
            if self.fail_next.swap(false, Ordering::AcqRel) {
                return Err(io::Error::other("controlled source failure"));
            }
            self.reads.fetch_add(1, Ordering::AcqRel);
            let at = usize::try_from(at).map_err(io::Error::other)?;
            let count = output.len().min(5).min(self.bytes.len().saturating_sub(at));
            output[..count].copy_from_slice(&self.bytes[at..at + count]);
            Ok(count)
        }
    }
    // Concrete, worker-owned fixture adapter; no dependency cfg(test) helper.
    use kwil_view_model::visual_rows::{
        CooperativeSource as FixtureCooperative, InputError as FixtureInputError, LineSpan as FixtureLineSpan,
        Provenance as FixtureProvenance, Rank as FixtureRank, ReadWork as FixtureWork,
        SourceMetadata as FixtureMetadata, TaskBytes as FixtureBytes, TaskPoll as FixturePoll,
        Terminator as FixtureTerminator, WorkCredit as FixtureCredit,
    };
    struct FixtureQuery {
        at: u64,
        length: usize,
        credit: FixtureCredit,
        bytes: Option<FixtureBytes>,
        returned: Option<usize>,
    }
    impl FixtureQuery {
        fn new(at: u64) -> Self {
            Self { at, length: 0, credit: FixtureCredit::default(), bytes: None, returned: None }
        }
    }
    struct FixtureTaskSource(Arc<Bytes>);
    impl kwil_view_model::visual_rows::SessionSource for FixtureTaskSource {
        fn into_transport(self) -> Arc<dyn WrapSource> {
            self.0
        }
    }
    macro_rules! fixture_ready {
        ($value:expr) => {
            match $value {
                FixturePoll::Ready(value) => value,
                FixturePoll::Pending => return FixturePoll::Pending,
                FixturePoll::Cancelled => return FixturePoll::Cancelled,
                FixturePoll::Failed(error) => return FixturePoll::Failed(error),
            }
        };
    }
    fn fixture_io<T>(value: io::Result<T>) -> FixturePoll<T> {
        match value {
            Ok(value) => FixturePoll::Ready(value),
            Err(error) => FixturePoll::Failed(FixtureInputError::Io(error).into()),
        }
    }
    impl FixtureCooperative for FixtureTaskSource {
        type LineQuery = FixtureQuery;
        type RankQuery = FixtureQuery;
        type ReadQuery = FixtureQuery;
        type ProvenanceQuery = FixtureQuery;
        fn metadata(&self) -> FixtureMetadata {
            self.0.source_metadata()
        }
        fn new_line_query(&self, at: u64) -> FixtureQuery {
            FixtureQuery::new(at)
        }
        fn new_rank_query(&self, at: u64) -> FixtureQuery {
            FixtureQuery::new(at)
        }
        fn new_provenance_query(&self, at: u64) -> FixtureQuery {
            FixtureQuery::new(at)
        }
        fn new_read_query(&self, at: u64, length: usize, budget: &AllocationBudget) -> Result<FixtureQuery, WrapError> {
            let mut query = FixtureQuery::new(at);
            query.length = length;
            query.bytes = Some(FixtureBytes::try_new(budget, length)?);
            Ok(query)
        }
        fn poll_line(
            &self,
            q: &mut FixtureQuery,
            work: &mut FixtureWork,
            cancel: &AtomicBool,
        ) -> FixturePoll<FixtureLineSpan> {
            fixture_ready!(q.credit.poll(
                FixtureWork::new(std::mem::size_of::<FixtureLineSpan>() as u64 + 2, 4),
                work,
                cancel
            ));
            self.0.worker_only();
            let start = match fixture_ready!(fixture_io(self.0.line_start(q.at))) {
                Some(start) => start,
                None => return FixturePoll::Failed(FixtureInputError::Range.into()),
            };
            let next = fixture_ready!(fixture_io(self.0.line_start(q.at + 1)));
            let end = next.unwrap_or(self.0.len());
            let mut content_end = end;
            let mut terminator = FixtureTerminator::None;
            if next.is_some() {
                content_end -= 1;
                terminator = FixtureTerminator::Lf;
                if content_end > start {
                    // Metadata inspection reads the tiny immutable fixture;
                    // the controlled read gate remains in poll_read below.
                    if self.0.bytes[content_end as usize - 1] == b'\r' {
                        content_end -= 1;
                        terminator = FixtureTerminator::CrLf;
                    }
                }
            }
            FixturePoll::Ready(FixtureLineSpan { start, end, content_end, terminator })
        }
        fn poll_rank(
            &self,
            q: &mut FixtureQuery,
            work: &mut FixtureWork,
            cancel: &AtomicBool,
        ) -> FixturePoll<FixtureRank> {
            fixture_ready!(q.credit.poll(FixtureWork::new(self.0.lines.len() as u64 * 8 + 8, 2), work, cancel));
            let line = fixture_ready!(fixture_io(self.0.line_of(q.at)));
            let start = fixture_ready!(fixture_io(self.0.line_start(line))).unwrap();
            FixturePoll::Ready(FixtureRank { line, column: q.at - start })
        }
        fn poll_read(
            &self,
            q: &mut FixtureQuery,
            output: &mut [u8],
            work: &mut FixtureWork,
            cancel: &AtomicBool,
        ) -> FixturePoll<usize> {
            if q.length != output.len() {
                return FixturePoll::Failed(FixtureInputError::QueryChanged.into());
            }
            if q.returned.is_none() {
                fixture_ready!(q.credit.poll(FixtureWork::new(q.length as u64, 1), work, cancel));
                q.returned = Some(fixture_ready!(fixture_io(self.0.read(q.at, q.bytes.as_mut().unwrap()))));
            }
            let n = q.returned.unwrap();
            fixture_ready!(q.credit.poll(FixtureWork::new(n as u64, 1), work, cancel));
            output[..n].copy_from_slice(&q.bytes.as_ref().unwrap()[..n]);
            FixturePoll::Ready(n)
        }
        fn poll_provenance(
            &self,
            q: &mut FixtureQuery,
            work: &mut FixtureWork,
            cancel: &AtomicBool,
        ) -> FixturePoll<FixtureProvenance> {
            fixture_ready!(q.credit.poll(FixtureWork::new(1, 1), work, cancel));
            self.0.worker_only();
            if q.at >= self.0.len() {
                return FixturePoll::Failed(FixtureInputError::Range.into());
            }
            // This fixture explicitly represents ordinary bytes.
            FixturePoll::Ready(FixtureProvenance { source: 0..self.0.len(), opaque: false })
        }
    }

    struct Fixture {
        host: Option<Host>,
        editor: Editor,
        context: WrapHostContext,
        body: Rect,
        source: Arc<Bytes>,
        wakes: Arc<Wakes>,
        seen: u64,
    }
    impl Fixture {
        fn new(text: &[u8], columns: u16, rows: u16) -> Self {
            let mut editor = Editor::new(Document::new());
            editor.replace_byte_range(0..0, text).unwrap();
            editor.set_cursors(vec![Cursor::caret(0)], 0).unwrap();
            let source = editor.document().hex_source(7);
            let key = LayoutKey {
                source: SourceIdentity {
                    document_id: 7,
                    source_epoch: source.epoch,
                    incarnation: source.epoch,
                    revision: source.revision,
                },
                input_epoch: 1,
                layout_generation: 1,
                body_generation: 1,
                body: BodyOwner::Text,
                geometry: LayoutGeometry { columns, rows, tab_size: 4, ..Default::default() },
            };
            let context = WrapHostContext { enabled: true, key, path_generation: 1, cursor_generation: 1 };
            let source = Arc::new(Bytes::new(key.source, text));
            let wakes = Arc::new(Wakes::default());
            let notify = wakes.clone();
            let host = Host::new(move || notify.wake()).unwrap();
            let mut value = Self {
                host: Some(host),
                editor,
                context,
                source,
                wakes,
                seen: 0,
                body: Rect::new(2, 1, columns + GUTTER, rows),
            };
            value.sync(false);
            value
        }
        fn host(&self) -> &Host {
            self.host.as_ref().unwrap()
        }
        fn host_mut(&mut self) -> &mut Host {
            self.host.as_mut().unwrap()
        }
        fn sync(&mut self, blocked: bool) {
            let deadline = Instant::now() + DEADLINE;
            for _ in 0..256 {
                assert!(Instant::now() < deadline, "bounded sync deadline");
                match self.host.as_mut().unwrap().sync(Some(&self.editor), self.context, self.body, blocked) {
                    Ok(_) => return,
                    Err(WrapError::Busy) => self.wakes.wait(&mut self.seen, deadline),
                    Err(error) => panic!("sync: {error:?}"),
                }
            }
            panic!("bounded sync wake count");
        }
        fn drive(&mut self) {
            if self.host().needs_source() {
                let source: Arc<dyn WrapSource> = self.source.clone();
                match self.host_mut().drive(source) {
                    ControlFlow::Continue(()) => {}
                    // A test refusal is explicitly retired by settling the
                    // worker before the returned source/request is dropped.
                    ControlFlow::Break(owner) => {
                        self.finish();
                        drop(owner);
                        panic!("unexpected source refusal");
                    }
                }
            }
        }
        fn publication(&mut self) -> kwil_view_model::wrap_host::WrapHostPublication {
            let deadline = Instant::now() + DEADLINE;
            for _ in 0..256 {
                assert!(Instant::now() < deadline, "bounded publication deadline");
                self.drive();
                let update = match self.host.as_mut().unwrap().ready(Some(&mut self.editor)) {
                    ControlFlow::Continue(update) => update,
                    ControlFlow::Break(owner) => {
                        self.finish();
                        drop(owner);
                        panic!("unexpected drain refusal");
                    }
                };
                assert!(update.failure.is_none(), "worker failure: {:?}", update.failure);
                if let Some(publication) = update.publication {
                    return publication;
                }
                self.wakes.wait(&mut self.seen, deadline);
            }
            panic!("bounded publication wake count");
        }
        fn fill(&mut self) {
            let _ = self.publication();
            assert!(self.host().page().is_some());
        }
        fn finish(&mut self) {
            self.source.gate.release();
            if let Some(host) = self.host.take() {
                settle_host(host.stop()).unwrap();
            }
        }
        fn paint(&self, terminal: &mut Terminal<TestBackend>, paint: &Paint<'_>) {
            terminal
                .draw(|frame| {
                    assert!(self.host().draw(frame, paint));
                })
                .unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.finish();
        }
    }

    fn cell(terminal: &Terminal<TestBackend>, x: u16, y: u16) -> &str {
        terminal.backend().buffer()[(x, y)].symbol()
    }

    #[test]
    fn wrap_tui_prepared_graphemes_tabs_selection_search_syntax_and_logical_gutter() {
        let text = "A界e\u{301}👩‍💻\tZ\nQ";
        let mut fixture = Fixture::new(text.as_bytes(), 6, 5);
        fixture.editor.set_cursors(vec![Cursor { anchor: 1, head: 4, desired_column: None }], 0).unwrap();
        fixture.context.cursor_generation += 1;
        fixture.sync(false);
        fixture.fill();
        let key = fixture.host().page().unwrap().key;
        let span = Span { range: 0..1, class: kwil_view_model::highlighting::Class::Keyword };
        let spans = [&span];
        let search = 1..4;
        let paint = Paint { key, syntax: &spans, search: std::slice::from_ref(&search), current_search: None };
        let mut terminal = Terminal::new(TestBackend::new(24, 8)).unwrap();
        let before = fixture.source.reads.load(Ordering::Acquire);
        fixture.paint(&mut terminal, &paint);
        assert_eq!(fixture.source.reads.load(Ordering::Acquire), before, "draw never reads source");
        let x = fixture.host().geometry().text.x;
        assert_eq!(cell(&terminal, x, 1), "A");
        assert_eq!(cell(&terminal, x + 1, 1), "界");
        assert_eq!(cell(&terminal, x + 3, 1), "e\u{301}");
        assert_eq!(cell(&terminal, 8, 1), "1");
        assert_eq!(cell(&terminal, 8, 2), ">", "continuation never invents another logical line number");
        assert_eq!(terminal.backend().buffer()[(x + 1, 1)].bg, SELECTION, "selection precedes search background");
        assert_eq!(fixture.host().selection_cells(0, 1..4).collect::<Vec<_>>(), vec![1..3]);
        assert_ne!(terminal.backend().buffer()[(x, 1)].fg, Color::Reset, "normal source atom receives syntax");
        for (row, prepared) in fixture.host().page().unwrap().rows().iter().enumerate() {
            for atom in fixture.host().page().unwrap().atoms(row).iter().filter(|atom| atom.is_tab()) {
                assert_eq!(
                    fixture.host().shared.hit_test(row, atom.cells.start as usize).unwrap().byte,
                    atom.source.start
                );
                assert!(prepared.source_coverage.start <= atom.source.start);
            }
        }
        let stale = Paint { key: LayoutKey { layout_generation: key.layout_generation + 1, ..key }, ..paint };
        fixture.paint(&mut terminal, &stale);
        assert_eq!(
            terminal.backend().buffer()[(x, 1)].fg,
            Color::Reset,
            "old syntax identity cannot paint after reflow"
        );
    }

    #[test]
    fn wrap_tui_resize_keeps_source_anchor_exact_hits_and_clears_short_eof_rows() {
        let mut fixture = Fixture::new(b"abcdefghijklmno\nZ", 3, 5);
        fixture.fill();
        fixture.host_mut().request_fill(TopAnchor { source_byte: 3, ..Default::default() }).unwrap();
        let _ = fixture.publication();
        let anchor = fixture.host().page().unwrap().anchor;
        assert_eq!(anchor.source_byte, 3);
        let mut terminal = Terminal::new(TestBackend::new(25, 9)).unwrap();
        fixture.paint(&mut terminal, &Paint::empty(fixture.host().page().unwrap().key));
        assert_eq!(cell(&terminal, fixture.body.x + 8, fixture.body.y + 4), "Z");
        fixture.body.width = 14;
        fixture.context.key.layout_generation += 1;
        fixture.sync(false);
        assert!(fixture.host().page().is_none(), "old geometry disappears before a worker result");
        terminal.backend_mut().resize(22, 8);
        let _ = fixture.publication();
        let page = fixture.host().page().unwrap();
        assert_eq!(page.key.geometry.columns, 6);
        assert!(page.rows()[0].source_coverage.contains(&anchor.source_byte));
        let exact = fixture.host().shared.hit_test(0, 4).unwrap();
        assert!(exact.byte <= 15, "hit uses source bytes, never display-string bytes");
        let key = page.key;
        fixture.paint(&mut terminal, &Paint::empty(key));
        assert_eq!(cell(&terminal, fixture.body.x + 8, fixture.body.y + 4), " ", "short EOF clears old rows");
        assert_eq!(cell(&terminal, fixture.body.x - 1, fixture.body.y), " ", "body does not overwrite adjacent chrome");
        fixture.body.width = 8;
        fixture.sync(false);
        assert!(!fixture.host().needs_source(), "zero text columns do no work");
        assert!(fixture.host().page().is_none());
    }

    #[test]
    fn wrap_tui_relative_fifo_keeps_counts_selection_and_mutation_busy_until_publication() {
        let mut fixture = Fixture::new(b"abcdefghijklmnopqr\nEND", 3, 3);
        fixture.fill();
        fixture.source.gate.arm();
        assert!(matches!(fixture.host_mut().command("motion.down", 2, true), Input::Accepted));
        assert!(matches!(fixture.host_mut().command("motion.up", 1, true), Input::Accepted));
        assert!(matches!(fixture.host_mut().command("motion.down", 1, true), Input::Accepted));
        fixture.drive();
        fixture.source.gate.entered();
        let revision = fixture.editor.document().revision();
        assert_eq!(fixture.editor.primary_cursor().head, 0);
        assert_eq!(fixture.host().mutation_gate(), MutationGate::RefusedPending);
        assert!(mutation_notice(fixture.host().mutation_gate()).is_some());
        fixture.source.gate.release();
        let mut ids = Vec::new();
        for expected in [6, 3, 6] {
            let published = fixture.publication();
            ids.push(published.relative_id.unwrap());
            assert_eq!(fixture.editor.primary_cursor().head, expected);
            assert_eq!(fixture.editor.primary_cursor().anchor, 0);
            assert_eq!(fixture.editor.document().revision(), revision, "busy input did not edit the old caret");
        }
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]), "accepted relative inputs publish in order");
        assert_eq!(fixture.host().mutation_gate(), MutationGate::Allowed);
        assert_eq!(fixture.host().shared.visual_cursors().unwrap()[0].head, 6);
    }

    #[test]
    fn wrap_tui_full_fifo_refuses_newest_escape_cancels_and_modal_owners_keep_input() {
        let mut fixture = Fixture::new(b"abcdefghijklmnop", 3, 3);
        fixture.fill();
        fixture.sync(true);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        assert!(matches!(fixture.host_mut().fallback_key(down), Input::Pass));
        let mut terminal = Terminal::new(TestBackend::new(24, 8)).unwrap();
        terminal
            .draw(|frame| {
                frame.set_cursor_position((1, 1));
                fixture.host().draw(frame, &Paint::empty(fixture.host().page().unwrap().key));
            })
            .unwrap();
        assert_eq!(
            terminal.backend().cursor_position(),
            ratatui::layout::Position::new(1, 1),
            "a modal's cursor remains owned by that modal"
        );
        fixture.sync(false);
        for _ in 0..kwil_view_model::wrap_session::RELATIVE_CAPACITY {
            assert!(matches!(fixture.host_mut().command("motion.down", 1, false), Input::Accepted));
        }
        let refused = fixture.host_mut().command("motion.up", 1, false);
        assert!(matches!(
            refused,
            Input::Refused(WrapHostInputError::Queue(kwil_view_model::wrap_session::QueueError::Full))
        ));
        assert!(refused.notice().unwrap().contains("not accepted"));
        assert!(matches!(
            fixture.host_mut().fallback_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Input::Accepted
        ));
        assert_eq!(fixture.host().mutation_gate(), MutationGate::Allowed);
        assert_eq!(fixture.editor.primary_cursor().head, 0);
    }

    #[test]
    fn wrap_tui_failed_relative_head_requires_explicit_retry_and_never_skips_accepted_input() {
        let mut fixture = Fixture::new(b"abcdefghijklmnop", 3, 3);
        fixture.fill();
        fixture.source.fail_next.store(true, Ordering::Release);
        assert!(matches!(fixture.host_mut().command("motion.down", 1, false), Input::Accepted));
        assert!(matches!(fixture.host_mut().command("motion.up", 1, false), Input::Accepted));
        fixture.drive();
        let deadline = Instant::now() + DEADLINE;
        let mut failed = false;
        for _ in 0..256 {
            assert!(Instant::now() < deadline, "bounded failed-head deadline");
            let update = match fixture.host.as_mut().unwrap().ready(Some(&mut fixture.editor)) {
                ControlFlow::Continue(update) => update,
                ControlFlow::Break(owner) => {
                    fixture.finish();
                    drop(owner);
                    panic!("unexpected owner refusal");
                }
            };
            assert!(update.publication.is_none());
            if update.failure.is_some() {
                failed = true;
                break;
            }
            fixture.wakes.wait(&mut fixture.seen, deadline);
        }
        assert!(failed);
        assert_eq!(fixture.host().mutation_gate(), MutationGate::RefusedFailed);
        assert_eq!(fixture.editor.primary_cursor().head, 0);
        fixture.host_mut().retry_relative().unwrap();
        let first = fixture.publication();
        assert_eq!(fixture.editor.primary_cursor().head, 3);
        let second = fixture.publication();
        assert_eq!(fixture.editor.primary_cursor().head, 0);
        assert!(first.relative_id.unwrap() < second.relative_id.unwrap());
        assert_eq!(fixture.host().mutation_gate(), MutationGate::Allowed);
    }

    #[test]
    fn wrap_tui_editor_loan_and_raw_body_reject_late_pages_and_restore_text_explicitly() {
        let mut fixture = Fixture::new(b"abcdefghi", 3, 3);
        fixture.source.gate.arm();
        fixture.drive();
        fixture.source.gate.entered();
        let mut loan = fixture.context;
        loan.key.body = BodyOwner::EditorLoan;
        loan.key.body_generation += 1;
        fixture.host.as_mut().unwrap().sync(None, loan, fixture.body, false).unwrap();
        assert!(!fixture.host().owns_body());
        assert!(fixture.host().page().is_none());
        fixture.source.gate.release();
        let deadline = Instant::now() + DEADLINE;
        // Wake, not polling: consume the old result only through shared drain.
        fixture.wakes.wait(&mut fixture.seen, deadline);
        assert!(matches!(fixture.host_mut().ready(None), ControlFlow::Continue(_)));
        assert!(fixture.host().page().is_none());
        let mut raw = loan;
        raw.key.body = BodyOwner::Raw;
        raw.key.body_generation += 1;
        fixture.context = raw;
        fixture.sync(false);
        let mut terminal = Terminal::new(TestBackend::new(24, 8)).unwrap();
        terminal
            .draw(|frame| {
                frame.buffer_mut()[(10, 1)].set_symbol("R");
                assert!(!fixture.host().draw(frame, &Paint::empty(raw.key)));
            })
            .unwrap();
        assert_eq!(cell(&terminal, 10, 1), "R", "raw body guard precedes wrapped paint");
        fixture.context.key.body = BodyOwner::Text;
        fixture.context.key.body_generation += 1;
        fixture.sync(false);
        fixture.fill();
        assert!(fixture.host().owns_body());
    }

    #[test]
    fn wrap_tui_tab_change_cancels_relative_intent_and_fences_the_old_worker_owner() {
        let mut fixture = Fixture::new(b"abcdefghijklmnop", 3, 3);
        fixture.source.gate.arm();
        assert!(matches!(fixture.host_mut().command("motion.down", 1, false), Input::Accepted));
        fixture.drive();
        fixture.source.gate.entered();
        let old_source = fixture.source.clone();
        fixture.context.key.source.document_id += 1;
        fixture.context.key.input_epoch += 1;
        fixture.context.key.body_generation += 1;
        let sync =
            fixture.host.as_mut().unwrap().sync(Some(&fixture.editor), fixture.context, fixture.body, false).unwrap();
        assert_eq!(sync.cancelled.unwrap().reason, CancelReason::TabChanged);
        assert_eq!(fixture.host().mutation_gate(), MutationGate::Allowed);
        assert!(fixture.host().page().is_none());
        fixture.source = Arc::new(Bytes::new(fixture.context.key.source, b"abcdefghijklmnop"));
        old_source.gate.release();
        fixture.fill();
        assert_eq!(fixture.host().page().unwrap().key.source.document_id, 8);
        assert_eq!(fixture.editor.primary_cursor().head, 0, "old tab motion never publishes into current tab");
        assert_eq!(fixture.host().shared.pending_inputs(), 0);
    }

    #[test]
    fn wrap_tui_mouse_uses_prepared_cell_mapping_and_chrome_never_becomes_text() {
        let mut fixture = Fixture::new("界\tA\nZ".as_bytes(), 5, 3);
        fixture.fill();
        let text = fixture.host().geometry().text;
        let mouse = |x, y, kind| MouseEvent { kind, column: x, row: y, modifiers: KeyModifiers::NONE };
        assert!(matches!(
            fixture.host_mut().mouse(mouse(text.x - 1, text.y, MouseEventKind::Down(MouseButton::Left))),
            Input::Pass
        ));
        assert!(matches!(
            fixture.host_mut().mouse(mouse(text.right(), text.y, MouseEventKind::Down(MouseButton::Left))),
            Input::Pass
        ));
        assert!(matches!(
            fixture.host_mut().mouse(mouse(text.x + 1, text.y, MouseEventKind::Down(MouseButton::Left))),
            Input::Accepted
        ));
        assert_eq!(fixture.host().mutation_gate(), MutationGate::RefusedPending);
        let _ = fixture.publication();
        assert_eq!(
            fixture.editor.primary_cursor().head,
            0,
            "wide-cell interior hits the complete source cluster start"
        );
        assert!(matches!(
            fixture.host_mut().mouse(mouse(text.x + 2, text.y, MouseEventKind::Down(MouseButton::Left))),
            Input::Accepted
        ));
        let _ = fixture.publication();
        assert_eq!(fixture.editor.primary_cursor().head, 3, "tab uses its one-byte source endpoint");
        assert!(matches!(fixture.host_mut().mouse(mouse(text.x, text.y, MouseEventKind::ScrollDown)), Input::Accepted));
        assert_eq!(fixture.host().mutation_gate(), MutationGate::RefusedPending);
        let _ = fixture.publication();
    }

    #[test]
    fn wrap_tui_worker_upstream_boundary_keeps_caret_on_published_row() {
        let mut fixture = Fixture::new(b"abcd\n12345678", 4, 3);
        fixture.editor.set_cursors(vec![Cursor::caret(4)], 0).unwrap();
        fixture.context.cursor_generation += 1;
        fixture.sync(false);
        fixture.fill();
        assert!(matches!(fixture.host_mut().command("motion.down", 1, false), Input::Accepted));
        let _ = fixture.publication();
        let primary = fixture.host().shared.visual_cursors().unwrap()[fixture.host().shared.primary_index().unwrap()];
        assert_eq!(primary.head, 9);
        assert_eq!(primary.head_affinity, CaretAffinity::Upstream);
        let page = fixture.host().page().unwrap();
        assert_eq!(page.rows()[0].source_coverage, 5..9);
        assert_eq!(page.rows()[1].source_coverage, 9..13);
        let mut terminal = Terminal::new(TestBackend::new(24, 8)).unwrap();
        fixture.paint(&mut terminal, &Paint::empty(page.key));
        assert_eq!(
            terminal.backend().cursor_position(),
            ratatui::layout::Position::new(13, 1),
            "terminal margin clamps the caret on its upstream row, never the next continuation"
        );
    }

    #[test]
    fn wrap_tui_scroll_within_one_logical_line_forwards_copy_and_exposed_row_damage() {
        let mut fixture = Fixture::new(b"abcdefghijklmnopqrstuvwxyz0123456789", 3, 6);
        fixture.fill();
        let text = fixture.host().geometry().text;
        assert!(matches!(
            fixture.host_mut().mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: text.x,
                row: text.y,
                modifiers: KeyModifiers::NONE
            }),
            Input::Accepted
        ));
        let publication = fixture.publication();
        assert!(!publication.damage.full, "same-key overlapping visual rows remain a copy operation");
        assert_eq!(publication.damage.scroll, 3);
        assert_eq!(publication.damage.changed_rows().collect::<Vec<_>>(), vec![3, 4, 5]);
        assert_eq!(fixture.host().page().unwrap().anchor.source_byte, 9);
        assert!(fixture.host().page().unwrap().rows().iter().all(|row| row.logical_line == 0));
    }

    #[test]
    fn wrap_tui_source_refusal_returns_the_exact_owner_without_a_ui_retry_slot() {
        let mut fixture = Fixture::new(b"abc", 3, 2);
        let wrong_key =
            SourceIdentity { incarnation: fixture.context.key.source.incarnation + 1, ..fixture.context.key.source };
        let source: Arc<dyn WrapSource> = Arc::new(Bytes::new(wrong_key, b"foreign"));
        let identity = Arc::as_ptr(&source);
        let refusal = match fixture.host_mut().drive(source) {
            ControlFlow::Break(refusal) => refusal,
            ControlFlow::Continue(()) => panic!("foreign source cannot be admitted"),
        };
        assert!(std::ptr::addr_eq(identity, Arc::as_ptr(refusal.owner.source())));
        assert_eq!(Arc::strong_count(refusal.owner.source()), 1, "adapter did not keep an extra source handle");
        fixture.finish();
        drop(refusal);
    }

    #[test]
    fn wrap_tui_opaque_mapping_never_requests_giant_source_for_syntax() {
        let text = format!("a{}Z", "\u{301}".repeat(160));
        let mut fixture = Fixture::new(text.as_bytes(), 1, 3);
        fixture.fill();
        let page = fixture.host().page().unwrap();
        assert_eq!(page.atoms(0)[0].kind, AtomKind::Opaque);
        let source = page.atoms(0)[0].source.clone();
        assert!(source.end - source.start > 256);
        assert!(fixture.host().check_syntax_request(source.clone(), 0).is_err());
        assert!(fixture.host().check_syntax_request(0..1, 1).is_err());
        assert!(fixture.host().syntax_windows().iter().all(|range| range.start >= source.end));
        let mut terminal = Terminal::new(TestBackend::new(16, 7)).unwrap();
        fixture.paint(&mut terminal, &Paint::empty(page.key));
        assert_eq!(cell(&terminal, 10, 1), "?");
        assert_eq!(fixture.host().shared.hit_test(0, 0).unwrap().byte, 0);
        assert_eq!(fixture.host().shared.hit_test(0, 1).unwrap().byte, source.end);
    }

    #[test]
    fn wrap_tui_shutdown_timeout_preserves_exact_source_owner_until_retry_settles() {
        let mut fixture = Fixture::new(b"abcdef", 3, 2);
        fixture.source.gate.arm();
        let weak = Arc::downgrade(&fixture.source);
        let gate = fixture.source.gate.clone();
        fixture.drive();
        gate.entered();
        let host = fixture.host.take().unwrap();
        let ticket = host.stop();
        let failure = ticket.settle(Duration::ZERO).unwrap_err();
        assert!(matches!(failure.error, WrapError::TimedOut));
        assert!(weak.upgrade().is_some());
        let bytes = fixture.source.clone();
        assert!(Arc::ptr_eq(&weak.upgrade().unwrap(), &bytes), "timeout retains this exact immutable source owner");
        gate.release();
        settle_host(failure.ticket).unwrap();
        assert_eq!(Arc::strong_count(&bytes), 2, "worker released its source after successful acknowledgement");
        drop(bytes);
        // Fixture still owns the test source; successful settlement has no
        // hidden UI source retry slot and never starts document cleanup early.
        assert_eq!(Arc::strong_count(&fixture.source), 1);
    }
}
