//! Terminal content views. Source reads and releases stay on workers.
use super::*;
use kwil_core_markdown::{BlockKind, Model};
use kwil_core_runtime::{NativeExitObservation, NativeExitState};
use kwil_view_model::log_session::{Filter, Level, LogSession, TextQuery, Viewport, ViewportPage};
use std::{sync::Arc, time::Instant};

type Source = (u64, u64, u64);
type MarkdownRequest = (Source, u64, u64, kwil_core_text::Snapshot);
fn source(view: &Workspace) -> Option<Source> {
    if view.document_busy(view.active_id()) {
        return None;
    }
    Some((view.active_id().0, view.editor()?.document().revision(), view.input_epoch()))
}
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Mode {
    #[default]
    Text,
    Preview,
    Split,
}
struct MarkdownWorker {
    send: Option<mpsc::SyncSender<MarkdownRequest>>,
    receive: mpsc::Receiver<(Source, u64, Result<Model, String>)>,
    thread: Option<std::thread::JoinHandle<()>>,
    native: Arc<NativeExitObservation>,
}
impl MarkdownWorker {
    fn start(wake: Arc<dyn Fn() + Send + Sync>) -> std::io::Result<Self> {
        let (send, requests) = mpsc::sync_channel::<MarkdownRequest>(1);
        let (results, receive) = mpsc::sync_channel(1);
        let native = Arc::new(NativeExitObservation::default());
        let observation = native.clone();
        let thread = std::thread::Builder::new().name("kwil-tui-markdown".into()).spawn(move || {
            let registration = observation.register_current();
            let mut pager = None;
            let mut pager_source = None;
            while let Ok((key, top, incarnation, snapshot)) = requests.recv() {
                if pager_source != Some((key.0, key.1, incarnation)) {
                    pager = None;
                    pager_source = Some((key.0, key.1, incarnation));
                }
                let result = if registration.is_err() {
                    Err("Markdown worker registration failed".into())
                } else {
                    parse_page(&snapshot, top, &mut pager)
                };
                drop(snapshot);
                if results.try_send((key, top, result)).is_err() {
                    break;
                }
                wake();
            }
        })?;
        Ok(Self { send: Some(send), receive, thread: Some(thread), native })
    }
    fn settle(&mut self, deadline: Instant) -> Result<(), String> {
        self.send.take();
        if let Some(thread) = &self.thread {
            match self.native.wait_until(thread, deadline).map_err(|e| format!("Markdown exit: {e:?}"))? {
                NativeExitState::Pending => return Err("Markdown exit pending".into()),
                NativeExitState::Exited => {}
            }
            if Instant::now() >= deadline {
                return Err("Markdown exit deadline".into());
            }
            self.thread.take().unwrap().join().map_err(|_| "Markdown worker panicked")?;
        }
        self.native.retire_after_join().map_err(|e| format!("Markdown retirement: {e:?}"))
    }
}
fn parse(snapshot: &kwil_core_text::Snapshot) -> Result<Model, String> {
    if snapshot.len() > kwil_core_markdown::bounded::PREVIEW_SOURCE_BYTES as u64 {
        return Err("Markdown preview limit is 256 KiB; use Text mode".into());
    }
    let mut bytes = Vec::with_capacity(snapshot.len() as usize);
    for chunk in snapshot.chunks(0..snapshot.len()).map_err(|e| e.to_string())? {
        bytes.extend_from_slice(&chunk.map_err(|e| e.to_string())?);
    }
    let text = std::str::from_utf8(&bytes).map_err(|e| e.to_string())?;
    kwil_core_markdown::bounded::preview_model(text, &|| false).map_err(|e| format!("Markdown preview: {e:?}"))
}
fn parse_page(
    snapshot: &kwil_core_text::Snapshot,
    top: u64,
    pager: &mut Option<kwil_core_markdown::paged::Pager>,
) -> Result<Model, String> {
    if snapshot.len() <= kwil_core_markdown::bounded::PREVIEW_SOURCE_BYTES as u64 {
        return parse(snapshot);
    }
    let mut read = |range: std::ops::Range<u64>, out: &mut Vec<u8>| {
        out.clear();
        for chunk in snapshot.chunks(range).map_err(|e| e.to_string())? {
            out.extend_from_slice(&chunk.map_err(|e| e.to_string())?);
        }
        Ok(())
    };
    if pager.is_none() {
        *pager = Some(kwil_core_markdown::paged::Pager::build(snapshot.len(), &mut read, &|| false)?);
    }
    pager.as_mut().unwrap().page(top, 64, &mut read, &|| false).map(|page| page.model)
}
#[derive(Default)]
pub(super) struct Host {
    mode: Mode,
    document: Option<u64>,
    markdown: Option<MarkdownWorker>,
    in_flight: bool,
    markdown_retry: Option<MarkdownRequest>,
    large_markdown: bool,
    model_top: Option<u64>,
    rendered_top: usize,
    block_scroll: Option<(usize, u64, u64)>,
    scroll_anchor: Option<u64>,
    model: Option<Model>,
    model_source: Option<Source>,
    markdown_top: usize,
    rendered: Vec<Line<'static>>,
    rendered_width: usize,
    error: String,
    log_settings: kwil_core_commands::settings::LogSettings,
    logs: Option<LogSession>,
    open: bool,
    filter: Filter,
    query: String,
    regex: bool,
    from: String,
    to: String,
    field: Option<usize>,
    level: usize,
    page: Option<ViewportPage>,
    submitted: Option<(Source, Filter, Viewport)>,
    generation: u64,
    retry: Option<(Source, kwil_core_text::Snapshot)>,
    top: u64,
    horizontal: u64,
    following: bool,
    reload: bool,
    watch: Option<kwil_view_model::log_follow::FollowWatch>,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
}
impl Host {
    pub fn configure(&mut self, settings: &kwil_core_commands::settings::LogSettings) {
        if self.log_settings != *settings {
            self.log_settings = settings.clone();
            if let Some(session) = &self.logs {
                session.configure(settings);
            }
            self.submitted = None;
        }
    }
    pub fn start(&mut self, wake: Arc<dyn Fn() + Send + Sync>) {
        self.wake = Some(wake);
    }
    pub fn command(&mut self, view: &mut Workspace, command: &str) {
        self.document = Some(view.active_id().0);
        self.error.clear();
        if command.starts_with("view.markdown") || command == "view.toggleMarkdown" {
            self.open = false;
            self.field = None;
            self.following = false;
            let mode = match command {
                "view.markdownText" => Mode::Text,
                "view.markdownPreview" => Mode::Preview,
                "view.markdownSplit" => Mode::Split,
                _ => match self.mode {
                    Mode::Text => Mode::Split,
                    Mode::Split => Mode::Preview,
                    Mode::Preview => Mode::Text,
                },
            };
            if mode != Mode::Text && !view.editor().is_some_and(|e| e.is_markdown()) {
                view.set_notice("Open a Markdown file first".into());
            } else {
                self.mode = mode;
                self.markdown_top = 0;
            }
            return;
        }
        if !view.is_log_document() {
            view.set_notice("Open a log file first".into());
            return;
        }
        self.mode = Mode::Text;
        match command {
            "log.pause" => {
                self.following = false;
                view.set_notice("Log follow paused".into());
            }
            "log.toggleFollow" if self.following => {
                self.following = false;
            }
            "log.resume" | "log.toggleFollow" => self.resume(view),
            "view.logFilters" => {
                self.open = !self.open;
                self.field = self.open.then_some(0);
            }
            "log.filter" => {
                self.open = true;
                self.field = Some(0);
            }
            "log.goToTime" => {
                self.open = true;
                self.field = Some(1);
            }
            "log.toggleRegex" => {
                self.open = true;
                self.regex = !self.regex;
                self.apply();
            }
            "log.toggleLevel" => {
                self.open = true;
                self.filter.levels ^= Level::ALL[self.level].bit();
                self.invalidate();
            }
            "log.clearFilter" => {
                self.filter = Filter::default();
                self.query.clear();
                self.from.clear();
                self.to.clear();
                self.regex = false;
                self.field = None;
                self.open = true;
                self.invalidate();
            }
            _ => {}
        }
        if !self.error.is_empty() {
            view.set_notice(self.error.clone());
        }
    }
    fn resume(&mut self, view: &Workspace) {
        let Some(editor) = view.editor() else {
            return;
        };
        if editor.document().is_dirty() {
            self.error = "Save log edits before following".into();
            return;
        }
        let Some(path) = editor.document().path() else {
            return;
        };
        let result = if let Some(watch) = &self.watch {
            watch.watch(path.to_owned())
        } else {
            let wake = self.wake.clone().unwrap();
            kwil_view_model::log_follow::FollowWatch::start(path.to_owned(), move || wake())
                .map(|watch| self.watch = Some(watch))
        };
        match result {
            Ok(()) => {
                self.following = true;
                self.reload = true;
                self.open = true;
            }
            Err(e) => self.error = e.to_string(),
        }
    }
    fn invalidate(&mut self) {
        self.submitted = None;
        self.page = None;
        self.top = 0;
    }
    fn apply(&mut self) {
        let result = (|| {
            let parse = |text: &str| {
                if text.is_empty() {
                    Ok(None)
                } else {
                    kwil_core_syntax::logs::parse_timestamp(text)
                        .map(Some)
                        .ok_or("Use an ISO timestamp, e.g. 2026-11-02T10:00:00Z")
                }
            };
            let from = parse(&self.from)?;
            let to = parse(&self.to)?;
            if from.zip(to).is_some_and(|(a, b)| a > b) {
                return Err("Time range end precedes start");
            }
            Ok(Filter {
                text: TextQuery::new(&self.query, self.regex)?,
                from,
                to,
                keep_unknown: from.is_none() && to.is_none(),
                ..self.filter
            })
        })();
        match result {
            Ok(filter) => {
                self.filter = filter;
                self.error.clear();
                self.invalidate();
                self.field = None;
            }
            Err(e) => self.error = e.into(),
        }
    }
    pub fn key(&mut self, view: &mut Workspace, key: KeyEvent) -> bool {
        if self.document != Some(view.active_id().0) {
            return false;
        }
        if let Some(field) = self.field {
            match key.code {
                KeyCode::Esc => {
                    self.field = None;
                    self.error.clear();
                }
                KeyCode::Enter => self.apply(),
                KeyCode::Tab => self.field = Some((field + 1) % 3),
                KeyCode::BackTab => self.field = Some((field + 2) % 3),
                KeyCode::Backspace => {
                    self.edit(field).pop();
                }
                KeyCode::Char(c)
                    if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) && !c.is_control() =>
                {
                    let text = self.edit(field);
                    if text.len() + c.len_utf8() <= 256 {
                        text.push(c);
                    }
                }
                _ => {}
            }
            return true;
        }
        if self.mode != Mode::Preview && !self.open {
            return false;
        }
        let delta = match key.code {
            KeyCode::Down => 1,
            KeyCode::Up => -1,
            KeyCode::PageDown => view.rows() as i64,
            KeyCode::PageUp => -(view.rows() as i64),
            _ => 0,
        };
        if self.large_markdown && self.mode != Mode::Text {
            match key.code {
                KeyCode::Home => {
                    view.scroll_to_start();
                    return true;
                }
                KeyCode::End => {
                    view.scroll_to_end();
                    return true;
                }
                _ if delta != 0 => {
                    self.scroll_preview(view, delta, None);
                    return true;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Text;
                self.open = false;
                self.following = false;
            }
            KeyCode::Home => {
                self.top = 0;
                self.markdown_top = 0;
                self.following = false;
            }
            KeyCode::End => {
                self.markdown_top = usize::MAX;
                if let Some(page) = &self.page {
                    self.top = page.total.saturating_sub(view.rows().max(1) as u64);
                }
            }
            KeyCode::Left if self.open => {
                self.horizontal = self.horizontal.saturating_sub(8);
            }
            KeyCode::Right if self.open => {
                self.horizontal = self.horizontal.saturating_add(8);
            }
            KeyCode::Enter if self.open => {
                if let Some(row) = self.page.as_ref().and_then(|p| p.rows.first()) {
                    let line = row.line + 1;
                    self.open = false;
                    self.following = false;
                    if let Err(e) = view.execute(
                        "navigation.goToLine",
                        &CommandArgs { line: Some(line), column: Some(1), ..Default::default() },
                        1,
                    ) {
                        view.set_notice(e.to_string());
                    }
                }
            }
            KeyCode::Char('f') if self.open => self.field = Some(0),
            KeyCode::Char('t') if self.open => self.field = Some(1),
            KeyCode::Char('r') if self.open => {
                self.regex = !self.regex;
                self.apply();
            }
            KeyCode::Char(c @ '1'..='6') if self.open => {
                self.level = c as usize - '1' as usize;
                self.filter.levels ^= Level::ALL[self.level].bit();
                self.invalidate();
            }
            _ if delta != 0 => {
                self.following = false;
                self.top = self.top.saturating_add_signed(delta);
                if let Some(page) = &self.page {
                    self.top = self.top.min(page.total.saturating_sub(1));
                }
                self.markdown_top = self.markdown_top.saturating_add_signed(delta as isize);
            }
            _ => return false,
        }
        true
    }
    fn edit(&mut self, field: usize) -> &mut String {
        match field {
            1 => &mut self.from,
            2 => &mut self.to,
            _ => &mut self.query,
        }
    }
    pub fn captures_body(&self, view: &Workspace) -> bool {
        self.document == Some(view.active_id().0)
            && !view.compare.open
            && !view.hex_active()
            && (self.open || self.mode == Mode::Preview)
    }
    pub fn paste(&mut self, text: &str) {
        if let Some(field) = self.field {
            let value = self.edit(field);
            if !text.chars().any(char::is_control) && value.len() + text.len() <= 256 {
                value.push_str(text);
            }
        }
    }
    pub fn mouse(&mut self, view: &mut Workspace, event: &event::MouseEvent) -> bool {
        let code = match event.kind {
            MouseEventKind::ScrollUp => KeyCode::Up,
            MouseEventKind::ScrollDown => KeyCode::Down,
            _ => return true,
        };
        if self.large_markdown && self.mode != Mode::Text {
            self.scroll_preview(view, if code == KeyCode::Up { -1 } else { 1 }, None);
        } else if self.mode == Mode::Split {
            self.markdown_top = if code == KeyCode::Up {
                self.markdown_top.saturating_sub(1)
            } else {
                self.markdown_top.saturating_add(1)
            };
        } else {
            self.key(view, KeyEvent::new(code, KeyModifiers::NONE));
        }
        true
    }
    pub fn editing(&self) -> bool {
        self.field.is_some()
    }
    pub fn prepared_key(
        &mut self,
        view: &mut Workspace,
        key: KeyEvent,
        runtime: Option<&mut wrap_ui::Runtime>,
    ) -> bool {
        if !self.large_markdown
            || self.mode != Mode::Preview
            || self.editing()
            || view.body_policy() != kwil_view_model::ViewBodyPolicy::Prepared
        {
            return false;
        }
        let Some(runtime) = runtime else {
            return false;
        };
        let result = match key.code {
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                let delta = match key.code {
                    KeyCode::Up => -1,
                    KeyCode::Down => 1,
                    KeyCode::PageUp => -(view.rows() as i64),
                    _ => view.rows() as i64,
                };
                self.scroll_preview(view, delta, Some(runtime));
                return true;
            }
            KeyCode::Home => runtime.reveal_source(0),
            KeyCode::End => runtime.reveal_source(view.editor().map_or(0, |e| e.document().buffer().len())),
            _ => return false,
        };
        if let Some(notice) = result.notice() {
            view.set_notice(notice.into());
        }
        true
    }
    pub fn prepared_mouse(
        &mut self,
        view: &mut Workspace,
        event: &event::MouseEvent,
        runtime: Option<&mut wrap_ui::Runtime>,
    ) -> bool {
        if !self.large_markdown
            || self.mode == Mode::Text
            || view.body_policy() != kwil_view_model::ViewBodyPolicy::Prepared
        {
            return false;
        }
        let Some(runtime) = runtime else {
            return false;
        };
        let delta = match event.kind {
            MouseEventKind::ScrollUp => -3,
            MouseEventKind::ScrollDown => 3,
            _ => return false,
        };
        self.scroll_preview(view, delta, Some(runtime));
        true
    }
    fn scroll_preview(&mut self, view: &mut Workspace, delta: i64, runtime: Option<&mut wrap_ui::Runtime>) {
        let offset = self.markdown_top.saturating_add_signed(delta as isize);
        if (delta < 0 && delta.unsigned_abs() as usize <= self.markdown_top)
            || (delta >= 0 && self.block_scroll.is_some_and(|b| offset < b.0))
        {
            self.markdown_top = offset;
        } else if delta > 0
            && let Some((height, line, byte)) = self.block_scroll
        {
            self.scroll_anchor = Some(line);
            if let Some(runtime) = runtime {
                let result = runtime.reveal_source(byte);
                if let Some(notice) = result.notice() {
                    view.set_notice(notice.into());
                    return;
                }
                self.markdown_top = offset.saturating_sub(height);
            } else {
                let old = view.top();
                view.scroll(line.saturating_sub(old) as i64);
                self.scroll_anchor = Some(view.top());
                self.markdown_top = if old == view.top() { offset } else { offset.saturating_sub(height) };
            }
        } else {
            if let Some(runtime) = runtime {
                runtime.scroll_rows(-1);
            } else {
                view.scroll(-1);
            }
            self.markdown_top = 0;
        }
    }
    pub fn mark_line(&self, view: &Workspace) -> Option<u64> {
        if !self.submitted.is_some_and(|(key, _, _)| Some(key) == source(view)) {
            return None;
        }
        if !self.open {
            return None;
        }
        self.page.as_ref()?.rows.first().map(|row| row.line + 1)
    }
    pub fn close(&mut self) {
        self.mode = Mode::Text;
        self.close_logs();
    }
    pub fn source_body(&self, mut area: Rect, view: &Workspace) -> Rect {
        if self.mode == Mode::Split && self.document == Some(view.active_id().0) {
            area.width = (area.width / 2).max(1);
        }
        area
    }
    pub fn preview_point(&self, view: &Workspace, event: &event::MouseEvent, area: Rect) -> bool {
        self.mode == Mode::Split
            && self.document == Some(view.active_id().0)
            && event.column >= area.x + area.width / 2
            && area.contains((event.column, event.row).into())
    }
    pub fn close_logs(&mut self) {
        self.open = false;
        self.field = None;
        self.following = false;
    }
    pub fn sync(&mut self, view: &mut Workspace, rows: usize) {
        if view.hex_active() || view.compare.open {
            self.mode = Mode::Text;
            self.close_logs();
        }
        if self.document != Some(view.active_id().0) {
            self.mode = Mode::Text;
            self.close_logs();
            self.model = None;
            self.model_source = None;
        }
        if let Some(watch) = &self.watch {
            if watch.take_changed() && self.following {
                self.reload = true;
            }
            if let Some(error) = watch.take_error() {
                self.following = false;
                self.error = error;
            }
        }
        if self.following && self.reload {
            match view.reload_log() {
                Ok(()) => self.reload = false,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    self.following = false;
                    self.error = e.to_string();
                }
            }
        }
        let current = source(view);
        self.large_markdown = view
            .editor()
            .is_some_and(|e| e.document().buffer().len() > kwil_core_markdown::bounded::PREVIEW_SOURCE_BYTES as u64);
        let markdown_top = if self.large_markdown { view.top() } else { 0 };
        if self.large_markdown && self.model_top != Some(markdown_top) && self.scroll_anchor != Some(markdown_top) {
            self.markdown_top = 0;
        }
        if let Some(worker) = &self.markdown {
            while let Ok((key, top, result)) = worker.receive.try_recv() {
                self.in_flight = false;
                if current == Some(key) && top == markdown_top && self.mode != Mode::Text {
                    self.model_source = Some(key);
                    self.model_top = Some(top);
                    match result {
                        Ok(model) => {
                            self.model = Some(model);
                            self.rendered_width = 0;
                            self.error.clear();
                        }
                        Err(e) => {
                            self.model = None;
                            self.error = e;
                        }
                    }
                }
            }
        }
        if self.mode != Mode::Text
            && (current != self.model_source || self.model_top != Some(markdown_top))
            && !self.in_flight
        {
            if self.markdown.is_none() {
                match MarkdownWorker::start(self.wake.clone().unwrap()) {
                    Ok(worker) => self.markdown = Some(worker),
                    Err(e) => self.error = e.to_string(),
                }
            }
            if let (Some(worker), Some(key), Some(editor)) = (&self.markdown, current, view.editor()) {
                let request = self.markdown_retry.take().unwrap_or_else(|| {
                    (key, markdown_top, editor.document().hex_source(key.0).epoch, editor.document().snapshot())
                });
                match worker.send.as_ref().unwrap().try_send(request) {
                    Ok(()) => self.in_flight = true,
                    Err(e) => {
                        self.markdown_retry = Some(match e {
                            mpsc::TrySendError::Full(work) | mpsc::TrySendError::Disconnected(work) => work,
                        });
                        self.error = "Markdown worker unavailable".into();
                    }
                }
            }
        }
        if self.logs.is_none() && self.open {
            let wake = self.wake.clone().unwrap();
            // Same disk admission as GUI log projection and SearchWorker.
            match LogSession::start_with_index_limit(1 << 30, move || wake()) {
                Ok(session) => {
                    session.configure(&self.log_settings);
                    self.logs = Some(session);
                }
                Err(e) => self.error = e.to_string(),
            }
        }
        let Some(session) = &mut self.logs else {
            return;
        };
        for report in session.drain_viewport() {
            if self.open
                && current == Some((report.document, report.revision, report.epoch))
                && report.generation == self.generation
                && self.submitted.is_some()
            {
                match report.result {
                    Ok(page) => {
                        if self.following && page.exact {
                            self.top = page.total.saturating_sub(rows.max(1) as u64);
                        }
                        self.page = Some(page);
                        self.error.clear();
                    }
                    Err(e) => self.error = e,
                }
            }
        }
        // Always retire a rejected stale snapshot before capturing another source.
        if let Some((key, snapshot)) = self.retry.take() {
            if current != Some(key) || !self.open {
                if let Err(e) = session.retire_snapshot(snapshot) {
                    self.retry = Some((key, e.snapshot));
                }
                return;
            }
            self.retry = Some((key, snapshot));
        }
        if !self.open {
            session.cancel_viewport();
            return;
        }
        let Some(key) = current else {
            self.page = None;
            return;
        };
        let rows = rows.clamp(1, kwil_view_model::log_session::VIEWPORT_ROWS);
        let viewport = Viewport {
            top: self.top,
            rows,
            horizontal: self.horizontal,
            row_bytes: (kwil_view_model::log_session::VIEWPORT_BYTES / rows)
                .min(kwil_view_model::log_session::ROW_BYTES),
        };
        if self.submitted != Some((key, self.filter, viewport)) {
            let snapshot =
                self.retry.take().map(|(_, s)| s).unwrap_or_else(|| view.editor().unwrap().document().snapshot());
            match session.submit_viewport(key.0, key.1, key.2, snapshot, self.filter, viewport) {
                Ok(generation) => {
                    self.generation = generation;
                    self.submitted = Some((key, self.filter, viewport));
                    self.page = None;
                }
                Err(e) => {
                    self.retry = Some((key, e.snapshot));
                    self.error = e.reason.into();
                }
            }
        }
    }
    pub fn draw(&mut self, frame: &mut ratatui::Frame, view: &Workspace, area: Rect) {
        if self.document != Some(view.active_id().0) || view.hex_active() || view.compare.open {
            return;
        }
        if self.mode != Mode::Text {
            let mut preview = area;
            if self.mode == Mode::Split {
                let left = Rect { width: area.width / 2, ..area };
                preview.x += left.width;
                preview.width -= left.width;
            }
            if self.rendered_width != preview.width as usize {
                self.rendered.clear();
                self.rendered_top = 0;
                self.block_scroll = None;
                if let Some(model) = &self.model {
                    let top = self.model_top.unwrap_or(0) as usize;
                    for block in &model.blocks {
                        if self.large_markdown && block.source_lines.end <= top {
                            continue;
                        }
                        let single = Model { blocks: vec![block.clone()], source_bytes: model.source_bytes.clone() };
                        let lines = markdown_lines(&single, preview.width as usize);
                        if self.large_markdown && block.source_lines.contains(&top) {
                            self.rendered_top = lines.len().saturating_mul(top - block.source_lines.start)
                                / block.source_lines.len().max(1);
                        }
                        if self.large_markdown && self.block_scroll.is_none() {
                            self.block_scroll = Some((
                                lines.len().saturating_sub(self.rendered_top),
                                block.source_lines.end as u64,
                                block.source_bytes.end as u64,
                            ));
                        }
                        self.rendered.extend(lines);
                    }
                }
                self.rendered_width = preview.width as usize;
            }
            self.markdown_top = self.markdown_top.min(self.rendered.len().saturating_sub(preview.height as usize));
            let text = if !self.error.is_empty() {
                vec![Line::from(self.error.clone())]
            } else if self.model_source != source(view) {
                vec![Line::from("Preparing Markdown preview...")]
            } else {
                self.rendered
                    .iter()
                    .skip(if self.large_markdown {
                        self.rendered_top.saturating_add(self.markdown_top)
                    } else {
                        self.markdown_top
                    })
                    .take(preview.height as usize)
                    .cloned()
                    .collect()
            };
            frame.render_widget(Paragraph::new(text).style(theme_ui::base()), preview);
        }
        if !self.open {
            return;
        }
        frame.render_widget(Paragraph::new("").style(theme_ui::base()), area);
        let levels = Level::ALL
            .iter()
            .enumerate()
            .map(|(i, l)| {
                format!("{}{}{}", i + 1, &l.label()[..1], if self.filter.levels & l.bit() != 0 { "+" } else { "-" })
            })
            .collect::<Vec<_>>()
            .join(" ");
        let label = if let Some(field) = self.field {
            format!("{}: {} | Tab: next Enter: apply Esc: cancel", ["Filter", "From", "To"][field], self.edit(field))
        } else {
            format!(
                "{}{} rows | {} | f:filter t:time r:regex={} | {} | Esc:text",
                self.page.as_ref().map_or(0, |page| page.total),
                if self.page.as_ref().is_some_and(|page| page.exact) { "" } else { "+" },
                levels,
                self.regex,
                if self.following { "Following" } else { "Paused" }
            )
        };
        frame.render_widget(Paragraph::new(label).style(theme_ui::base()), Rect { height: 1, ..area });
        let body = Rect { y: area.y.saturating_add(1), height: area.height.saturating_sub(1), ..area };
        if !self.error.is_empty() {
            frame.render_widget(Paragraph::new(self.error.clone()), body);
        } else if self.submitted.is_some_and(|(key, filter, _)| Some(key) == source(view) && filter == self.filter)
            && let Some(page) = &self.page
        {
            let lines = page
                .rows
                .iter()
                .take(body.height as usize)
                .map(|row| {
                    let mut spans = vec![Span::raw(format!(
                        "{:>7}{} ",
                        row.line + 1,
                        if view.log_marked(row.line + 1) { '*' } else { ' ' }
                    ))];
                    for cell in kwil_view_model::grid::cells(&row.bytes, body.width.saturating_sub(9) as usize, 4) {
                        let offset = row.offset + cell.bytes.start as u64;
                        let index = row.spans.partition_point(|span| span.range.end as u64 <= offset);
                        let style = row.spans.get(index).filter(|span| span.range.start as u64 <= offset).map_or(
                            theme_ui::base(),
                            |span| {
                                let rgb = theme_ui::syntax(span.class);
                                theme_ui::base().fg(Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8))
                            },
                        );
                        spans.push(Span::styled(
                            if cell.text == "\t" { " ".repeat(cell.width) } else { cell.text },
                            row.color.map_or(style, |rgb| {
                                theme_ui::base().fg(Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8))
                            }),
                        ));
                    }
                    Line::from(spans)
                })
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(lines).style(theme_ui::base()), body);
        }
    }
    /// Existing off-UI executor retains this host on any refusal.
    pub fn settle(&mut self, deadline: Instant) -> Result<(), String> {
        if let Some(watch) = &mut self.watch {
            watch.stop();
        }
        if let Some(worker) = &mut self.markdown {
            worker.settle(deadline)?;
        }
        if let Some(session) = &mut self.logs {
            session.settle_until(deadline).map_err(|e| format!("Log retirement: {e:?}"))?;
        }
        self.retry.take();
        self.markdown_retry.take();
        Ok(())
    }
}
fn markdown_lines(model: &Model, width: usize) -> Vec<Line<'static>> {
    let mut result = Vec::new();
    for block in &model.blocks {
        if let BlockKind::Table { rows, alignments } = &block.kind {
            let cells: Vec<Vec<String>> = rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|spans| {
                            let model = Model {
                                blocks: vec![kwil_core_markdown::Block {
                                    kind: BlockKind::Paragraph,
                                    spans: spans.clone(),
                                    source_lines: block.source_lines.clone(),
                                    source_bytes: block.source_bytes.clone(),
                                    quote_depth: 0,
                                    list_depth: 0,
                                }],
                                source_bytes: 0..0,
                            };
                            styled_block(&model.blocks[0], usize::MAX / 2)
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                        .collect()
                })
                .collect();
            let widths = (0..alignments.len())
                .map(|i| {
                    cells
                        .iter()
                        .filter_map(|r| r.get(i))
                        .map(|s| Line::from(s.as_str()).width())
                        .max()
                        .unwrap_or(0)
                        .min(width.saturating_sub(alignments.len() * 3 + 1) / alignments.len().max(1))
                        .max(1)
                })
                .collect::<Vec<_>>();
            for row in cells {
                let wrapped = row.iter().enumerate().map(|(i, cell)| wrap_cell(cell, widths[i])).collect::<Vec<_>>();
                for subrow in 0..wrapped.iter().map(Vec::len).max().unwrap_or(0) {
                    let mut text = String::from("| ");
                    for (i, cell) in wrapped.iter().enumerate() {
                        let cell = cell.get(subrow).map(String::as_str).unwrap_or("");
                        text.push_str(cell);
                        text.push_str(&" ".repeat(widths[i].saturating_sub(Line::from(cell).width())));
                        text.push_str(" | ");
                    }
                    result.push(Line::from(text));
                }
            }
        } else if !block.spans.is_empty() {
            result.extend(styled_block(block, width));
        } else {
            let single = Model { blocks: vec![block.clone()], source_bytes: model.source_bytes.clone() };
            let style = if matches!(block.kind, BlockKind::Heading(_)) {
                theme_ui::base().add_modifier(ratatui::style::Modifier::BOLD)
            } else {
                theme_ui::base()
            };
            result.extend(
                kwil_core_markdown::render_text(&single, width)
                    .lines()
                    .map(|line| Line::styled(line.to_owned(), style)),
            );
        }
        result.push(Line::default());
    }
    result
}

fn wrap_cell(text: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    let mut used = 0;
    for cell in kwil_view_model::grid::cells(text.as_bytes(), text.len().max(1), 4) {
        if used > 0 && used + cell.width > width {
            lines.push(String::new());
            used = 0;
        }
        used += cell.width;
        lines.last_mut().unwrap().push_str(&cell.text);
    }
    lines
}

fn styled_block(block: &kwil_core_markdown::Block, width: usize) -> Vec<Line<'static>> {
    use kwil_core_markdown::InlineKind;
    use ratatui::style::Modifier;
    let width = width.max(1);
    let mut empty = block.clone();
    empty.spans.clear();
    let prefix = kwil_core_markdown::render_text(&Model { blocks: vec![empty], source_bytes: 0..0 }, width);
    let mut line = Line::from(prefix.trim_end().to_owned());
    if line.width() > 0 {
        line.push_span(" ");
    }
    let mut lines = Vec::new();
    for span in &block.spans {
        let mut style = theme_ui::base();
        if span.style.strong || matches!(block.kind, BlockKind::Heading(_)) {
            style = style.add_modifier(Modifier::BOLD);
        }
        if span.style.emphasis {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if span.style.strikethrough {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if span.style.link.is_some() {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        let mut text = match &span.kind {
            InlineKind::Text(text) | InlineKind::Code(text) => text.clone(),
            InlineKind::SoftBreak => " ".into(),
            InlineKind::HardBreak => "\n".into(),
            InlineKind::Image { alt, url } => format!("[image: {alt}] ({url})"),
        };
        if let Some(url) = &span.style.link {
            text.push_str(&format!(" ({url})"));
        }
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                lines.push(std::mem::take(&mut line));
            }
            for cell in kwil_view_model::grid::cells(part.as_bytes(), part.len().max(1), 4) {
                if line.width() > 0 && line.width() + cell.width > width {
                    lines.push(std::mem::take(&mut line));
                }
                line.push_span(Span::styled(if cell.text == "\t" { " ".repeat(cell.width) } else { cell.text }, style));
            }
        }
    }
    lines.push(line);
    lines
}

/// Decode chrome commands without allowing editor fallback to mutate the source.
pub(super) fn chrome_key(
    view: &mut Workspace,
    key: KeyEvent,
    keys: &mut KeymapEngine,
    at: Duration,
    preset: Preset,
) -> KeyResult {
    if let Some(chord) = chord(key) {
        match keys.feed_with_selection(chord) {
            VimResult::Command(call) => return invoke(view, call, false, at, preset),
            VimResult::Error(error) => {
                view.set_notice(error);
                return KeyResult::Changed(true);
            }
            _ => {}
        }
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('q' | 'Q')) {
        KeyResult::Quit
    } else {
        KeyResult::Changed(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn view(extension: &str, text: &str) -> (Self, Workspace) {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "kwil-tui-content-{}-{}.{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                extension
            ));
            std::fs::write(&path, text).unwrap();
            let file = kwil_core_workspace::open_for_editing(&path).unwrap();
            file.build_index(1).unwrap();
            let view = Workspace::new(View::new(Some((path.clone(), file)))).unwrap();
            (Self(path), view)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_file(&self.0).unwrap();
        }
    }
    fn host() -> (Host, mpsc::Receiver<()>) {
        let (tx, rx) = mpsc::channel();
        let mut host = Host::default();
        host.start(Arc::new(move || {
            let _ = tx.send(());
        }));
        (host, rx)
    }
    fn wait(host: &mut Host, view: &mut Workspace, rx: &mpsc::Receiver<()>, done: impl Fn(&Host) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        host.sync(view, 12);
        while !done(host) {
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
            host.sync(view, 12);
        }
    }
    fn stop(host: &mut Host) {
        host.settle(Instant::now() + Duration::from_secs(10)).unwrap();
    }
    #[test]
    fn markdown_modes_render_blocks_links_aligned_tables_and_keep_source_editable_in_split() {
        let (_file, mut view) = Fixture::view(
            "md",
            "# Heading\n\n*emphasis* and **strong** [site](https://example.com)\n\n- list\n\n```rust\nlet x = 1;\n```\n\n| A | B |\n|---|---|\n| long | x |\n",
        );
        let (mut host, rx) = host();
        host.command(&mut view, "view.markdownPreview");
        assert!(host.captures_body(&view));
        wait(&mut host, &mut view, &rx, |h| h.model.is_some());
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| host.draw(f, &view, f.area())).unwrap();
        let screen = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect::<String>();
        for text in [
            "Heading",
            "emphasis",
            "strong",
            "https://example.com",
            "- list",
            "let x = 1;",
            "| A    | B |",
            "| long | x |",
        ] {
            assert!(screen.contains(text), "missing {text}: {screen}");
        }
        host.command(&mut view, "view.markdownSplit");
        assert!(!host.captures_body(&view));
        view.execute("edit.insert", &CommandArgs { text: Some("Changed ".into()), ..Default::default() }, 1).unwrap();
        let expected = source(&view);
        wait(&mut host, &mut view, &rx, |h| h.model_source == expected);
        host.command(&mut view, "view.markdownText");
        assert_eq!(host.mode as u8, Mode::Text as u8);
        stop(&mut host);
    }
    #[test]
    fn log_filters_use_shared_projection_regex_levels_times_and_original_coordinates() {
        let (_file, mut view) = Fixture::view(
            "log",
            "2026-11-02T10:00:00Z INFO alpha\n2026-11-02T11:00:00Z ERROR beta\n2026-11-02T12:00:00Z WARN alpha\n",
        );
        let (mut host, rx) = host();
        host.command(&mut view, "log.filter");
        host.paste("alpha");
        assert!(host.key(&mut view, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        wait(&mut host, &mut view, &rx, |h| h.page.as_ref().is_some_and(|p| p.exact));
        assert_eq!(host.page.as_ref().unwrap().rows.iter().map(|r| r.line).collect::<Vec<_>>(), [0, 2]);
        host.query = "beta|alpha".into();
        host.command(&mut view, "log.toggleRegex");
        host.level = Level::ALL.iter().position(|l| *l == Level::Info).unwrap();
        host.command(&mut view, "log.toggleLevel");
        host.command(&mut view, "log.goToTime");
        host.from = "2026-11-02T10:30:00Z".into();
        host.to = "2026-11-02T11:30:00Z".into();
        host.apply();
        wait(&mut host, &mut view, &rx, |h| h.page.as_ref().is_some_and(|p| p.exact));
        assert_eq!(host.page.as_ref().unwrap().rows.iter().map(|r| r.line).collect::<Vec<_>>(), [1]);
        assert_eq!(host.mark_line(&view), Some(2));
        host.key(&mut view, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!host.open);
        assert_eq!(view.status_position().unwrap().0, 1);
        host.command(&mut view, "log.clearFilter");
        assert_eq!(host.filter, Filter::default());
        stop(&mut host);
    }
    #[test]
    fn invalid_ranges_preserve_the_filter_and_log_input_is_bounded() {
        let (_file, mut view) = Fixture::view("log", "INFO original\n");
        let (mut host, _) = host();
        host.command(&mut view, "log.goToTime");
        let original = host.filter;
        host.from = "2026-11-02T12:00:00Z".into();
        host.to = "2026-11-02T10:00:00Z".into();
        host.apply();
        assert_eq!(host.filter, original);
        assert!(host.error.contains("precedes"));
        host.field = Some(0);
        host.paste(&"x".repeat(257));
        assert!(host.query.is_empty());
        host.paste("one\ntwo");
        assert!(host.query.is_empty());
        host.command(&mut view, "log.resume");
        assert!(host.following);
        host.command(&mut view, "log.pause");
        assert!(!host.following);
        stop(&mut host);
    }
    #[test]
    fn large_markdown_worker_tracks_deep_editor_scroll() {
        let text = (0..24_000).map(|i| format!("# Section {i:06}\n\nA **bold** paragraph.\n\n")).collect::<String>();
        let (_file, mut view) = Fixture::view("md", &text);
        let (mut host, rx) = host();
        host.command(&mut view, "view.markdownSplit");
        view.scroll(48_000);
        let expected = source(&view);
        wait(&mut host, &mut view, &rx, |h| h.model_source == expected && h.model_top == Some(48_000));
        assert!(host.large_markdown);
        assert!(host.model.as_ref().unwrap().blocks[0].source_lines.contains(&48_000));
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| host.draw(f, &view, f.area())).unwrap();
        let screen = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(screen.contains("Section 012000"), "{screen}");
        host.mouse(
            &mut view,
            &event::MouseEvent { kind: MouseEventKind::ScrollDown, column: 80, row: 5, modifiers: KeyModifiers::NONE },
        );
        assert_eq!(view.top(), 48_000, "scroll within a block retains its editor anchor");
        assert_eq!(host.markdown_top, 1);
        stop(&mut host);
    }
    #[test]
    fn markdown_rejects_large_sources_and_stale_tab_results() {
        let mut buffer = kwil_core_text::Buffer::empty();
        buffer.insert(0, &vec![b'x'; kwil_core_markdown::bounded::PREVIEW_SOURCE_BYTES + 1]).unwrap();
        assert!(parse(&buffer.snapshot()).unwrap_err().contains("256 KiB"));
        // A completion for an old tab must never paint in the active one.
        let (_file, mut view) = Fixture::view("md", "# Old tab\n");
        let (mut host, rx) = host();
        host.command(&mut view, "view.markdownPreview");
        host.sync(&mut view, 12);
        view.file_command("file.new").unwrap();
        host.sync(&mut view, 12);
        assert!(!host.captures_body(&view));
        assert!(host.model.is_none());
        rx.recv_timeout(Duration::from_secs(15)).unwrap();
        host.sync(&mut view, 12);
        assert!(host.model.is_none());
        stop(&mut host);
        let _ = &mut buffer;
    }
}
