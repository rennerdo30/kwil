//! Compact in-window log filter and read-only main-body projection.
//!
//! ROOT hook contract (no lib.rs/export/manifest writes in this package):
//! 1. Configure Host::admit_projection_disk(existing_search_spool_cap) before
//!    first open. No implicit disk policy or profile-directory fallback.
//! 2. body_area(): compute the unreserved source TabArea after any width split,
//!    then return log_ui::layout(source, &ui, logs.is_open()).body. Refresh
//!    Workspace row geometry/retained body cache on open/close/resize/zoom/DPI.
//!    sync_logs derives the UNRESERVED top from chrome; never reserve twice.
//! 3. If logs.owns_body(), replace draw_row/editor selection/caret/search marks
//!    with draw_body(&logs, body_pixels, body_stride, editor_font, current_key).
//!    The input slice is ROOT's local retained source-body buffer, excludes
//!    scrollbars, and has exactly the reserved body's size. Capture current_key
//!    with Source::capture(&view) AFTER sync_logs, not from Host::source(). A
//!    stale/empty/draft/error projection paints blank, never underlying text.
//!    draw() paints only layout.bar with UiFont, before menus/higher modals.
//! 4. Include logs.paint and logs.body_paint in pending/take paint hooks.
//!    body_paint requests damage the source body when page, source, geometry,
//!    selection or mode changes. paint covers the bar; hover dirties only it.
//!    blocks_copy() excludes normal-editor scroll copies until ROOT supplies
//!    projection copy/exposed-row handling; never reuse ordinary-editor pixels.
//!    Closing restores body geometry and repaints the real editor once. No
//!    frame loop, retry timer, source reads or worker submissions from draw().
//! 5. Vertical scrollbar: viewport_position().0 is matching ordinal;
//!    viewport_extent().0/.1 is count/exact. Visible rows are body's h/font.ch
//!    (up to VIEWPORT_ROWS=4096); legacy hit pages still cap at 256.
//!    Horizontal scrollbar: position().1 and extent().2 are original line bytes.
//!    Route both track/drag results through App::logs_scroll_to(top, byte).
//!    Wheel/keys use the same index; no source rescan after the initial pass.
//!    Handle scrollbar presses first; they must never reach row activation.
//! 6. After higher-priority modals, route logs_key BEFORE editor commands/text,
//!    logs_mouse BEFORE text hit/selection/context menus, logs_track and wheel.
//!    While owns_body(), consume ALL remaining buttons, mouse release/drag,
//!    paste/drop/IME and edit commands targeting that body. Never send Cut,
//!    Paste, Undo, Replace, Delete, SelectAll or context edits to hidden text.
//!    Existing toolbar/tab/menu chrome remains ROOT-owned. Right-click may
//!    offer read-only Open original, never stale editor selection actions.
//!    Ctrl+C copies only a whole loaded UTF-8 row, directly from worker bytes;
//!    invalid bytes, NUL and partial windows refuse copying without mutation.
//!    Click/Enter verifies source/generation, uses strict original GoTo(line,1),
//!    then closes the filter and reveals the original editable line.
//! 7. Keep existing LogReady, sync_logs after source/result/tab/edit/epoch
//!    changes, bounded retry-on-worker-wake, IME field tokens and off-UI shutdown
//!    hooks. UserEvent::LogReady calls logs_ready even after closing. Cancel
//!    compositions on focus/modal/source changes; never fall back to editing.
//!    shutdown() joins off UI and reports exact spool-retirement failures.
//!
//! Source coordinates are logical Snapshot bytes (decoded source coordinates,
//! not offsets in a compressed file/legacy encoding). No source is modified.
//! Row windows preserve valid UTF-8 scalars and invalid raw bytes; existing grid
//! cells retain original byte ranges for escaping, tabs and Unicode shaping.
//! Horizontal positioning is a byte window, as in Workspace, not a display
//! column. A clipped row must be opened in the original for whole-row copying.
//! Original GoTo's existing bounded source reads remain ROOT's admission gate.
use super::*;
use chrome::Metrics;
use desktop::{Colors, Field, Hits, Icon, Paint, Sizes, Style, Surface, inset};
use kwil_core_commands::theme::Theme;
use kwil_core_syntax::logs::parse_timestamp;
use kwil_view_model::Clipboard;
#[cfg(test)]
use kwil_view_model::log_session::Timestamp;
use kwil_view_model::log_session::{
    Filter, Hit, Level, LogSession, Page, ROW_BYTES, Report, VIEWPORT_BYTES, VIEWPORT_ROWS, Viewport, ViewportPage,
    ViewportReport, ViewportRow,
};
use unicode_segmentation::UnicodeSegmentation;

#[cfg(test)]
const PAGE: usize = 256;
const INPUT_BYTES: usize = 256;
const PREVIEW_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettlementReason {
    Worker(kwil_view_model::log_session::LogSettlementReason),
    Retirement,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Source {
    pub document: u64,
    pub revision: u64,
    pub epoch: u64,
}
impl Source {
    pub fn capture(view: &Workspace) -> Option<Self> {
        if view.document_busy(view.active_id()) {
            return None;
        }
        Some(Self {
            document: view.active_id().0,
            revision: view.editor()?.document().revision(),
            epoch: view.input_epoch(),
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Version {
    source: Source,
    generation: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FieldId {
    Query,
    From,
    To,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Level(usize), // 0 = All, 1..=6 = Level::ALL
    Field(FieldId),
    Unknown,
    Apply,
    Clear,
    Results,
    Hit(usize),
    Close,
}
const ORDER: [Target; 15] = [
    Target::Field(FieldId::Query),
    Target::Level(0),
    Target::Level(1),
    Target::Level(2),
    Target::Level(3),
    Target::Level(4),
    Target::Level(5),
    Target::Level(6),
    Target::Field(FieldId::From),
    Target::Field(FieldId::To),
    Target::Unknown,
    Target::Apply,
    Target::Clear,
    Target::Results,
    Target::Close,
];

#[derive(Default)]
struct Edit {
    text: String,
    cursor: usize,
    all: bool,
}
impl Edit {
    fn insert(&mut self, text: &str) -> Result<bool, &'static str> {
        if text.is_empty() {
            return Ok(false);
        }
        let old = if self.all { 0 } else { self.text.len() };
        if old.saturating_add(text.len()) > INPUT_BYTES {
            return Err("Log field is too long (256 UTF-8 bytes maximum)");
        }
        if text.chars().any(char::is_control) {
            return Err("Log fields accept one line of text");
        }
        if self.all {
            self.text.clear();
            self.cursor = 0;
        }
        self.all = false;
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
        // An inserted combining character may join its following neighbour.
        self.snap_cursor();
        Ok(true)
    }
    fn snap_cursor(&mut self) {
        self.cursor = self
            .text
            .grapheme_indices(true)
            .map(|(at, _)| at)
            .chain(std::iter::once(self.text.len()))
            .find(|&at| at >= self.cursor)
            .unwrap_or(self.text.len());
    }
    fn left(&self) -> usize {
        self.text.grapheme_indices(true).map(|(at, _)| at).take_while(|&at| at < self.cursor).last().unwrap_or(0)
    }
    fn right(&self) -> usize {
        self.text.grapheme_indices(true).map(|(at, _)| at).find(|&at| at > self.cursor).unwrap_or(self.text.len())
    }
    fn key(&mut self, key: NamedKey) -> bool {
        if self.all && matches!(key, NamedKey::Backspace | NamedKey::Delete) {
            let changed = !self.text.is_empty();
            self.text.clear();
            self.cursor = 0;
            self.all = false;
            return changed;
        }
        let mut changed = false;
        match key {
            NamedKey::Backspace => {
                let left = self.left();
                changed = left != self.cursor;
                self.text.replace_range(left..self.cursor, "");
                self.cursor = left;
            }
            NamedKey::Delete => {
                let right = self.right();
                changed = right != self.cursor;
                self.text.replace_range(self.cursor..right, "");
            }
            NamedKey::ArrowLeft => self.cursor = if self.all { 0 } else { self.left() },
            NamedKey::ArrowRight => self.cursor = if self.all { self.text.len() } else { self.right() },
            NamedKey::Home => self.cursor = 0,
            NamedKey::End => self.cursor = self.text.len(),
            _ => {}
        }
        self.all = false;
        // Removing a separator can join two formerly separate graphemes
        // (for example regional indicators); retain a true field boundary.
        self.snap_cursor();
        changed
    }
}

/// A composition is pinned to a source, a particular field and its input epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Composition {
    source: Source,
    input_epoch: u64,
    field: FieldId,
}
struct Retry {
    source: Source,
    serial: u64,
    snapshot: kwil_core_text::Snapshot,
    filter: Filter,
    viewport: Viewport,
}

pub(super) struct Host {
    follow_watch: Option<kwil_view_model::log_follow::FollowWatch>,
    follow_document: Option<kwil_view_model::SessionDocumentId>,
    following: bool,
    follow_reload: bool,
    session: Option<LogSession>,
    // Never replace/drop a rejected snapshot on UI. Retry this exact owner
    // before acquiring another, including after close or source changes.
    retry: Option<Retry>,
    retirement_failure: Option<String>,
    open: bool,
    source: Option<Source>,
    expected: Option<Version>,
    loaded: Option<Version>,
    serial: u64,
    input_epoch: u64,
    from: Edit,
    query: Edit,
    regex: bool,
    to: Edit,
    levels: u8,
    keep_unknown: bool,
    filter: Filter,
    draft: bool,
    want: bool,
    busy: bool,
    error: Option<String>,
    notice: Option<String>,
    hits: Vec<Hit>,
    rows: Vec<ViewportRow>,
    horizontal: u64,
    max_line_bytes: u64,
    row_bytes: usize,
    // ROOT supplies the same explicit resource policy as SearchWorker. None
    // refuses viewport admission, never silently chooses a new disk budget.
    index_limit: Option<u64>,
    body: Rect,
    source_width: usize,
    total: u64,
    exact: bool,
    page_start: u64,
    selected: usize,
    scroll: usize,
    visible: usize,
    focus: Target,
    hover: Option<Target>,
    geometry: Hits<Target>,
    geometry_epoch: u64,
    surface: Surface,
    area: Option<Rect>,
    restore: Option<Rect>,
    caret: Option<Rect>,
    pub body_top: usize,
    pub paint: Paint,
    pub body_paint: Paint,
}
impl Default for Host {
    fn default() -> Self {
        let filter = Filter::default();
        Self {
            follow_watch: None,
            follow_document: None,
            following: false,
            follow_reload: false,
            session: None,
            retry: None,
            retirement_failure: None,
            open: false,
            source: None,
            expected: None,
            loaded: None,
            serial: 0,
            input_epoch: 0,
            from: Edit::default(),
            query: Edit::default(),
            regex: false,
            to: Edit::default(),
            levels: filter.levels,
            keep_unknown: true,
            filter,
            draft: false,
            want: false,
            busy: false,
            error: None,
            notice: None,
            hits: Vec::new(),
            rows: Vec::new(),
            horizontal: 0,
            max_line_bytes: 0,
            row_bytes: 4096,
            index_limit: None,
            body: Rect::default(),
            source_width: 0,
            total: 0,
            exact: false,
            page_start: 0,
            selected: 0,
            scroll: 0,
            visible: 1,
            focus: Target::Field(FieldId::From),
            hover: None,
            geometry: Hits::default(),
            geometry_epoch: 0,
            surface: Surface::default(),
            area: None,
            restore: None,
            caret: None,
            body_top: 0,
            paint: Paint::default(),
            body_paint: Paint::default(),
        }
    }
}
fn all_levels() -> u8 {
    Level::ALL.iter().fold(0, |mask, level| mask | level.bit())
}
impl Host {
    pub fn panic_payload(&self) -> Option<&(dyn std::any::Any + Send)> {
        self.session.as_ref().and_then(|session| session.panic_payload())
    }
    /// OFF UI. Keep the original session and failed retirement in this host;
    /// the shared producer owns its bounded wait and actual thread join.
    pub fn retirement_error(&self) -> Option<&str> {
        self.retirement_failure.as_deref()
    }
    /// Explicit OFF-UI producer retry. Only that producer can reap its inactive
    /// registry entries and clear the original sticky retirement diagnostic.
    pub fn retry_retirement_until(
        &mut self,
        deadline: std::time::Instant,
    ) -> Result<(), kwil_view_model::log_session::LogRetirementRetryReason> {
        let session =
            self.session.as_mut().ok_or(kwil_view_model::log_session::LogRetirementRetryReason::WorkerNotJoined)?;
        session.retry_retirement_until(deadline)?;
        self.retirement_failure = None;
        Ok(())
    }
    pub fn settle_until(&mut self, deadline: std::time::Instant) -> Result<(), SettlementReason> {
        if self.retirement_failure.is_some() {
            return Err(SettlementReason::Retirement);
        }
        self.close();
        if let Some(watch) = &mut self.follow_watch {
            watch.stop();
        }
        drop(self.retry.take());
        if let Some(session) = &mut self.session {
            session.settle_until(deadline).map_err(SettlementReason::Worker)?;
            if let Err(error) = session.retirement_status().result() {
                self.retirement_failure = Some(error);
                return Err(SettlementReason::Retirement);
            }
        }
        Ok(())
    }
    /// Configure once, before opening. ROOT uses the existing search spool
    /// cap (currently SearchWorker::new's 1 GiB), not a new product setting.
    pub fn admit_projection_disk(&mut self, bytes: u64) {
        assert!(self.session.is_none(), "configure log disk admission before starting its worker");
        self.index_limit = Some(bytes);
    }
    pub fn owns_body(&self) -> bool {
        self.open
    }
    pub fn owns_point(&self, point: (usize, usize)) -> bool {
        let source = Rect {
            x: self.body.x,
            y: self.body_top,
            w: self.source_width,
            h: self.body.y.saturating_add(self.body.h).saturating_sub(self.body_top),
        };
        self.open && (source.contains(point) || self.area.is_some_and(|r| r.contains(point)))
    }
    pub fn viewport_position(&self) -> (u64, u64) {
        (self.page_start, self.horizontal)
    }
    pub fn viewport_extent(&self) -> (u64, bool, u64) {
        (self.total, self.exact, self.max_line_bytes)
    }
    fn viewport(&self) -> Viewport {
        Viewport {
            top: self.page_start,
            rows: self.visible.clamp(1, VIEWPORT_ROWS),
            horizontal: self.horizontal,
            row_bytes: self.row_bytes,
        }
    }
    pub fn is_open(&self) -> bool {
        self.open
    }
    pub fn blocks_copy(&self) -> bool {
        self.open || self.area.is_some() || self.restore.is_some()
    }
    pub fn take_restore(&mut self) -> Option<Rect> {
        self.restore.take()
    }
    pub fn caret(&self) -> Option<Rect> {
        self.caret
    }
    fn bump(&mut self) {
        self.input_epoch = self.input_epoch.checked_add(1).expect("log input epoch exhausted");
        self.caret = None;
        self.paint.request();
        self.body_paint.request();
    }
    fn invalidate(&mut self) {
        self.serial = self.serial.checked_add(1).expect("log query serial exhausted");
        self.expected = None;
        self.loaded = None;
        self.hits.clear();
        self.rows.clear();
        self.notice = None;
        self.total = 0;
        self.exact = false;
        self.max_line_bytes = 0;
        self.busy = false;
        self.selected = 0;
        self.scroll = 0;
        self.bump();
    }
    fn changed(&mut self) {
        if let Some(session) = &self.session {
            session.cancel_viewport();
        }
        self.invalidate();
        self.draft = true;
        self.want = false;
        self.error = None;
    }
    fn sync_source(&mut self, source: Option<Source>) {
        if self.source == source {
            return;
        }
        self.source = source;
        if let Some(session) = &self.session {
            session.cancel_viewport();
        }
        self.invalidate();
        self.page_start = 0;
        self.horizontal = 0;
        self.want = self.open && !self.draft;
        self.error = None;
    }
    fn close(&mut self) {
        if !self.open {
            return;
        }
        self.open = false;
        if let Some(session) = &self.session {
            session.cancel_viewport();
        }
        self.want = false;
        self.invalidate();
        self.restore = self.area.take().or(self.restore);
        self.geometry = Hits::default();
        self.hover = None;
    }
    fn edit(&mut self, field: FieldId) -> &mut Edit {
        match field {
            FieldId::Query => &mut self.query,
            FieldId::From => &mut self.from,
            FieldId::To => &mut self.to,
        }
    }
    fn field(&self) -> Option<FieldId> {
        match self.focus {
            Target::Field(field) => Some(field),
            _ => None,
        }
    }
    fn validated(&self) -> Result<Filter, &'static str> {
        let parse = |text: &str| {
            if text.trim().is_empty() {
                Ok(None)
            } else {
                parse_timestamp(text.trim())
                    .map(Some)
                    .ok_or("Use an ISO date/time with timezone, log4j UTC time, or Unix timestamp")
            }
        };
        let from = parse(&self.from.text)?;
        let to = parse(&self.to.text)?;
        if from.zip(to).is_some_and(|(a, b)| a > b) {
            return Err("From must be earlier than or equal to To");
        }
        Ok(Filter {
            levels: self.levels & all_levels(),
            from,
            to,
            keep_unknown: self.keep_unknown,
            text: kwil_view_model::log_session::TextQuery::new(&self.query.text, self.regex)?,
        })
    }
    fn apply(&mut self) {
        if let Some(session) = &self.session {
            session.cancel_viewport();
        }
        self.invalidate();
        self.want = false;
        match self.validated() {
            Ok(filter) => {
                self.filter = filter;
                self.draft = false;
                self.page_start = 0;
                self.horizontal = 0;
                self.error = None;
                self.want = self.open;
            }
            Err(error) => {
                self.draft = true;
                self.error = Some(error.into());
            }
        }
    }
    fn clear(&mut self) {
        self.query = Edit::default();
        self.from = Edit::default();
        self.to = Edit::default();
        self.levels = all_levels();
        self.keep_unknown = true;
        self.apply();
    }
    fn toggle_level(&mut self, index: usize) {
        if index == 0 {
            self.levels = all_levels();
        } else if let Some(level) = Level::ALL.get(index - 1) {
            self.levels ^= level.bit();
        }
        self.changed();
    }
    fn accepted(&mut self, source: Source, serial: u64, generation: u64) {
        if self.open && self.source == Some(source) && self.serial == serial && !self.draft {
            self.expected = Some(Version { source, generation });
            self.want = false;
            self.busy = true;
            self.error = None;
            self.paint.request();
        }
    }
    #[cfg(test)]
    fn receive(&mut self, report: Report) -> bool {
        self.receive_mode(report, PAGE)
    }
    fn receive_mode(&mut self, report: Report, row_limit: usize) -> bool {
        let version = Version {
            source: Source { document: report.document, revision: report.revision, epoch: report.epoch },
            generation: report.generation,
        };
        if !self.open || self.source != Some(version.source) || self.expected != Some(version) {
            return false;
        }
        // A delayed partial report can never regress an exact final page.
        if self.loaded == Some(version) && self.exact {
            return false;
        }
        match report.result {
            Ok(Page { hits, total, exact }) => {
                if hits.len() > row_limit
                    || hits.iter().any(|hit| hit.preview.len() > PREVIEW_BYTES)
                    || hits.windows(2).any(|pair| pair[0].line >= pair[1].line || pair[0].offset >= pair[1].offset)
                    || (!hits.is_empty() && self.page_start.saturating_add(hits.len() as u64) > total)
                {
                    self.expected = None;
                    self.loaded = None;
                    self.hits.clear();
                    self.busy = false;
                    self.error = Some("Invalid log result page; apply the filter again".into());
                } else {
                    self.loaded = Some(version);
                    self.hits = hits;
                    self.total = total;
                    self.exact = exact;
                    self.busy = !exact;
                    self.selected = self.selected.min(self.hits.len().saturating_sub(1));
                    self.scroll = self.scroll.min(self.hits.len().saturating_sub(self.visible));
                }
            }
            Err(error) => {
                self.loaded = None;
                self.hits.clear();
                self.busy = false;
                self.error = Some(desktop::visible(&error, 256));
            }
        }
        // Replacing a page invalidates hit geometry even within one generation.
        self.bump();
        true
    }
    fn receive_viewport(&mut self, report: ViewportReport) -> bool {
        let version = Version {
            source: Source { document: report.document, revision: report.revision, epoch: report.epoch },
            generation: report.generation,
        };
        if !self.open
            || self.source != Some(version.source)
            || self.expected != Some(version)
            || (self.loaded == Some(version) && self.exact)
        {
            return false;
        }
        let result = match report.result {
            Ok(page) => {
                if !self.valid_page(&page) {
                    Err("Invalid log viewport page; apply the filter again".into())
                } else {
                    self.rows = page.rows;
                    self.max_line_bytes = page.max_line_bytes;
                    Ok(Page {
                        hits: self
                            .rows
                            .iter()
                            .map(|row| Hit {
                                line: row.line,
                                offset: row.line_start,
                                metadata: Default::default(),
                                preview: String::new(),
                            })
                            .collect(),
                        total: page.total,
                        exact: page.exact,
                    })
                }
            }
            Err(error) => Err(error),
        };
        if result.is_err() {
            self.rows.clear();
        }
        self.receive_mode(
            Report {
                document: report.document,
                revision: report.revision,
                epoch: report.epoch,
                generation: report.generation,
                result,
            },
            VIEWPORT_ROWS,
        )
    }
    fn valid_page(&self, page: &ViewportPage) -> bool {
        page.viewport == self.viewport()
            && page.rows.len() <= page.viewport.rows
            && page.rows.iter().map(|row| row.bytes.len()).sum::<usize>() <= VIEWPORT_BYTES
            && page.rows.iter().map(|row| row.spans.len()).sum::<usize>() <= 8192
            && page.rows.iter().all(|row| {
                row.line_start <= row.offset
                    && row.offset <= row.line_end
                    && row.bytes.len() <= self.row_bytes
                    && row.bytes.len() as u64 <= row.line_end - row.offset
                    && row.spans.iter().all(|span| {
                        span.range.start <= span.range.end
                            && span.range.start as u64 >= row.offset
                            && span.range.end as u64 <= row.offset + row.bytes.len() as u64
                    })
            })
            && page.rows.windows(2).all(|pair| pair[0].line < pair[1].line && pair[0].line_end < pair[1].line_start)
            && (page.rows.is_empty()
                || self.page_start.checked_add(page.rows.len() as u64).is_some_and(|n| n <= page.total))
    }
    fn projection_rows(&self, current: Option<Source>) -> &[ViewportRow] {
        if !self.open
            || current != self.source
            || self.loaded != self.expected
            || self.loaded.is_none()
            || self.draft
            || self.error.is_some()
        {
            &[]
        } else {
            &self.rows
        }
    }
    fn destination(&self) -> Option<Composition> {
        if !self.open {
            return None;
        }
        Some(Composition { source: self.source?, input_epoch: self.input_epoch, field: self.field()? })
    }
    fn commit(&mut self, token: Composition, text: &str) -> bool {
        if self.destination() != Some(token) {
            return false;
        }
        self.insert(text);
        true
    }
    fn insert(&mut self, text: &str) {
        let Some(field) = self.field() else { return };
        match self.edit(field).insert(text) {
            Ok(true) => self.changed(),
            Ok(false) => {}
            Err(error) => {
                self.error = Some(error.into());
                self.bump();
            }
        }
    }
    fn focus(&mut self, target: Target) {
        self.from.all = false;
        self.to.all = false;
        self.focus = target;
        self.bump();
    }
    fn tab(&mut self, back: bool) {
        let index = ORDER.iter().position(|&target| target == self.focus).unwrap_or(0);
        self.focus(ORDER[(index + if back { ORDER.len() - 1 } else { 1 }) % ORDER.len()]);
    }
    fn geometry_matches(&self, size: (usize, usize), metrics: &Metrics) -> bool {
        let width = if self.source_width == 0 { size.0.saturating_sub(metrics.bar) } else { self.source_width };
        let area = layout(
            kwil_view_model::TabArea {
                x: 0,
                y: self.body_top.min(size.1),
                width: width.min(size.0),
                height: size.1.saturating_sub(self.body_top.saturating_add(metrics.status)),
            },
            metrics,
            true,
        )
        .bar;
        self.geometry_epoch == self.input_epoch && self.surface == Surface::new(size, metrics) && self.area == area
    }
    fn navigation(&self, source: Option<Source>, index: usize) -> Option<u64> {
        if !self.open
            || source != self.source
            || self.loaded != self.expected
            || self.loaded.is_none()
            || self.draft
            || self.error.is_some()
        {
            return None;
        }
        self.hits.get(index)?.line.checked_add(1)
    }
    #[cfg(test)]
    fn can_next(&self) -> bool {
        !self.draft
            && self.loaded.is_some()
            && self.loaded == self.expected
            && self.total > self.page_start.saturating_add(PAGE as u64)
    }
    #[cfg(test)]
    fn page(&mut self, next: bool) {
        if self.draft || (next && !self.can_next()) || (!next && self.page_start == 0) {
            return;
        }
        self.page_start = if next {
            self.page_start.saturating_add(PAGE as u64)
        } else {
            self.page_start.saturating_sub(PAGE as u64)
        };
        self.invalidate();
        self.want = self.open;
    }
    fn move_viewport(&mut self, top: u64, horizontal: u64) {
        let top = if self.exact { top.min(self.total.saturating_sub(self.visible as u64)) } else { top };
        let horizontal = horizontal.min(self.max_line_bytes);
        if self.page_start == top && self.horizontal == horizontal {
            return;
        }
        let (total, exact, maximum) = (self.total, self.exact, self.max_line_bytes);
        self.invalidate();
        self.total = total;
        // exact describes the index extent; a fresh page still needs a report.
        self.exact = exact;
        self.max_line_bytes = maximum;
        self.page_start = top;
        self.horizontal = horizontal;
        self.want = self.open && !self.draft;
    }
    fn status(&self) -> String {
        if let Some(error) = &self.error {
            return format!("Filter view · read-only · {error}");
        }
        if let Some(notice) = &self.notice {
            return format!("Filter view · read-only · {notice}");
        }
        if self.source.is_none() {
            return "Filter view · read-only · Waiting for a text document".into();
        }
        if self.draft {
            return "Filter view · read-only · Filter changed — Apply".into();
        }
        if self.want {
            return "Filter view · read-only · Waiting for log worker…".into();
        }
        if self.busy {
            return format!("Filter view · read-only · {}+ rows · indexing… · click/Enter opens original", self.total);
        }
        if self.exact {
            return format!(
                "Filter view · read-only · {} rows · {} · click/Enter opens original · Esc closes",
                self.total,
                if self.following { "Following · scroll up to pause" } else { "Follow paused" }
            );
        }
        "Filter view · read-only · Apply to show matching lines".into()
    }
}

impl App {
    pub(super) fn is_log_document(&self) -> bool {
        if let Some(language) = self.highlighting.as_ref().and_then(|host| host.detected_language(&self.view)) {
            return language == kwil_core_syntax::Language::Log;
        }
        self.view.is_log_document()
            || self.view.editor().is_none()
                && self.logs.following
                && self.logs.follow_document == Some(self.view.active_id())
    }

    pub(super) fn filter_log_level(&mut self, level: Option<Level>) {
        if !self.logs.is_open() {
            self.dispatch_logs("view.logFilters");
        }
        self.logs.levels = level.map_or_else(all_levels, Level::bit);
        self.logs.keep_unknown = level.is_none();
        self.logs.apply();
        self.sync_logs();
        self.logs_repaint();
    }

    pub(super) fn logs_blocked(&self) -> bool {
        self.file_modal()
            || self.save_path.is_some()
            || self.view.file_prompt().is_some()
            || self.settings_open()
            || self.palette_open()
            || self.search_panel.is_open()
            || self.menu.state.is_open()
            || self.settings_choice_open()
    }
    fn logs_repaint(&mut self) {
        self.logs.paint.request();
        self.request_frame();
    }
    pub(super) fn close_logs(&mut self) {
        if !self.logs.is_open() {
            return;
        }
        self.logs.close();
        self.logs.following = false;
        self.logs_cancel_input();
        self.logs_repaint();
    }
    fn logs_cancel_input(&mut self) {
        self.ime_destination = Some(ime::Destination::Cancelled);
        self.composing = false;
        if let Some(keys) = &mut self.keys {
            keys.cancel_pending();
        }
    }
    pub(super) fn dispatch_logs(&mut self, command: &str) -> bool {
        if command != "view.logFilters" && !command.starts_with("log.") {
            return false;
        }
        if !self.is_log_document() {
            let damage = self.view.set_notice("Open a log file first".into());
            self.apply(damage);
            return true;
        }
        if command == "log.highlightRules" {
            self.close_logs();
            self.dispatch_settings("view.settings");
            if let Some(panel) = &mut self.settings_panel
                && let Err(error) = panel.open_log_highlights()
            {
                self.view.set_notice(error);
            }
            return true;
        }
        if matches!(command, "log.toggleMark" | "log.nextMark" | "log.previousMark") {
            let line = self.logs.navigation(Source::capture(&self.view), self.logs.selected);
            if self.logs.is_open() && line.is_none() {
                let damage = self.view.set_notice("Wait for filtered log lines before marking or navigating".into());
                self.apply(damage);
                return true;
            }
            if command != "log.toggleMark" {
                self.close_logs();
            }
            let damage = match self.view.log_mark_command(command, line) {
                Ok(damage) => damage,
                Err(error) => self.view.set_notice(error.to_string()),
            };
            self.apply(damage);
            self.logs.body_paint.request();
            self.logs_repaint();
            return true;
        }
        if command == "log.pause" || command == "log.toggleFollow" && self.logs.following {
            self.logs.following = false;
            self.logs.notice = Some("Follow paused · Resume Follow to tail again".into());
            let damage = self.view.set_notice("Log follow paused".into());
            self.apply(damage);
            self.logs_repaint();
            return true;
        }
        if command == "view.logFilters" && self.logs.is_open() {
            self.close_logs();
            return true;
        }
        if self.file_modal() || self.save_path.is_some() || self.settings_open() || self.view.file_prompt().is_some() {
            return true;
        }
        self.close_search();
        if self.palette_open() {
            self.palette_input(kwil_view_model::palette_session::PaletteInput::Cancel);
        }
        self.menu.state = menu::State::Closed;
        self.menu_dirty = true;
        self.drag = None;
        self.selecting = false;
        self.logs_cancel_input();
        if !self.logs.open {
            self.logs.open = true;
            self.logs.invalidate();
            self.logs.want = !self.logs.draft;
        }
        match command {
            "log.toggleFollow" | "log.resume" => self.resume_log_follow(),
            "log.filter" => self.logs.focus(Target::Field(FieldId::Query)),
            "log.goToTime" => {
                self.logs.keep_unknown = false;
                self.logs.focus(Target::Field(FieldId::From));
            }
            "log.clearFilter" => self.logs.clear(),
            "log.toggleRegex" => {
                self.logs.regex = !self.logs.regex;
                self.logs.apply();
            }
            "log.toggleLevel" => {
                let index = match self.logs.focus {
                    Target::Level(index @ 1..=6) => index,
                    _ => 5,
                };
                self.logs.toggle_level(index);
                self.logs.focus(Target::Level(index));
                self.logs.apply();
            }
            _ => {}
        }
        self.sync_logs();
        self.logs_repaint();
        true
    }

    fn resume_log_follow(&mut self) {
        let path = self
            .view
            .editor()
            .and_then(|e| (!e.document().is_dirty()).then(|| e.document().path()).flatten())
            .map(|p| p.to_path_buf());
        let Some(path) = path else {
            self.logs.error = Some("Save log edits before following the file".into());
            return;
        };
        let id = self.view.active_id();
        let result = if let Some(watch) = &self.logs.follow_watch {
            if self.logs.follow_document == Some(id) { Ok(()) } else { watch.watch(path) }
        } else {
            let proxy = self.proxy.clone();
            kwil_view_model::log_follow::FollowWatch::start(path, move || {
                let _ = proxy.send_event(UserEvent::LogReady);
            })
            .map(|watch| self.logs.follow_watch = Some(watch))
        };
        match result {
            Ok(()) => {
                self.logs.follow_document = Some(id);
                self.logs.following = true;
                self.logs.follow_reload = true;
                self.logs.notice = Some("Following · scroll up to pause".into());
                self.logs.apply();
            }
            Err(error) => self.logs.error = Some(format!("Cannot follow log: {error}")),
        }
    }
    /// No polling: call on identity changes or the backend wake. At most one
    /// rejected snapshot is retained; the same owner is admitted before a new
    /// snapshot is cloned. Older accepted work retires on the backend worker.
    pub(super) fn sync_logs(&mut self) {
        if self.logs.follow_document != Some(self.view.active_id()) {
            self.logs.following = false;
        }
        if self.logs.following && self.logs.follow_reload && self.view.editor().is_some() {
            match self.view.reload_log() {
                Ok(()) => {
                    self.logs.follow_reload = false;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    self.logs.following = false;
                    self.logs.error = Some(format!("Follow paused: {error}"));
                }
            }
        }
        if self.logs.open && !self.is_log_document() {
            self.close_logs();
        }
        if !self.logs.open && self.logs.retry.is_none() {
            return;
        }
        // body_area() may already reserve this bar. Derive its unreserved top
        // from chrome, while ROOT's source-body width still owns split layout.
        let mut source_area = chrome::layout(self.size, &self.ui, self.settings.tabs.layout).body;
        source_area.width = self.body_area().width;
        let areas = layout(source_area, &self.ui, self.logs.open);
        self.logs.body_top = source_area.y;
        self.logs.source_width = source_area.width;
        self.logs.body = Rect {
            x: areas.body.x,
            y: areas.body.y,
            w: areas.body.width,
            h: areas.body.height.saturating_sub(self.ui.bar),
        };
        let visible = (self.logs.body.h / self.font.as_ref().map_or(16, |f| f.ch)).clamp(1, VIEWPORT_ROWS);
        let row_bytes = (VIEWPORT_BYTES / visible).min(ROW_BYTES);
        if self.logs.visible != visible || self.logs.row_bytes != row_bytes {
            self.logs.visible = visible;
            self.logs.row_bytes = row_bytes;
            let (total, exact, maximum) = (self.logs.total, self.logs.exact, self.logs.max_line_bytes);
            self.logs.invalidate();
            self.logs.total = total;
            self.logs.exact = exact;
            self.logs.max_line_bytes = maximum;
            self.logs.want = self.logs.open && !self.logs.draft;
        }
        let source = Source::capture(&self.view);
        let changed = source != self.logs.source;
        self.logs.sync_source(source);
        if changed && self.logs.open {
            self.logs_cancel_input();
        }
        if self.logs.session.is_none() && self.logs.open && self.logs.want {
            let Some(limit) = self.logs.index_limit else {
                self.logs.want = false;
                self.logs.error = Some("Filter view · read-only · Log index disk budget was not admitted".into());
                self.logs_repaint();
                return;
            };
            let proxy = self.proxy.clone();
            match LogSession::start_with_index_limit(limit, move || {
                let _ = proxy.send_event(UserEvent::LogReady);
            }) {
                Ok(session) => self.logs.session = Some(session),
                Err(error) => {
                    self.logs.want = false;
                    self.logs.error = Some(format!("Log worker unavailable: {error}"));
                    self.logs_repaint();
                    return;
                }
            }
        }
        if self.logs.session.is_none() || self.logs.retry.is_some() {
            if self.logs.paint.pending() {
                self.request_frame();
            }
            return;
        }
        if self.logs.open
            && self.logs.want
            && !self.logs.draft
            && let Some(source) = source
        {
            let retry = Retry {
                source,
                serial: self.logs.serial,
                snapshot: self.view.editor().expect("captured editor").document().snapshot(),
                filter: self.logs.filter,
                viewport: self.logs.viewport(),
            };
            match self.logs.session.as_mut().unwrap().submit_viewport(
                source.document,
                source.revision,
                source.epoch,
                retry.snapshot,
                retry.filter,
                retry.viewport,
            ) {
                Ok(generation) => self.logs.accepted(source, retry.serial, generation),
                Err(error) => self.logs.retry = Some(Retry { snapshot: error.snapshot, ..retry }),
            }
        }
        if self.logs.paint.pending() {
            self.request_frame();
        }
    }
    pub(super) fn logs_ready(&mut self) {
        if let Some(watch) = &self.logs.follow_watch {
            if watch.take_changed() {
                self.logs.follow_reload = true;
            }
            if let Some(error) = watch.take_error() {
                self.logs.following = false;
                self.logs.error = Some(format!("Log watcher: {error}"));
                self.logs.paint.request();
            }
        }
        self.sync_logs(); // sync identity BEFORE consuming a completion.
        // A rejected snapshot is retried only on the backend wake, never by
        // frames, hover, wheel or a timer. Keep it even if the card closed.
        if let Some(retry) = self.logs.retry.take() {
            let obsolete = !self.logs.open
                || self.logs.draft
                || self.logs.source != Some(retry.source)
                || self.logs.serial != retry.serial;
            let session = self.logs.session.as_mut().expect("retry owns a live log session");
            let submission = if obsolete {
                session.retire_snapshot(retry.snapshot)
            } else {
                session.submit_viewport(
                    retry.source.document,
                    retry.source.revision,
                    retry.source.epoch,
                    retry.snapshot,
                    retry.filter,
                    retry.viewport,
                )
            };
            match submission {
                Ok(generation) => self.logs.accepted(retry.source, retry.serial, generation),
                Err(error) => self.logs.retry = Some(Retry { snapshot: error.snapshot, ..retry }),
            }
            self.sync_logs(); // a retired stale retry may admit the current source.
        }
        let reports = self.logs.session.as_ref().map(LogSession::drain_viewport).unwrap_or_default();
        for report in reports {
            self.logs.receive_viewport(report);
        }
        if self.logs.following && self.logs.exact && !self.logs.draft {
            let top = self.logs.total.saturating_sub(self.logs.visible as u64);
            if self.logs.page_start != top {
                self.logs.move_viewport(top, self.logs.horizontal);
                self.sync_logs();
            }
        }
        if self.logs.paint.pending() {
            self.request_frame();
        }
    }
    fn logs_activate(&mut self, index: usize) {
        let Some(line) = self.logs.navigation(Source::capture(&self.view), index) else {
            return;
        };
        self.logs.selected = index;
        // ROOT explicitly retains the same bounded Go to Line / Find adapter.
        // No new source lookup is introduced here to build snippets/offsets.
        // All-worker source-admission acceptance remains a separate ROOT gate.
        let result = self.view.execute(
            "navigation.goToLine",
            &CommandArgs { line: Some(line), column: Some(1), extend: false, ..Default::default() },
            1,
        );
        match result {
            Ok(result) => {
                // The filter projection has no editable selection. Resolve the
                // strict original line first, then reveal it in the real editor.
                self.close_logs();
                self.apply(result.damage);
                self.keep_caret_visible();
                self.command_status(result.status);
            }
            Err(error) => self.logs.error = Some(format!("Cannot jump to line: {error}")),
        }
        self.logs.bump();
        self.logs_repaint();
    }
    fn logs_target(&mut self, target: Target) {
        self.logs_cancel_input();
        match target {
            Target::Close => self.close_logs(),
            Target::Hit(index) => {
                self.logs.focus(Target::Results);
                self.logs_activate(index);
            }
            Target::Field(field) => {
                self.logs.focus(Target::Field(field));
            }
            Target::Level(index) => {
                self.logs.focus(target);
                self.logs.toggle_level(index);
            }
            Target::Unknown => {
                self.logs.focus(target);
                self.logs.keep_unknown = !self.logs.keep_unknown;
                self.logs.changed();
            }
            Target::Apply => {
                self.logs.focus(target);
                self.logs.apply();
            }
            Target::Clear => {
                self.logs.focus(target);
                self.logs.clear();
            }
            Target::Results => self.logs.focus(target),
        }
        self.sync_logs();
        self.logs_repaint();
    }
    pub(super) fn logs_key(&mut self, event: &HostKey<'_>) -> bool {
        if !self.logs.open || self.logs_blocked() {
            return false;
        }
        self.sync_logs();
        if self.logs.focus == Target::Results
            && matches!(event.logical_key, Key::Named(NamedKey::ArrowUp | NamedKey::PageUp | NamedKey::Home))
        {
            self.logs.following = false;
        }
        let ctrl = self.mods.control_key() || self.mods.super_key();
        // Keep the palette and configured log shortcuts reachable while the
        // read-only projection owns the body. All other editor input remains
        // fenced by this host; query editing uses its existing field path.
        if (ctrl || self.mods.alt_key() || !matches!(event.logical_key, Key::Character(_)))
            && let Some(chord) = input::chord(&event.logical_key, self.mods)
        {
            if self.keys.is_none() {
                self.keys =
                    kwil_core_commands::keymap::KeymapEngine::new(self.settings.keymap, &self.settings.bindings).ok();
            }
            if let Some(keys) = &mut self.keys {
                keys.set_context(kwil_core_commands::Context::Editor);
                use kwil_core_commands::vim::VimResult;
                match keys.feed(chord) {
                    VimResult::Pending => return true,
                    VimResult::Command(call) if call.id.starts_with("log.") => {
                        self.dispatch_logs(call.id);
                        return true;
                    }
                    VimResult::Command(call) if call.id == "view.commandPalette" => {
                        self.dispatch_palette(call.id);
                        return true;
                    }
                    VimResult::Unbound if keys.suppresses_fallback() => return true,
                    _ => {}
                }
            }
        }
        if event.repeat && matches!(event.logical_key, Key::Named(NamedKey::Escape | NamedKey::Enter)) {
            return true;
        }
        if event.logical_key == Key::Named(NamedKey::Escape) {
            self.close_logs();
            return true;
        }
        if self.composing {
            return true;
        }
        match &event.logical_key {
            Key::Named(NamedKey::Tab) => {
                self.logs_cancel_input();
                self.logs.tab(self.mods.shift_key());
            }
            Key::Named(NamedKey::Enter) => {
                let target = if matches!(self.logs.focus, Target::Results) {
                    Target::Hit(self.logs.selected)
                } else if self.logs.field().is_some() {
                    Target::Apply
                } else {
                    self.logs.focus
                };
                self.logs_target(target);
                return true;
            }
            Key::Named(NamedKey::Space) if self.logs.field().is_none() => {
                self.logs_target(self.logs.focus);
                return true;
            }
            Key::Named(NamedKey::ArrowUp | NamedKey::ArrowDown) if self.logs.focus == Target::Results => {
                let up = event.logical_key == Key::Named(NamedKey::ArrowUp);
                if (up && self.logs.selected == 0 && self.logs.page_start > 0)
                    || (!up && self.logs.selected + 1 >= self.logs.hits.len())
                {
                    let top = if up {
                        self.logs.page_start.saturating_sub(1)
                    } else {
                        self.logs.page_start.saturating_add(1)
                    };
                    self.logs.move_viewport(top, self.logs.horizontal);
                    self.sync_logs();
                } else {
                    self.logs.selected = if up {
                        self.logs.selected.saturating_sub(1)
                    } else {
                        (self.logs.selected + 1).min(self.logs.hits.len().saturating_sub(1))
                    };
                }
                self.logs.scroll =
                    desktop::reveal(self.logs.scroll, self.logs.selected, self.logs.visible, self.logs.hits.len());
                self.logs.bump();
            }
            Key::Named(NamedKey::PageUp | NamedKey::PageDown) if self.logs.focus == Target::Results => {
                let top = if event.logical_key == Key::Named(NamedKey::PageUp) {
                    self.logs.page_start.saturating_sub(self.logs.visible as u64)
                } else {
                    self.logs.page_start.saturating_add(self.logs.visible as u64)
                };
                self.logs.move_viewport(top, self.logs.horizontal);
                self.sync_logs();
            }
            Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowRight | NamedKey::Home | NamedKey::End)
                if self.logs.focus == Target::Results =>
            {
                let (top, horizontal) = match event.logical_key {
                    Key::Named(NamedKey::ArrowLeft) => (self.logs.page_start, self.logs.horizontal.saturating_sub(16)),
                    Key::Named(NamedKey::ArrowRight) => (self.logs.page_start, self.logs.horizontal.saturating_add(16)),
                    Key::Named(NamedKey::Home) => (0, 0),
                    _ => (self.logs.total.saturating_sub(self.logs.visible as u64), self.logs.horizontal),
                };
                self.logs.move_viewport(top, horizontal);
                self.sync_logs();
            }
            Key::Character(c)
                if ctrl
                    && !self.mods.alt_key()
                    && c.eq_ignore_ascii_case("c")
                    && self.logs.focus == Target::Results =>
            {
                // Copy worker bytes with their original range, never displayed
                // escape glyphs or a stale hidden-editor selection.
                if let Some(row) = self.logs.projection_rows(Source::capture(&self.view)).get(self.logs.selected) {
                    let hwnd = self.gfx.as_ref().map_or(0, |gfx| gfx.hwnd);
                    let result = copy_row(row).and_then(|data| clipboard::SystemClipboard(hwnd).set(data));
                    self.logs.notice = Some(match result {
                        Ok(()) => format!("Copied original line {}", row.line.saturating_add(1)),
                        Err(error) => format!("Cannot copy row: {error}"),
                    });
                }
                self.logs.bump();
            }
            Key::Named(
                key @ (NamedKey::Backspace
                | NamedKey::Delete
                | NamedKey::ArrowLeft
                | NamedKey::ArrowRight
                | NamedKey::Home
                | NamedKey::End),
            ) => {
                if let Some(field) = self.logs.field() {
                    if self.logs.edit(field).key(*key) {
                        self.logs.changed();
                    } else {
                        self.logs.bump();
                    }
                }
            }
            Key::Character(c) if ctrl && !self.mods.alt_key() && c.eq_ignore_ascii_case("a") => {
                if let Some(field) = self.logs.field() {
                    self.logs.edit(field).all = true;
                    self.logs.bump();
                }
            }
            Key::Character(c) if ctrl && !self.mods.alt_key() && c.eq_ignore_ascii_case("v") => {
                if self.logs.field().is_some() {
                    let hwnd = self.gfx.as_ref().map_or(0, |gfx| gfx.hwnd);
                    match clipboard::SystemClipboard(hwnd).get_text(INPUT_BYTES) {
                        Ok(Some(text)) => self.logs.insert(&text),
                        Ok(None) => {}
                        Err(error) => {
                            self.logs.error = Some(format!("Cannot paste timestamp: {error}"));
                            self.logs.bump();
                        }
                    }
                }
            }
            _ if !ctrl || (self.mods.control_key() && self.mods.alt_key()) => {
                if let Some(text) = event.text {
                    self.logs.insert(text);
                }
            }
            _ => {}
        }
        self.logs_repaint();
        true // all remaining keys belong to the card, never the document.
    }
    pub(super) fn capture_logs_composition(&self) -> Option<Composition> {
        if self.logs_blocked() || Source::capture(&self.view) != self.logs.source {
            return None;
        }
        self.logs.destination()
    }
    /// ROOT calls this on focus loss or before opening a higher-priority
    /// modal/dropdown, even if no token was captured in ROOT's IME slot.
    pub(super) fn cancel_logs_composition(&mut self) {
        if self.logs.open {
            self.logs.bump();
        }
        self.logs_cancel_input();
    }
    pub(super) fn logs_commit(&mut self, token: Composition, text: &str) -> bool {
        if self.logs_blocked() || Source::capture(&self.view) != Some(token.source) {
            return false;
        }
        if !self.logs.commit(token, text) {
            return false;
        }
        self.logs_repaint();
        true
    }
    pub(super) fn logs_mouse(&mut self) -> bool {
        if !self.logs.open || self.logs_blocked() {
            return false;
        }
        self.sync_logs();
        if !self.logs.owns_point(self.pointer) {
            return false;
        }
        if !self.logs.geometry_matches(self.size, &self.ui) {
            self.logs_repaint();
            return true;
        }
        if let Some(target) = self.logs.geometry.at(self.pointer) {
            self.logs_target(target);
        } else if self.logs.body.contains(self.pointer) {
            self.logs.focus(Target::Results);
            let row = self.pointer.1.saturating_sub(self.logs.body.y) / self.ch();
            self.logs_activate(row);
        } else {
            // Chrome remains ROOT-owned. The projection consumes its entire
            // document body, including blank rows and every mouse button.
            return false;
        }
        true
    }
    pub(super) fn logs_track(&mut self) -> bool {
        if !self.logs.open || self.logs_blocked() {
            return false;
        }
        let hover =
            self.logs.geometry_matches(self.size, &self.ui).then(|| self.logs.geometry.at(self.pointer)).flatten();
        if self.logs.hover != hover {
            self.logs.hover = hover;
            self.logs_repaint();
        }
        self.logs.owns_point(self.pointer)
    }
    pub(super) fn logs_wheel(&mut self, up: bool) -> bool {
        if !self.logs.open || self.logs_blocked() {
            return false;
        }
        if !self.logs.owns_point(self.pointer) {
            return false;
        }
        if !self.logs.geometry_matches(self.size, &self.ui) {
            self.logs_repaint();
            return true;
        }
        if self.logs.body.contains(self.pointer) || self.logs.area.is_some_and(|r| r.contains(self.pointer)) {
            if up {
                self.logs.following = false;
            }
            let top = if up { self.logs.page_start.saturating_sub(3) } else { self.logs.page_start.saturating_add(3) };
            self.logs.move_viewport(top, self.logs.horizontal);
            self.sync_logs();
            self.logs_repaint();
            return true;
        }
        false
    }

    /// ROOT's scrollbar drag/track route, before ordinary Workspace scrolling.
    /// Uses matching-row ordinals and original-line byte displacement.
    pub(super) fn logs_scroll_to(&mut self, top: u64, horizontal: u64) -> bool {
        if !self.logs.open || self.logs_blocked() {
            return false;
        }
        if top < self.logs.total.saturating_sub(self.logs.visible as u64) {
            self.logs.following = false;
        }
        self.logs.move_viewport(top, horizontal);
        self.sync_logs();
        self.logs_repaint();
        true
    }
}

/// Pure reserved-height contract: pass the UNRESERVED source body (after any
/// source/preview width split). ROOT's body_area() returns layout(...).body.
/// The filter consumes height, never overlays document pixels or adds a list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Layout {
    pub bar: Option<Rect>,
    pub body: kwil_view_model::TabArea,
}
pub(super) fn layout(source: kwil_view_model::TabArea, m: &Metrics, open: bool) -> Layout {
    if !open || source.width == 0 || source.height == 0 {
        return Layout { bar: None, body: source };
    }
    let rows = if source.width >= m.dip(1200.0) {
        1
    } else if source.width >= m.dip(800.0) {
        2
    } else {
        3
    };
    let height = ((rows + 1) * m.dip(32.0) + (rows + 3) * m.dip(4.0) + m.dip(20.0)).min(source.height);
    Layout {
        bar: Some(Rect { x: source.x, y: source.y, w: source.width, h: height }),
        body: kwil_view_model::TabArea { y: source.y.saturating_add(height), height: source.height - height, ..source },
    }
}
#[cfg(test)]
fn card_area(size: (usize, usize), m: &Metrics, top: usize) -> Option<Rect> {
    layout(
        kwil_view_model::TabArea {
            x: 0,
            y: top.min(size.1),
            width: size.0.saturating_sub(m.bar),
            height: size.1.saturating_sub(top.saturating_add(m.status)),
        },
        m,
        true,
    )
    .bar
}
fn clipped_row(inner: Rect, y: usize, h: usize) -> Rect {
    Rect { x: inner.x, y: y.min(inner.y + inner.h), w: inner.w, h: h.min((inner.y + inner.h).saturating_sub(y)) }
}

/// Chrome font for controls only. ROOT paints this in reserved layout.bar,
/// before its menus/modals. Draw the projection body separately with draw_body.
pub(super) fn draw(
    host: &mut Host,
    pixels: &mut [u32],
    size: (usize, usize),
    font: &mut font::UiFont,
    theme: &Theme,
    m: &Metrics,
) -> Option<Rect> {
    host.geometry = Hits::default();
    host.caret = None;
    if !host.open {
        return None;
    }
    let width = if host.source_width == 0 { size.0.saturating_sub(m.bar) } else { host.source_width };
    let area = layout(
        kwil_view_model::TabArea {
            x: 0,
            y: host.body_top.min(size.1),
            width: width.min(size.0),
            height: size.1.saturating_sub(host.body_top.saturating_add(m.status)),
        },
        m,
        true,
    )
    .bar;
    if host.area != area {
        host.restore = host.area.or(host.restore);
    }
    host.area = area;
    let r = area?;
    host.surface = Surface::new(size, m);
    host.geometry_epoch = host.input_epoch;
    let c = Colors::new(theme);
    let mut s = Sizes::new(m);
    s.gap = m.dip(4.0);
    s.inner = m.dip(6.0);
    chrome::fill_clipped(pixels, size.0, r, c.sidebar);
    let inner = inset(r, s.gap);
    let rows = if width >= m.dip(1200.0) {
        1
    } else if width >= m.dip(800.0) {
        2
    } else {
        3
    };
    if inner.w < m.dip(320.0) || inner.h < s.control + s.text {
        desktop::label(
            pixels,
            size.0,
            font,
            inner,
            "Filter view · read-only · enlarge window for controls · Esc closes",
            c.muted,
        );
        return Some(r);
    }
    let chip_total = if rows == 1 {
        m.dip(336.0)
    } else if rows == 2 {
        m.dip(392.0)
    } else {
        inner.w.saturating_sub(s.control + s.gap)
    };
    let chip_w = chip_total.saturating_sub(6 * s.gap) / 7;
    for index in 0..7 {
        let rect = Rect { x: inner.x + index * (chip_w + s.gap), y: inner.y, w: chip_w, h: s.control };
        let chosen =
            if index == 0 { host.levels == all_levels() } else { host.levels & Level::ALL[index - 1].bit() != 0 };
        draw_button(
            host,
            pixels,
            size.0,
            font,
            rect,
            ["All", "Trace", "Debug", "Info", "Warn", "Error", "Fatal"][index],
            Target::Level(index),
            chosen,
            true,
            &c,
            &s,
        );
    }
    let close = Rect { x: inner.x + inner.w - s.control, y: inner.y, w: s.control, h: s.control };
    desktop::icon_button(pixels, size.0, close, Icon::Close, host.hover == Some(Target::Close), true, &c, &s);
    host.geometry.push(close, Target::Close);
    let actions_y = inner.y + if rows == 3 { 2 * (s.control + s.gap) } else { 0 };
    let actions_x = if rows == 3 { inner.x } else { inner.x + inner.w - m.dip(260.0) - s.control - s.gap };
    let unknown = Rect { x: actions_x, y: actions_y, w: m.dip(112.0), h: s.control };
    desktop::checkbox(
        pixels,
        size.0,
        font,
        unknown,
        "Unknown",
        host.keep_unknown,
        host.hover == Some(Target::Unknown),
        true,
        &c,
        &s,
    );
    host.geometry.push(unknown, Target::Unknown);
    focus_ring(pixels, size.0, unknown, host.focus == Target::Unknown, &c, &s);
    for (index, (target, text)) in [(Target::Apply, "Apply"), (Target::Clear, "Clear")].into_iter().enumerate() {
        let rect = Rect {
            x: unknown.x + unknown.w + s.gap + index * (m.dip(64.0) + s.gap),
            y: actions_y,
            w: m.dip(64.0),
            h: s.control,
        };
        draw_button(host, pixels, size.0, font, rect, text, target, target == Target::Apply, true, &c, &s);
    }
    let fields_x = if rows == 1 { inner.x + chip_total + s.gap } else { inner.x };
    let fields_y = inner.y + if rows == 1 { 0 } else { s.control + s.gap };
    let fields_w = if rows == 1 { actions_x.saturating_sub(fields_x + s.gap) } else { inner.w };
    let field_w = fields_w.saturating_sub(s.gap) / 2;
    for (index, (field, label)) in [(FieldId::From, "From"), (FieldId::To, "To")].into_iter().enumerate() {
        let x = fields_x + index * (field_w + s.gap);
        let label_w = m.dip(36.0);
        desktop::label(pixels, size.0, font, Rect { x, y: fields_y, w: label_w, h: s.control }, label, c.muted);
        let rect = Rect { x: x + label_w, y: fields_y, w: field_w.saturating_sub(label_w), h: s.control };
        let edit = match field {
            FieldId::Query => &host.query,
            FieldId::From => &host.from,
            FieldId::To => &host.to,
        };
        let focused = host.focus == Target::Field(field);
        let caret = desktop::text_field(
            pixels,
            size.0,
            font,
            rect,
            Field {
                text: &edit.text,
                cursor: edit.cursor,
                placeholder: "Any time",
                focused,
                hovered: host.hover == Some(Target::Field(field)),
                search: false,
                clear: false,
            },
            &c,
            &s,
        );
        if focused {
            host.caret = caret;
        }
        focus_ring(pixels, size.0, rect, focused && edit.all, &c, &s);
        host.geometry.push(rect, Target::Field(field));
    }
    let query_y = inner.y + rows * (s.control + s.gap);
    let label_w = m.dip(120.0).min(inner.w);
    desktop::label(
        pixels,
        size.0,
        font,
        Rect { x: inner.x, y: query_y, w: label_w, h: s.control },
        if host.regex { "Filter · regex" } else { "Filter · literal" },
        c.muted,
    );
    let rect = Rect { x: inner.x + label_w, y: query_y, w: inner.w.saturating_sub(label_w), h: s.control };
    let focused = host.focus == Target::Field(FieldId::Query);
    let caret = desktop::text_field(
        pixels,
        size.0,
        font,
        rect,
        Field {
            text: &host.query.text,
            cursor: host.query.cursor,
            placeholder: "Text · AND / OR / NOT · Enter applies",
            focused,
            hovered: host.hover == Some(Target::Field(FieldId::Query)),
            search: true,
            clear: false,
        },
        &c,
        &s,
    );
    if focused {
        host.caret = caret;
    }
    host.geometry.push(rect, Target::Field(FieldId::Query));
    let status = clipped_row(inner, query_y + s.control + s.gap, s.text);
    desktop::label(pixels, size.0, font, status, &host.status(), if host.error.is_some() { c.error } else { c.muted });
    Some(r)
}

/// Paint into ROOT's retained source-body buffer, using the existing editor
/// Font, draw_text/grid byte mapping, syntax classes and active theme colours.
/// ROOT skips ordinary draw_row/caret/selection/marks while owns_body() is true.
/// `current` must be captured after source synchronization, never the loaded key.
/// No snapshot, disk, parser, selection or command executes while drawing.
pub(super) fn draw_body(
    host: &Host,
    pixels: &mut [u32],
    stride: usize,
    font: &mut Font,
    current: Option<Source>,
    view: &View,
) -> bool {
    if !host.open {
        return false;
    }
    if stride == 0 {
        return true;
    }
    let height = pixels.len() / stride;
    pixels.fill(bg());
    let rows = host.projection_rows(current);
    let digits = rows.last().map_or(1, |row| row.line.saturating_add(1).to_string().len());
    let gutter_columns = GUTTER_COLS.max(digits + 1).min(stride / font.cw.max(1));
    let gutter = gutter_columns * font.cw;
    let columns = stride.saturating_sub(gutter) / font.cw.max(1);
    for (index, row) in rows.iter().enumerate().take(height / font.ch.max(1)) {
        let y = index * font.ch;
        let row_bg =
            if host.focus == Target::Results && index == host.selected { chrome::mix(bg(), fg(), 0x12) } else { bg() };
        fill(pixels, stride, Rect { x: 0, y, w: gutter, h: font.ch }, gutter_bg());
        fill(pixels, stride, Rect { x: gutter, y, w: stride - gutter, h: font.ch }, row_bg);
        let number = format!(
            "{:>width$}{}",
            row.line.saturating_add(1),
            if view.log_marked(row.line + 1) { '*' } else { ' ' },
            width = gutter_columns.saturating_sub(1)
        );
        draw_text(pixels, stride, font, 0, y, number.as_bytes(), gutter_columns, gutter_fg(), gutter_bg());
        draw_text(pixels, stride, font, gutter, y, &row.bytes, columns, fg(), row_bg);
        if row.bytes.iter().take(columns).all(|byte| (32..127).contains(byte)) {
            for span in &row.spans {
                let start = (span.range.start as u64)
                    .saturating_sub(row.offset)
                    .min(columns as u64)
                    .min(row.bytes.len() as u64) as usize;
                let end =
                    (span.range.end as u64).saturating_sub(row.offset).min(columns as u64).min(row.bytes.len() as u64)
                        as usize;
                if start >= end {
                    continue;
                }
                let x = gutter + start * font.cw;
                fill(pixels, stride, Rect { x, y, w: (end - start) * font.cw, h: font.ch }, row_bg);
                draw_text(
                    pixels,
                    stride,
                    font,
                    x,
                    y,
                    &row.bytes[start..end],
                    end - start,
                    theme_ui::syntax_rgb(kwil_view_model::highlight_session::syntax_rgb(span.class)),
                    row_bg,
                );
            }
            continue;
        }
        let cells = text::cells(&row.bytes, columns, font.tab_size());
        // Reuse the editor's bidi paragraph rule; per-cell colour overlays
        // must not reorder an already-shaped RTL paragraph.
        if !cells.iter().any(|cell| shaping::needs_bidi(&cell.text)) {
            let mut span_index = 0;
            for cell in cells {
                if cell.text == "\t" {
                    continue;
                }
                let offset = row.offset + cell.bytes.start as u64;
                while row.spans.get(span_index).is_some_and(|span| span.range.end as u64 <= offset) {
                    span_index += 1;
                }
                if let Some(span) = row.spans.get(span_index).filter(|span| span.range.start as u64 <= offset) {
                    let color = theme_ui::syntax_rgb(kwil_view_model::highlight_session::syntax_rgb(span.class));
                    let x = gutter + cell.column * font.cw;
                    fill(pixels, stride, Rect { x, y, w: cell.width * font.cw, h: font.ch }, row_bg);
                    draw_text(pixels, stride, font, x, y, cell.text.as_bytes(), cell.width, color, row_bg);
                }
            }
        }
    }
    true
}

fn copy_row(row: &ViewportRow) -> std::io::Result<kwil_view_model::ClipboardData> {
    if row.offset != row.line_start || row.offset.checked_add(row.bytes.len() as u64) != Some(row.line_end) {
        return Err(std::io::Error::other("Only a byte window is loaded; open original to copy the full row"));
    }
    let text = std::str::from_utf8(&row.bytes)
        .map_err(|_| std::io::Error::other("Invalid UTF-8; open original to copy bytes"))?;
    if text.contains('\0') {
        return Err(std::io::Error::other("NUL cannot be preserved by the system clipboard"));
    }
    kwil_view_model::ClipboardData::text(&row.bytes)
}
#[allow(clippy::too_many_arguments)]
fn draw_button(
    host: &mut Host,
    pixels: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    rect: Rect,
    text: &str,
    target: Target,
    primary: bool,
    enabled: bool,
    c: &Colors,
    s: &Sizes,
) {
    desktop::button(
        pixels,
        stride,
        font,
        rect,
        text,
        if primary { Style::Primary } else { Style::Secondary },
        enabled && host.hover == Some(target),
        c,
        s,
    );
    if enabled {
        host.geometry.push(rect, target);
    }
    focus_ring(pixels, stride, rect, host.focus == target, c, s);
}
fn focus_ring(pixels: &mut [u32], stride: usize, rect: Rect, focused: bool, c: &Colors, s: &Sizes) {
    if focused && rect.w > 2 * s.inner && rect.h > 2 * s.focus {
        chrome::fill_clipped(
            pixels,
            stride,
            Rect { x: rect.x + s.inner, y: rect.y + rect.h - s.focus, w: rect.w - 2 * s.inner, h: s.hairline },
            c.accent,
        );
    }
}

/// Formatting only, never a second parser. Gregorian UTC, full nanosecond
/// precision; out-of-calendar timestamps retain an explicit epoch unit.
#[cfg(test)]
fn timestamp_label(time: Timestamp) -> String {
    let seconds = time.0.div_euclid(1_000_000_000);
    let fraction = time.0.rem_euclid(1_000_000_000);
    let days = seconds.div_euclid(86400);
    if !(-719528..=2932896).contains(&days) {
        return format!("{} ns since Unix epoch", time.0);
    }
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i128::from(month <= 2);
    let sec = seconds.rem_euclid(86400);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}.{fraction:09}Z", sec / 3600, sec / 60 % 60, sec % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kwil_view_model::log_session::Metadata;

    const SOURCE: Source = Source { document: 5, revision: 7, epoch: 11 };

    #[test]
    fn log_commands_require_a_log_document() {
        for command in kwil_core_commands::COMMANDS.iter().filter(|c| c.id.starts_with("log.")) {
            let (mut app, _events) = crate::search_ui::host_dispatch_tests::host("plain text");
            assert!(app.dispatch_logs(command.id), "{} needs a host", command.id);
            assert_eq!(app.view.notice(), "Open a log file first", "{}", command.id);
        }
    }

    #[test]
    fn log_commands_edit_the_existing_projection_and_marks() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "kwil-log-commands-{}-{}.log",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&path, b"INFO first\nERROR second\nWARN third\n").unwrap();
        let file = kwil_core_text::TextFile::open(&path).unwrap();
        file.build_index(1).unwrap();
        let (mut app, events) = crate::search_ui::host_dispatch_tests::host("");
        app.view = Workspace::new(View::new(Some((path.clone(), file)))).unwrap();
        app.logs.admit_projection_disk(1 << 20);
        assert!(app.dispatch_logs("log.filter"));
        assert_eq!(app.logs.field(), Some(FieldId::Query));
        app.logs.insert("second");
        app.logs.apply();
        app.sync_logs();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !app.logs.exact {
            events.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).unwrap();
            app.logs_ready();
        }
        assert_eq!(app.logs.total, 1);
        assert_eq!(app.logs.rows[0].line, 1);
        app.dispatch_logs("log.toggleMark");
        assert!(app.view.log_marked(2));
        app.dispatch_logs("log.clearFilter");
        assert_eq!(app.logs.filter, Filter::default());
        app.dispatch_logs("log.toggleRegex");
        assert!(app.logs.regex);
        app.dispatch_logs("log.toggleLevel");
        assert_eq!(app.logs.focus, Target::Level(5));
        assert_eq!(app.logs.levels & Level::Error.bit(), 0);
        app.dispatch_logs("log.goToTime");
        assert_eq!(app.logs.field(), Some(FieldId::From));
        assert!(!app.logs.keep_unknown);
        app.close_logs();
        app.dispatch_logs("log.nextMark");
        assert!(!app.logs.is_open());
        assert_eq!(
            app.view.editor().unwrap().byte_position(app.view.editor().unwrap().primary_cursor().head).unwrap().0,
            1
        );
        app.logs.settle_until(deadline).unwrap();
        drop(app);
        std::fs::remove_file(path).unwrap();
    }

    fn row(bytes: &[u8]) -> ViewportRow {
        ViewportRow {
            line: 41,
            line_start: 300,
            line_end: 300 + bytes.len() as u64,
            offset: 300,
            bytes: bytes.to_vec(),
            spans: Vec::new(),
        }
    }
    fn viewport_report(host: &Host, generation: u64, rows: Vec<ViewportRow>) -> ViewportReport {
        ViewportReport {
            document: SOURCE.document,
            revision: SOURCE.revision,
            epoch: SOURCE.epoch,
            generation,
            result: Ok(ViewportPage {
                viewport: host.viewport(),
                total: rows.len() as u64,
                rows,
                exact: true,
                max_line_bytes: 4096,
            }),
        }
    }
    #[test]
    fn projection_body_rejects_stale_source_generation_draft_and_closed_views() {
        let mut host = host();
        assert!(!host.receive_viewport(viewport_report(&host, 12, vec![row(b"ERROR original")])));
        assert!(host.receive_viewport(viewport_report(&host, 13, vec![row(b"ERROR original")])));
        assert_eq!(host.projection_rows(Some(SOURCE))[0].bytes, b"ERROR original");
        assert!(host.projection_rows(Some(Source { revision: 99, ..SOURCE })).is_empty());
        assert_eq!(host.navigation(Some(SOURCE), 0), Some(42));
        let delayed = viewport_report(&host, 13, vec![row(b"stale")]);
        host.changed();
        assert!(host.projection_rows(Some(SOURCE)).is_empty());
        assert!(!host.receive_viewport(delayed));
        host.close();
        assert!(host.projection_rows(Some(SOURCE)).is_empty());
    }
    #[test]
    fn projection_pages_reject_mismatched_windows_and_bad_original_coordinates() {
        let mut host = host();
        let mut report = viewport_report(&host, 13, vec![row(b"ERROR original")]);
        if let Ok(page) = &mut report.result {
            page.viewport.horizontal = 10;
        }
        assert!(host.receive_viewport(report));
        assert!(host.projection_rows(Some(SOURCE)).is_empty());
        let host = self::host();
        let mut page = viewport_report(&host, 13, vec![row(b"ERROR original")]).result.unwrap();
        page.rows[0].line_end = page.rows[0].offset;
        assert!(!host.valid_page(&page));
    }
    #[test]
    fn zoomed_out_body_can_show_more_than_a_legacy_hit_page() {
        let mut host = host();
        host.visible = 300;
        host.row_bytes = 1024;
        let rows = (0..300)
            .map(|index| ViewportRow {
                line: 1000 + index,
                line_start: index * 5,
                line_end: index * 5 + 4,
                offset: index * 5,
                bytes: b"INFO".to_vec(),
                spans: Vec::new(),
            })
            .collect();
        assert!(host.receive_viewport(viewport_report(&host, 13, rows)));
        assert_eq!(host.projection_rows(Some(SOURCE)).len(), 300);
        assert_eq!(host.navigation(Some(SOURCE), 299), Some(1300));
    }
    #[test]
    fn row_copy_is_original_utf8_and_refuses_invalid_nul_or_partial_windows() {
        let bytes = "界e\u{301}\\xFF".as_bytes();
        let data = copy_row(&row(bytes)).unwrap();
        let mut copied = Vec::new();
        data.write_to(&mut copied).unwrap();
        assert_eq!(copied, bytes);
        for bytes in [b"bad\xff".as_slice(), b"nul\0".as_slice()] {
            assert!(copy_row(&row(bytes)).is_err());
        }
        let mut partial = row(b"visible suffix");
        partial.line_start = 0;
        assert!(copy_row(&partial).is_err());
        let mut partial = row(b"visible prefix");
        partial.line_end += 1;
        assert!(copy_row(&partial).is_err());
    }
    #[test]
    fn reserved_strip_never_overlaps_source_and_readonly_status_survives_errors() {
        let mut host = host();
        for scale in [0.5, 1.0, 1.25, 2.0, 8.0] {
            for width in [0, 1, 320, 800, 1200, 1600] {
                let source = kwil_view_model::TabArea { x: 4, y: 70, width, height: 700 };
                let areas = layout(source, &Metrics::new(scale), true);
                assert_eq!(areas.body.width, source.width);
                assert_eq!(areas.body.x, source.x);
                if let Some(bar) = areas.bar {
                    assert_eq!(bar.y + bar.h, areas.body.y);
                    assert_eq!(bar.h + areas.body.height, source.height);
                }
                assert_eq!(layout(source, &Metrics::new(scale), false).body, source);
            }
        }
        for error in [None, Some("Quota refused".into())] {
            host.error = error;
            assert!(host.status().contains("read-only"));
        }
        host.body_paint.take();
        host.paint.take();
        host.hover = Some(Target::Apply);
        host.paint.request();
        assert!(!host.body_paint.pending(), "bar hover must not dirty the main text viewport");
    }
    fn host() -> Host {
        let mut host = Host { open: true, ..Host::default() };
        host.sync_source(Some(SOURCE));
        host.accepted(SOURCE, host.serial, 13);
        host
    }
    fn report(source: Source, generation: u64, exact: bool) -> Report {
        Report {
            document: source.document,
            revision: source.revision,
            epoch: source.epoch,
            generation,
            result: Ok(Page {
                hits: vec![Hit {
                    line: 41,
                    offset: 300,
                    metadata: Metadata::default(),
                    preview: "worker preview".into(),
                }],
                total: 1,
                exact,
            }),
        }
    }
    #[test]
    fn level_chips_are_independent_masks_and_clear_keeps_unknown_by_default() {
        let mut host = host();
        assert_eq!(host.levels, all_levels());
        assert!(host.keep_unknown);
        for index in 1..=6 {
            host.toggle_level(index);
            assert_eq!(host.levels & Level::ALL[index - 1].bit(), 0);
        }
        assert_eq!(host.validated().unwrap().levels, 0);
        host.toggle_level(4);
        host.toggle_level(5);
        assert_eq!(host.validated().unwrap().levels, Level::Warn.bit() | Level::Error.bit());
        host.toggle_level(0);
        assert_eq!(host.levels, all_levels());
        host.keep_unknown = false;
        host.clear();
        assert_eq!(host.filter, Filter::default());
        assert!(host.want);
    }
    #[test]
    fn invalid_and_reversed_dates_never_request_submission() {
        let mut host = host();
        for (from, to) in [
            ("2026-02-30 10:00:00", ""),
            ("garbage", ""),
            ("2026-10-24T01:00:00Z", "2026-10-24T00:00:00Z"),
            ("", "2026-10-24T00:00:00"),
        ] {
            host.from.text = from.into();
            host.to.text = to.into();
            host.apply();
            assert!(!host.want && host.expected.is_none() && host.error.is_some());
        }
        host.from.text = "2026-10-24T00:00:00Z".into();
        host.to.text = "2026-10-24 00:00:00".into();
        host.apply();
        assert!(host.want && host.error.is_none());
        assert_eq!(host.filter.from, host.filter.to);
    }
    #[test]
    fn completions_and_clicks_require_source_generation_and_current_draft() {
        let mut host = host();
        for source in
            [Source { document: 6, ..SOURCE }, Source { revision: 8, ..SOURCE }, Source { epoch: 12, ..SOURCE }]
        {
            assert!(!host.receive(report(source, 13, true)));
            assert_eq!(host.navigation(Some(source), 0), None);
        }
        assert!(!host.receive(report(SOURCE, 12, true)));
        assert!(host.receive(report(SOURCE, 13, true)));
        assert_eq!(host.navigation(Some(SOURCE), 0), Some(42));
        assert_eq!(host.navigation(Some(SOURCE), 1), None);
        assert!(!host.receive(report(SOURCE, 13, false)), "late progress cannot replace exact results");
        host.changed();
        assert_eq!(host.navigation(Some(SOURCE), 0), None);
        assert!(!host.receive(report(SOURCE, 13, true)));
        host.close();
        assert!(!host.receive(report(SOURCE, 13, true)));
    }
    #[test]
    fn source_sync_clears_hits_before_requery_and_invalidates_old_geometry() {
        let mut host = host();
        host.receive(report(SOURCE, 13, true));
        host.geometry_epoch = host.input_epoch;
        let old = host.input_epoch;
        host.sync_source(Some(Source { epoch: 99, ..SOURCE }));
        assert!(host.hits.is_empty() && host.expected.is_none());
        assert!(host.input_epoch > old && host.want);
        assert!(host.navigation(Some(SOURCE), 0).is_none());
        assert!(!host.receive(report(SOURCE, 13, true)));
    }
    #[test]
    fn unicode_fields_edit_graphemes_and_ime_tokens_are_typed_and_single_use() {
        let mut host = host();
        let token = host.destination().unwrap();
        assert!(host.commit(token, "界e\u{301}👩\u{200d}💻"));
        assert!(!host.commit(token, "duplicate"));
        assert!(host.from.key(NamedKey::Backspace));
        assert_eq!(host.from.text, "界e\u{301}");
        host.from.key(NamedKey::ArrowLeft);
        assert_eq!(host.from.cursor, "界".len());
        assert!(host.from.key(NamedKey::Delete));
        assert_eq!(host.from.text, "界");
        host.from.all = true;
        assert!(host.from.insert("日本語").unwrap());
        assert_eq!(host.from.text, "日本語");
        let token = host.destination().unwrap();
        host.focus(Target::Field(FieldId::To));
        assert!(!host.commit(token, "wrong field"));
        assert!(host.to.text.is_empty());
        let token = host.destination().unwrap();
        host.sync_source(Some(Source { revision: 8, ..SOURCE }));
        assert!(!host.commit(token, "stale source"));
        assert!(host.to.text.is_empty());
    }
    #[test]
    fn input_limit_rejects_whole_commit_and_controls_without_splitting_unicode() {
        let mut edit = Edit::default();
        edit.insert("界").unwrap();
        assert!(edit.insert(&"x".repeat(INPUT_BYTES)).is_err());
        assert!(edit.insert("\n").is_err());
        assert_eq!(edit.text, "界");
        edit.all = true;
        edit.insert("replacement").unwrap();
        assert_eq!(edit.text, "replacement");
    }
    #[test]
    fn deletion_that_joins_graphemes_keeps_a_valid_caret_boundary() {
        let mut edit = Edit::default();
        edit.insert("🇯 🇵").unwrap();
        edit.key(NamedKey::ArrowLeft);
        assert!(edit.key(NamedKey::Backspace));
        assert_eq!(edit.text, "🇯🇵");
        assert_eq!(edit.cursor, edit.text.len());
        edit.key(NamedKey::Backspace);
        assert!(edit.text.is_empty());
    }
    #[test]
    fn retired_retry_cannot_arm_a_new_draft_source_or_closed_card() {
        let mut host = host();
        let old_serial = host.serial;
        host.changed();
        host.accepted(SOURCE, old_serial, 14);
        assert!(host.expected.is_none());
        host.apply();
        host.accepted(Source { epoch: 12, ..SOURCE }, host.serial, 15);
        assert!(host.expected.is_none());
        host.close();
        host.accepted(SOURCE, host.serial, 16);
        assert!(host.expected.is_none());
    }
    #[test]
    fn tiny_and_scaled_geometry_is_bounded_and_surface_changes_invalidate_clicks() {
        let mut host = host();
        for scale in [0.5, 1.0, 1.25, 2.0, 8.0] {
            let m = Metrics::new(scale);
            for size in [(0, 0), (1, 1), (140, 80), (320, 480), (1280, 900)] {
                if let Some(r) = card_area(size, &m, 0) {
                    assert!(r.x + r.w <= size.0 && r.y + r.h <= size.1);
                    host.area = Some(r);
                    host.surface = Surface::new(size, &m);
                    host.geometry_epoch = host.input_epoch;
                    assert!(host.geometry_matches(size, &m));
                    assert!(!host.geometry_matches((size.0 + 1, size.1), &m));
                    host.bump();
                    assert!(!host.geometry_matches(size, &m));
                }
            }
        }
        assert!(card_area((0, 0), &Metrics::default(), usize::MAX).is_none());
    }
    #[test]
    fn result_pages_are_capped_and_page_buttons_use_total_not_visible_rows() {
        let mut host = host();
        let mut invalid = report(SOURCE, 13, true);
        if let Ok(page) = &mut invalid.result {
            page.hits[0].preview = "x".repeat(PREVIEW_BYTES + 1);
        }
        assert!(host.receive(invalid));
        assert!(host.hits.is_empty() && host.navigation(Some(SOURCE), 0).is_none());
        let mut host = self::host();
        let mut page = report(SOURCE, 13, false);
        if let Ok(page) = &mut page.result {
            page.total = 257;
        }
        host.receive(page);
        assert!(host.can_next());
        host.page(true);
        assert_eq!(host.page_start, 256);
        assert!(host.want && host.hits.is_empty());
        host.page(false);
        assert_eq!(host.page_start, 0);
    }
    #[test]
    fn utc_format_preserves_fraction_offsets_and_pre_epoch_times() {
        for text in ["1970-01-01T00:00:00Z", "1969-12-31T23:59:59.999999999Z", "2024-02-29T12:34:56.123Z"] {
            let stamp = parse_timestamp(text).unwrap();
            let formatted = timestamp_label(stamp);
            assert_eq!(parse_timestamp(&formatted), Some(stamp));
        }
        assert_eq!(
            timestamp_label(parse_timestamp("2026-10-24T09:00:00+09:00").unwrap()),
            "2026-10-24 00:00:00.000000000Z"
        );
        assert!(timestamp_label(Timestamp(i128::MAX)).contains("ns since Unix epoch"));
    }
}
