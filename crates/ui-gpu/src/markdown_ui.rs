//! Bounded native Markdown host. One worker, one admitted snapshot, no UI reads.
use super::*;
use kwil_core_markdown::{BlockKind, InlineKind, ListMarker, Model, Span};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Mode {
    #[default]
    Text,
    Preview,
    Split,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModeAction {
    Unhandled,
    Unavailable,
    Set(Mode),
}
fn mode_action(command: &str, current: Mode, markdown_source: bool) -> ModeAction {
    let mode = match command {
        "view.toggleMarkdown" => match current {
            Mode::Text => Mode::Split,
            Mode::Split => Mode::Preview,
            Mode::Preview => Mode::Text,
        },
        "view.markdownText" => Mode::Text,
        "view.markdownPreview" => Mode::Preview,
        "view.markdownSplit" => Mode::Split,
        _ => return ModeAction::Unhandled,
    };
    if mode == Mode::Text || markdown_source { ModeAction::Set(mode) } else { ModeAction::Unavailable }
}
fn controls_available(markdown_source: bool, current: Mode) -> bool {
    markdown_source || current != Mode::Text
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Key {
    document: kwil_view_model::SessionDocumentId,
    revision: u64,
    epoch: u64,
    base: u64,
}
struct Work {
    key: Key,
    snapshot: kwil_core_text::Snapshot,
    path: Option<std::path::PathBuf>,
}
pub(super) struct Completion {
    key: Key,
    result: Result<Model, String>,
    images: markdown_images::Images,
    diagrams: Diagrams,
    highlights: CodeHighlights,
}
type CodeHighlights = std::collections::HashMap<usize, Vec<kwil_core_syntax::Span>>;
type Diagrams = std::collections::HashMap<usize, Result<kwil_core_markdown::diagram::Diagram, String>>;
type DiagramLayouts = std::collections::HashMap<usize, ((usize, u64, u64), Result<markdown_diagram::Layout, String>)>;
struct Lane {
    tx: mpsc::SyncSender<Work>,
    thread: std::thread::JoinHandle<()>,
}
#[derive(Default)]
pub(super) struct Host {
    modes: std::collections::HashMap<kwil_view_model::SessionDocumentId, Mode>,
    lane: Option<Lane>,
    stop: Arc<AtomicBool>,
    busy: bool,
    desired: Option<Key>,
    current: Option<Key>,
    base_path: Option<std::path::PathBuf>,
    path_generation: u64,
    model: Option<Model>,
    images: markdown_images::Images,
    diagrams: Diagrams,
    layouts: DiagramLayouts,
    highlights: CodeHighlights,
    error: Option<String>,
    scroll: usize,
    #[cfg(feature = "frame-harness")]
    probe_target: Option<u64>,
}
impl Host {
    #[cfg(feature = "frame-harness")]
    pub fn ready(&self) -> bool {
        self.model.as_ref().is_some_and(|model| !model.blocks.is_empty()) && self.current == self.desired
    }

    fn mode(&self, id: kwil_view_model::SessionDocumentId) -> Mode {
        self.modes.get(&id).copied().unwrap_or_default()
    }
    fn receive(&mut self, completion: Completion) {
        self.busy = false;
        if self.desired != Some(completion.key) {
            return;
        }
        self.current = Some(completion.key);
        self.images = completion.images;
        self.diagrams = completion.diagrams;
        self.layouts.clear();
        self.highlights = completion.highlights;
        match completion.result {
            Ok(model) => {
                self.model = Some(model);
                self.error = None;
            }
            Err(error) => {
                self.model = None;
                self.error = Some(error);
            }
        }
    }
    pub fn shutdown(&mut self) -> Option<shutdown::Job> {
        self.stop.store(true, Ordering::Release);
        self.lane.take().map(|Lane { tx, thread }| {
            Box::new(move || {
                drop(tx);
                thread.join().map_err(|_| "Markdown worker panicked".into())
            }) as shutdown::Job
        })
    }
}
fn code_highlights(model: &Model, stop: &AtomicBool) -> CodeHighlights {
    use kwil_core_syntax::{Config, Engine, Language};
    let mut result = CodeHighlights::new();
    let Ok(mut engine) = Engine::new(Config { parse_limit: 0, margin_bytes: 0, ..Default::default() }) else {
        return result;
    };
    let mut count = 0usize;
    for (index, block) in model.blocks.iter().enumerate() {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let BlockKind::CodeBlock { language: Some(language), raw } = &block.kind else { continue };
        let language = language.split_whitespace().next().unwrap_or("");
        let Some(language) = Language::ALL.iter().copied().find(|candidate| {
            candidate.id().eq_ignore_ascii_case(language)
                || candidate.extensions().split_whitespace().any(|ext| ext.eq_ignore_ascii_case(language))
        }) else {
            continue;
        };
        let source = raw.as_bytes().to_vec();
        if let Ok(highlighted) = engine
            .highlight(&source, language, index as u64, None, None, 0..source.len(), &|| stop.load(Ordering::Acquire))
        {
            let spans: Vec<_> = highlighted.spans.into_iter().take(8192usize.saturating_sub(count)).collect();
            count += spans.len();
            result.insert(index, spans);
        }
        if count >= 8192 {
            break;
        }
    }
    result
}
fn diagrams(model: &Model) -> Diagrams {
    let mut diagrams = Diagrams::new();
    let mut retained = 0usize;
    for (index, block) in model.blocks.iter().enumerate() {
        if let BlockKind::CodeBlock { language: Some(language), raw } = &block.kind
            && language.eq_ignore_ascii_case("mermaid")
        {
            if diagrams.len() >= 32 {
                break;
            }
            let parsed =
                kwil_core_markdown::diagram::parse(raw).map_err(|error| error.to_string()).and_then(|diagram| {
                    let bytes = diagram.text_bytes();
                    if retained.saturating_add(bytes) > 64 * 1024 {
                        Err("Diagram text exceeds the shared 64 KiB limit".into())
                    } else {
                        retained += bytes;
                        Ok(diagram)
                    }
                });
            diagrams.insert(index, parsed);
        }
    }
    diagrams
}
fn parse(snapshot: &kwil_core_text::Snapshot, stop: &AtomicBool) -> Result<Model, String> {
    use kwil_core_markdown::bounded::{PREVIEW_SOURCE_BYTES, preview_model};
    if snapshot.len() > PREVIEW_SOURCE_BYTES as u64 {
        return Err(
            "Preview unavailable: this document exceeds the 256 KiB preview limit. Text editing remains available."
                .into(),
        );
    }
    let mut bytes = Vec::with_capacity(snapshot.len() as usize);
    for chunk in snapshot.chunks(0..snapshot.len()).map_err(|e| e.to_string())? {
        if stop.load(Ordering::Acquire) {
            return Err("Preview cancelled".into());
        }
        bytes.extend_from_slice(chunk.map_err(|e| e.to_string())?);
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| "Preview unavailable: malformed or opaque text cannot be interpreted as Markdown".to_owned())?;
    preview_model(text, &|| stop.load(Ordering::Acquire)).map_err(|e| format!("Preview unavailable: {e:?}"))
}
impl App {
    // Menus keep the Text escape available after Save As changes the suffix.
    pub(super) fn is_markdown_document(&self) -> bool {
        controls_available(self.markdown_source_eligible(), self.markdown_mode())
    }
    fn markdown_source_eligible(&self) -> bool {
        match self.highlighting.as_ref().and_then(|host| host.detected_language(&self.view)) {
            Some(language) => language == kwil_core_syntax::Language::Markdown,
            None => self.view.editor().is_some_and(|editor| editor.is_markdown()),
        }
    }

    pub(super) fn preview_blocks_command(&mut self, command: &str) -> bool {
        if self.markdown_mode() != Mode::Preview || !command.starts_with("edit.") || command == "edit.copy" {
            return false;
        }
        let damage = self.view.set_notice("Switch to Text or Split to edit".into());
        self.apply(damage);
        true
    }
    #[cfg(feature = "frame-harness")]
    pub(super) fn markdown_probe_click(&mut self, el: &ActiveEventLoop, mode: Mode) -> bool {
        let command = match mode {
            Mode::Text => "view.markdownText",
            Mode::Preview => "view.markdownPreview",
            Mode::Split => "view.markdownSplit",
        };
        let Some(tool) = self.menu.toolbar.iter().find(|tool| tool.command == command) else { return false };
        self.pointer = (tool.rect.x + tool.rect.w / 2, tool.rect.y + tool.rect.h / 2);
        self.menu_mouse(el)
    }
    /// The next paint measures preceding blocks without painting or extra IO,
    /// then starts exactly at this source section using the production layout.
    #[cfg(feature = "frame-harness")]
    pub(super) fn markdown_probe_position(&mut self, source_line: u64) {
        self.markdown.probe_target = Some(source_line);
        self.markdown.scroll = 0;
        self.apply(Damage { full: true, ..Default::default() });
    }
    pub(super) fn markdown_mode(&self) -> Mode {
        self.markdown.mode(self.view.active_id())
    }
    pub(super) fn dispatch_markdown(&mut self, command: &str) -> bool {
        let mode = match mode_action(command, self.markdown_mode(), self.markdown_source_eligible()) {
            ModeAction::Unhandled => return false,
            ModeAction::Unavailable => return true,
            ModeAction::Set(mode) => mode,
        };
        self.set_markdown_mode(mode);
        true
    }
    fn set_markdown_mode(&mut self, mode: Mode) {
        let id = self.view.active_id();
        self.markdown.modes.insert(id, mode);
        self.markdown.scroll = 0;
        self.drag = None;
        self.selecting = false;
        self.ime_destination = Some(ime::Destination::Cancelled);
        self.composing = false;
        if let Some(keys) = &mut self.keys {
            keys.cancel_pending();
        }
        let rows = self.text_rows();
        self.view.set_rows(rows);
        self.apply(Damage { full: true, status: true, ..Default::default() });
    }
    pub(super) fn sync_markdown(&mut self) {
        if self.markdown_mode() == Mode::Text {
            self.markdown.desired = None;
            return;
        }
        let Some(editor) = self.view.editor() else { return };
        let path = editor.document().path().map(std::path::Path::to_path_buf);
        if path != self.markdown.base_path {
            let Some(generation) = self.markdown.path_generation.checked_add(1) else {
                self.markdown.error = Some("Preview path generation exhausted; reopen the application".into());
                return;
            };
            self.markdown.path_generation = generation;
            self.markdown.base_path = path;
        }
        let key = Key {
            document: self.view.active_id(),
            revision: editor.document().revision(),
            epoch: self.view.input_epoch(),
            base: self.markdown.path_generation,
        };
        if self.markdown.desired != Some(key) {
            if self.markdown.desired.is_none_or(|old| old.document != key.document) {
                self.markdown.scroll = 0;
            }
            self.markdown.desired = Some(key);
            self.markdown.current = None;
            self.markdown.model = None;
            self.markdown.error = None;
        }
        if self.markdown.busy || self.markdown.current == Some(key) {
            return;
        }
        if self.markdown.lane.is_none() {
            let (tx, rx) = mpsc::sync_channel::<Work>(1);
            let proxy = self.proxy.clone();
            let stop = self.markdown.stop.clone();
            match std::thread::Builder::new().name("kwil-markdown".into()).spawn(move || {
                while let Ok(work) = rx.recv() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let result = parse(&work.snapshot, &stop);
                    let images = result.as_ref().map_or_else(
                        |_| Default::default(),
                        |model| markdown_images::load_images(model, work.path.as_deref(), &stop),
                    );
                    let diagrams = result.as_ref().map(diagrams).unwrap_or_default();
                    let highlights = result.as_ref().map(|model| code_highlights(model, &stop)).unwrap_or_default();
                    // Snapshot and its file/spill leases retire on this worker.
                    drop(work.snapshot);
                    if proxy
                        .send_event(UserEvent::MarkdownReady(Box::new(Completion {
                            key: work.key,
                            result,
                            images,
                            diagrams,
                            highlights,
                        })))
                        .is_err()
                    {
                        break;
                    }
                }
            }) {
                Ok(thread) => self.markdown.lane = Some(Lane { tx, thread }),
                Err(error) => {
                    self.markdown.error = Some(error.to_string());
                    return;
                }
            }
        }
        let work = Work {
            key,
            snapshot: editor.document().snapshot(),
            path: editor.document().path().map(|p| p.to_path_buf()),
        };
        match self.markdown.lane.as_ref().unwrap().tx.try_send(work) {
            Ok(()) => self.markdown.busy = true,
            Err(error) => {
                self.markdown.error = Some(format!("Preview worker unavailable: {error}"));
                self.markdown.current = Some(key);
            }
        }
    }
    pub(super) fn markdown_ready(&mut self, completion: Completion) {
        self.markdown.receive(completion);
        self.apply(Damage { full: true, ..Default::default() });
    }
    pub(super) fn markdown_pointer(&mut self) -> bool {
        let eligible =
            self.view.editor().is_some_and(|editor| editor.is_markdown()) || self.markdown_mode() != Mode::Text;
        if !eligible {
            return false;
        }
        preview_area(self).is_some_and(|area| area.contains(self.pointer))
    }
    pub(super) fn markdown_scroll_key(&mut self, delta: i64) {
        self.markdown.scroll = self.markdown.scroll.saturating_add_signed(delta as isize).min(1_048_576);
        self.apply(Damage { full: true, ..Default::default() });
    }
    pub(super) fn markdown_wheel(&mut self, delta: i64) -> bool {
        if self.markdown_mode() == Mode::Text || !preview_area(self).is_some_and(|area| area.contains(self.pointer)) {
            return false;
        }
        self.markdown.scroll = self.markdown.scroll.saturating_add_signed(delta as isize).min(1_048_576);
        self.apply(Damage { full: true, ..Default::default() });
        true
    }
}
pub(super) fn preview_area(app: &App) -> Option<Rect> {
    let body = chrome::layout(app.size, &app.ui, app.settings.tabs.layout).body;
    let mut rect = tabs::rect(body);
    match app.markdown_mode() {
        Mode::Text => None,
        Mode::Preview => Some(rect),
        Mode::Split => {
            let half = rect.w / 2;
            rect.x += half;
            rect.w -= half;
            Some(rect)
        }
    }
}
/// Row coordinates are always body-line units, including headings and images.
/// Signed placement clips partially scrolled blocks without changing their layout.
struct Flow<'a> {
    pixels: &'a mut [u32],
    stride: usize,
    clip: Rect,
    font: &'a mut font::UiFont,
    theme: &'a kwil_core_commands::theme::Theme,
    images: &'a markdown_images::Images,
    x: usize,
    left: usize,
    right: usize,
    origin: usize,
    line: usize,
    skip: usize,
    height: usize,
    step: usize,
    heading: bool,
    bold: bool,
    paint: bool,
    fence: bool,
    background_line: Option<usize>,
    foreground: Option<u32>,
    quote: usize,
    quote_left: usize,
    unit: usize,
    after_cr: bool,
}
impl Flow<'_> {
    fn y(&self) -> i64 {
        self.origin as i64 + (self.line as i64 - self.skip as i64) * self.height as i64
    }
    fn next(&mut self) {
        self.advance(self.step);
    }
    fn advance(&mut self, rows: usize) {
        self.line += rows;
        self.x = self.left;
    }
    fn visible(&self) -> bool {
        self.paint
            && self.y() < (self.clip.y + self.clip.h) as i64
            && self.y() + (self.height * self.step) as i64 > self.clip.y as i64
    }
    fn done(&self) -> bool {
        self.paint && self.y() >= (self.clip.y + self.clip.h) as i64
    }
    fn fill(&mut self, x: usize, y: i64, width: usize, height: usize, color: u32) {
        if !self.paint {
            return;
        }
        let top = y.max(self.clip.y as i64);
        let bottom = (y + height as i64).min((self.clip.y + self.clip.h) as i64);
        let left = x.max(self.clip.x);
        let right = x.saturating_add(width).min(self.clip.x + self.clip.w);
        if bottom > top && right > left {
            chrome::fill_clipped(
                self.pixels,
                self.stride,
                Rect { x: left, y: top as usize, w: right - left, h: (bottom - top) as usize },
                color,
            );
        }
    }
    fn role(&self, role: kwil_core_commands::theme::UiRole) -> u32 {
        theme_ui::rgb(self.theme, role)
    }
    fn row(&mut self) {
        if !self.visible() || self.background_line == Some(self.line) {
            return;
        }
        self.background_line = Some(self.line);
        if self.fence {
            self.fill(
                self.left.saturating_sub(self.unit),
                self.y(),
                self.right - self.left + 2 * self.unit,
                self.height * self.step,
                self.role(kwil_core_commands::theme::UiRole::TabBackground),
            );
        }
        for depth in 0..self.quote {
            self.fill(
                self.quote_left + depth * self.unit * 2,
                self.y(),
                (self.unit / 4).max(1),
                self.height * self.step,
                self.role(kwil_core_commands::theme::UiRole::Accent),
            );
        }
    }
    fn width(&mut self, text: &str, code: bool) -> usize {
        let face = if code {
            self.font.monospace()
        } else if self.heading {
            self.font.heading()
        } else {
            &mut *self.font
        };
        face.width(text)
    }
    fn text(&mut self, text: &str, style: &kwil_core_markdown::Style, code: bool) {
        use unicode_segmentation::UnicodeSegmentation;
        // Each shaping operation stays below UiFont's scalar limit. Graphemes
        // are admitted on the worker and never split by long-word wrapping.
        for word in text.split_inclusive(char::is_whitespace) {
            let mut piece = String::new();
            let mut scalars = 0;
            for cluster in word.graphemes(true) {
                if self.done() {
                    return;
                }
                if cluster == "\n" && self.after_cr {
                    self.after_cr = false;
                    continue;
                }
                self.after_cr = cluster == "\r";
                if cluster == "\n" || cluster == "\r\n" || cluster == "\r" {
                    if !piece.is_empty() {
                        self.run(&piece, style, code);
                        piece.clear();
                    }
                    scalars = 0;
                    self.row();
                    self.next();
                } else if cluster == "\t" && code {
                    if !piece.is_empty() {
                        self.run(&piece, style, code);
                        piece.clear();
                    }
                    scalars = 0;
                    let cell = self.width(" ", true).max(1);
                    let column = (self.x - self.left) / cell;
                    for _ in 0..4 - column % 4 {
                        self.run(" ", style, true);
                    }
                } else {
                    let count = cluster.chars().count();
                    if scalars + count > 64 && !piece.is_empty() {
                        self.run(&piece, style, code);
                        piece.clear();
                        scalars = 0;
                    }
                    piece.push_str(cluster);
                    scalars += count;
                }
            }
            if !piece.is_empty() {
                self.run(&piece, style, code);
            }
        }
    }
    fn run(&mut self, text: &str, style: &kwil_core_markdown::Style, code: bool) {
        use unicode_segmentation::UnicodeSegmentation;
        if self.done() {
            return;
        }
        let width = self.width(text, code);
        let available = self.right.saturating_sub(self.left);
        if width > available && text.graphemes(true).take(2).count() > 1 {
            for cluster in text.graphemes(true) {
                self.run(cluster, style, code);
            }
            return;
        }
        if self.x.saturating_add(width) > self.right && self.x > self.left {
            self.next();
        }
        // Prose wrap collapses leading whitespace; code retains every space.
        if !code && self.x == self.left && text.chars().all(char::is_whitespace) {
            return;
        }
        if self.visible() {
            self.row();
            let fg = self.foreground.unwrap_or_else(|| {
                self.role(if style.link.is_some() {
                    kwil_core_commands::theme::UiRole::Accent
                } else {
                    kwil_core_commands::theme::UiRole::PanelForeground
                })
            });
            if code && !self.fence {
                self.fill(
                    self.x,
                    self.y(),
                    width,
                    self.height,
                    self.role(kwil_core_commands::theme::UiRole::TabBackground),
                );
            }
            let y = self.y();
            let face = if code {
                self.font.monospace()
            } else if self.heading {
                self.font.heading()
            } else {
                &mut *self.font
            };
            // baseline() is unsigned, but partially visible rows need a signed top.
            let baseline = y + face.baseline(0, self.height * self.step) as i64;
            if baseline >= 0 {
                let clip = Rect {
                    x: self.left.max(self.clip.x),
                    y: self.clip.y,
                    w: self.right.min(self.clip.x + self.clip.w).saturating_sub(self.left.max(self.clip.x)),
                    h: self.clip.h,
                };
                face.draw_styled(self.pixels, self.stride, clip, self.x, baseline as usize, text, fg, style.emphasis);
                if style.strong || self.bold {
                    face.draw_styled(
                        self.pixels,
                        self.stride,
                        clip,
                        self.x + 1,
                        baseline as usize,
                        text,
                        fg,
                        style.emphasis,
                    );
                }
                if style.strikethrough {
                    self.fill(self.x, y + (self.height * self.step / 2) as i64, width, 1, fg);
                }
                if style.link.is_some() {
                    self.fill(self.x, baseline + 2, width, 1, fg);
                }
            }
        }
        self.x += width;
    }
    fn image(&mut self, alt: &str, url: &str) {
        let Some(Ok(image)) = self.images.get(url) else {
            let error =
                self.images.get(url).and_then(|r| r.as_ref().err()).map_or("Image limit reached", String::as_str);
            self.text(&format!("Image unavailable ({url}): {alt}. {error}"), &Default::default(), false);
            return;
        };
        if self.x != self.left {
            self.next();
        }
        let (iw, ih) = (image.width as usize, image.height as usize);
        let width = iw.min(self.right.saturating_sub(self.left)).max(1);
        let height = (ih * width / iw).max(1);
        let y = self.y();
        if self.paint {
            let from_y = (self.clip.y as i64 - y).max(0) as usize;
            let to_y = height.min(((self.clip.y + self.clip.h) as i64 - y).max(0) as usize);
            for dy in from_y..to_y {
                let sy = dy * ih / height;
                for dx in 0..width.min(self.clip.x + self.clip.w - self.left) {
                    let sx = dx * iw / width;
                    let from = (sy * iw + sx) * 4;
                    let rgba = &image.rgba[from..from + 4];
                    let color = (u32::from(rgba[0]) << 16) | (u32::from(rgba[1]) << 8) | u32::from(rgba[2]);
                    let target = &mut self.pixels[(y + dy as i64) as usize * self.stride + self.left + dx];
                    *target = font::blend(*target, color, rgba[3]);
                }
            }
        }
        self.advance(height.div_ceil(self.height));
    }
    fn spans(&mut self, spans: &[Span]) {
        for span in spans {
            if self.done() {
                break;
            }
            match &span.kind {
                InlineKind::Text(text) => self.text(text, &span.style, false),
                InlineKind::Code(text) => self.text(text, &span.style, true),
                InlineKind::SoftBreak => self.text(" ", &span.style, false),
                InlineKind::HardBreak => self.next(),
                InlineKind::Image { alt, url } => self.image(alt, url),
            }
        }
    }
    fn task_marker(&mut self, checked: bool) {
        self.row();
        let side = (2 * self.unit).min(self.height.saturating_sub(self.unit / 2)).max(4);
        let thickness = (self.unit / 6).max(1);
        let (x, y) = (self.x, self.y() + ((self.height - side) / 2) as i64);
        let accent = self.role(kwil_core_commands::theme::UiRole::Accent);
        let fg = self.role(kwil_core_commands::theme::UiRole::PanelForeground);
        if checked {
            self.fill(x, y, side, side, accent);
            let color = self.role(kwil_core_commands::theme::UiRole::PanelBackground);
            let (a, b) = (side / 4, side / 2);
            for offset in 0..side / 4 {
                self.fill(x + a + offset, y + (b + offset) as i64, thickness, thickness, color);
            }
            for offset in 0..side / 2 {
                self.fill(x + a + side / 4 + offset, y + (b + side / 4) as i64 - offset as i64,
                    thickness, thickness, color);
            }
        } else {
            self.fill(x, y, side, thickness, fg);
            self.fill(x, y + (side - thickness) as i64, side, thickness, fg);
            self.fill(x, y, thickness, side, fg);
            self.fill(x + side - thickness, y, thickness, side, fg);
        }
        self.x += side + self.unit;
    }
    fn cell(&mut self, spans: &[Span], left: usize, right: usize, paint: bool, bold: bool) -> (usize, usize) {
        let mut cell = Flow {
            pixels: self.pixels,
            stride: self.stride,
            clip: self.clip,
            font: self.font,
            theme: self.theme,
            images: self.images,
            x: left,
            left,
            right,
            origin: self.origin,
            line: self.line,
            skip: self.skip,
            height: self.height,
            step: 1,
            heading: false,
            bold,
            paint,
            fence: false,
            background_line: None,
            foreground: None,
            quote: 0,
            quote_left: left,
            unit: self.unit,
            after_cr: false,
        };
        cell.spans(spans);
        (cell.line - self.line + 1, cell.x.saturating_sub(left))
    }
    fn table(&mut self, alignments: &[kwil_core_markdown::Alignment], rows: &[Vec<Vec<Span>>]) {
        use kwil_core_markdown::Alignment;
        let columns = alignments.len().max(1);
        let available = self.right.saturating_sub(self.left);
        let inset = self.unit;
        if available / columns < 6 * self.unit {
            self.text("Table shown as stacked cells to fit this pane", &Default::default(), false);
            self.next();
            for row in rows {
                for (index, cell) in row.iter().enumerate() {
                    self.text(&format!("{}. ", index + 1), &Default::default(), false);
                    self.spans(cell);
                    self.next();
                    if self.done() {
                        return;
                    }
                }
                self.next();
            }
            return;
        }
        let left = self.left;
        let border = self.role(kwil_core_commands::theme::UiRole::Border);
        for (row_index, row) in rows.iter().enumerate() {
            if self.done() {
                break;
            }
            let mut row_lines = 1;
            let mut metrics = Vec::with_capacity(row.len().min(columns));
            for (index, cell) in row.iter().enumerate().take(columns) {
                let a = left + index * available / columns + inset;
                let b = left + (index + 1) * available / columns - inset;
                let measured = self.cell(cell, a, b, false, row_index == 0);
                row_lines = row_lines.max(measured.0);
                metrics.push(measured);
            }
            let row_height = (row_lines + 1) * self.height;
            let y = self.y();
            self.fill(
                left,
                y,
                available,
                row_height,
                self.role(if row_index % 2 == 0 {
                    kwil_core_commands::theme::UiRole::TabBackground
                } else {
                    kwil_core_commands::theme::UiRole::PanelBackground
                }),
            );
            for (index, cell) in row.iter().enumerate().take(columns) {
                let a = left + index * available / columns + inset;
                let b = left + (index + 1) * available / columns - inset;
                // Use the exact run advances from layout, including rounding at
                // style/word boundaries. Measuring a whole span differs by a
                // pixel and can spuriously wrap a right-aligned cell.
                let width = if metrics[index].0 == 1 { metrics[index].1.min(b - a) } else { b - a };
                let offset = match alignments.get(index) {
                    Some(Alignment::Center) => (b - a - width) / 2,
                    Some(Alignment::Right) => b - a - width,
                    _ => 0,
                };
                self.cell(cell, a + offset, b, self.paint, row_index == 0);
            }
            // Text decorations and inline-code backgrounds stay inside the
            // grid. Paint shared borders last so they cannot erase a rule.
            self.fill(left, y, available, 1, border);
            for index in 0..=columns {
                self.fill(left + index * available / columns, y, 1, row_height, border);
            }
            self.advance(row_lines + 1);
            self.fill(left, self.y(), available, 1, border);
        }
    }
}
#[allow(clippy::too_many_arguments)]
pub(super) fn draw(
    host: &mut Host,
    mode: Mode,
    eligible: bool,
    top: u64,
    pixels: &mut [u32],
    size: (usize, usize),
    area: Rect,
    font: &mut font::UiFont,
    ui: &chrome::Metrics,
    theme: &kwil_core_commands::theme::Theme,
) -> Option<Rect> {
    if mode == Mode::Text {
        return None;
    }
    let _ = eligible;
    let bg = theme_ui::rgb(theme, kwil_core_commands::theme::UiRole::PanelBackground);
    let mut pane = area;
    if mode == Mode::Split {
        let half = pane.w / 2;
        pane.x += half;
        pane.w -= half;
    }
    if pane.w == 0 || pane.h == 0 {
        return Some(area);
    }
    chrome::fill_clipped(pixels, size.0, pane, bg);
    let pad = ui.dip(24.0).min(pane.w / 8);
    let mut flow = Flow {
        pixels,
        stride: size.0,
        clip: pane,
        font,
        theme,
        images: &host.images,
        x: pane.x + pad,
        left: pane.x + pad,
        right: pane.x + pane.w.saturating_sub(pad),
        origin: pane.y + pad,
        line: 0,
        skip: host.scroll,
        height: ui.dip(25.0).max(1),
        step: 1,
        heading: false,
        bold: false,
        paint: true,
        fence: false,
        background_line: None,
        foreground: None,
        quote: 0,
        quote_left: pane.x + pad,
        unit: ui.dip(8.0).max(1),
        after_cr: false,
    };
    #[cfg(feature = "frame-harness")]
    {
        if host.probe_target.is_some() {
            flow.paint = false;
        }
    }
    if let Some(model) = &host.model {
        let anchor = if mode == Mode::Split { top as usize } else { 0 };
        for (index, block) in model.blocks.iter().enumerate() {
            if block.source_lines.end <= anchor {
                continue;
            }
            if flow.done() {
                break;
            }
            #[cfg(feature = "frame-harness")]
            if host.probe_target.is_some_and(|target| block.source_lines.end as u64 > target) {
                host.scroll = flow.line;
                flow.skip = flow.line;
                host.probe_target = None;
                flow.paint = true;
            }
            flow.heading = matches!(block.kind, BlockKind::Heading(1 | 2));
            flow.step = if flow.heading { 2 } else { 1 };
            flow.bold = matches!(block.kind, BlockKind::Heading(1 | 3..=6));
            flow.quote = block.quote_depth.min(8);
            flow.left = pane.x + pad + (flow.quote + block.list_depth.saturating_sub(1)).min(12) * 2 * flow.unit;
            flow.left = flow.left.min(flow.right.saturating_sub(flow.unit));
            flow.x = flow.left;
            flow.background_line = None;
            match &block.kind {
                BlockKind::Heading(_) | BlockKind::Paragraph => flow.spans(&block.spans),
                BlockKind::ListItem { marker, task } => {
                    if let Some(checked) = task {
                        flow.task_marker(*checked);
                    } else {
                        let mark = match marker {
                            ListMarker::Bullet(_) => "• ".into(),
                            ListMarker::Ordered { number, .. } => format!("{number}. "),
                        };
                        flow.text(&mark, &Default::default(), false);
                    }
                    flow.left = flow.x; // hanging indent survives every wrapped line
                    flow.spans(&block.spans);
                }
                BlockKind::CodeBlock { language, raw } => {
                    let mut drawn = false;
                    if language.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("mermaid")) {
                        let width = flow.right.saturating_sub(flow.left);
                        let key = (width, flow.font.id(), ui.scale.to_bits());
                        if host.layouts.get(&index).is_none_or(|(old, _)| *old != key) {
                            let layout = match host.diagrams.get(&index) {
                                Some(Ok(diagram)) => {
                                    markdown_diagram::prepare(diagram, width, flow.font, ui).map_err(|e| e.to_string())
                                }
                                Some(Err(error)) => Err(error.clone()),
                                None => Err("Diagram count exceeds the bounded preview limit".into()),
                            };
                            host.layouts.insert(index, (key, layout));
                        }
                        match &host.layouts[&index].1 {
                            Ok(layout) => {
                                if flow.paint {
                                    let y = flow.y();
                                    let _ = markdown_diagram::draw(
                                        layout,
                                        flow.pixels,
                                        flow.stride,
                                        (flow.left as i64, y),
                                        pane,
                                        flow.font,
                                        theme,
                                    );
                                }
                                flow.advance(layout.height().div_ceil(flow.height));
                                drawn = true;
                            }
                            Err(error) => {
                                flow.text(&markdown_diagram::notice(error), &Default::default(), false);
                                flow.next();
                            }
                        }
                    }
                    if !drawn {
                        flow.fence = true;
                        flow.row();
                        flow.text(language.as_deref().unwrap_or("code"), &Default::default(), false);
                        flow.next();
                        let mut at = 0;
                        for span in host.highlights.get(&index).into_iter().flatten() {
                            if span.range.start < at
                                || span.range.end > raw.len()
                                || !raw.is_char_boundary(span.range.start)
                                || !raw.is_char_boundary(span.range.end)
                            {
                                continue;
                            }
                            flow.foreground = None;
                            flow.text(&raw[at..span.range.start], &Default::default(), true);
                            flow.foreground = theme
                                .style(span.class.name())
                                .foreground
                                .map(|c| (u32::from(c.0) << 16) | (u32::from(c.1) << 8) | u32::from(c.2));
                            flow.text(&raw[span.range.clone()], &Default::default(), true);
                            at = span.range.end;
                            if flow.done() {
                                break;
                            }
                        }
                        flow.foreground = None;
                        flow.text(&raw[at..], &Default::default(), true);
                        flow.row();
                        flow.fence = false;
                    }
                }
                BlockKind::Table { rows, alignments } => flow.table(alignments, rows),
                BlockKind::ThematicBreak => flow.fill(
                    flow.left,
                    flow.y() + (flow.height / 2) as i64,
                    flow.right - flow.left,
                    1,
                    flow.role(kwil_core_commands::theme::UiRole::Border),
                ),
                BlockKind::Image { alt, url } => flow.image(alt, url),
                BlockKind::Html(raw) => {
                    flow.text("HTML is displayed as inert source", &Default::default(), false);
                    flow.next();
                    flow.text(raw, &Default::default(), true);
                }
            }
            // Lists use one row per item; paragraphs have one breathing row.
            let compact = matches!(block.kind, BlockKind::ListItem { .. });
            flow.next();
            flow.step = 1;
            if !compact {
                flow.next();
            }
        }
        if !flow.done() {
            let visible = pane.h.saturating_sub(pad) / flow.height;
            let last = flow.line.saturating_sub(visible.saturating_sub(1));
            if host.scroll > last {
                host.scroll = last;
                return draw(host, mode, eligible, top, pixels, size, area, font, ui, theme);
            }
        }
    } else {
        flow.text(host.error.as_deref().unwrap_or("Preparing Markdown preview…"), &Default::default(), false);
    }
    Some(area)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn save_as_plain_text_keeps_an_exit_without_admitting_another_preview() {
        for old in [Mode::Preview, Mode::Split] {
            assert!(controls_available(false, old), "Text escape must remain offered");
            assert_eq!(mode_action("view.markdownText", old, false), ModeAction::Set(Mode::Text));
            assert_eq!(mode_action("view.markdownPreview", old, false), ModeAction::Unavailable);
            assert_eq!(mode_action("view.markdownSplit", old, false), ModeAction::Unavailable);
        }
        assert_eq!(mode_action("view.toggleMarkdown", Mode::Preview, false), ModeAction::Set(Mode::Text));
        assert!(!controls_available(false, Mode::Text));
        assert_eq!(mode_action("view.toggleMarkdown", Mode::Text, false), ModeAction::Unavailable);
        assert_eq!(mode_action("file.save", Mode::Preview, false), ModeAction::Unhandled);
    }
    fn render(source: &str, height: usize, scroll: usize) -> Vec<u32> {
        render_scaled(source, 540, height, scroll, 1.0)
    }
    fn render_scaled(source: &str, width: usize, height: usize, scroll: usize, scale: f64) -> Vec<u32> {
        let mut host = Host { model: Some(kwil_core_markdown::parse(source).unwrap()), scroll, ..Default::default() };
        let ui = chrome::Metrics::new(scale);
        let mut font = font::UiFont::load(ui.text_px()).expect("test needs the platform UI font");
        let mut pixels = vec![0x123456; width * height];
        draw(
            &mut host,
            Mode::Preview,
            true,
            0,
            &mut pixels,
            (width, height),
            Rect { x: 0, y: 0, w: width, h: height },
            &mut font,
            &ui,
            &kwil_core_commands::theme::Theme::builtin(kwil_core_commands::theme::Scheme::Dark),
        );
        pixels
    }
    #[test]
    fn checked_and_unchecked_tasks_have_distinct_native_markers() {
        let checked = render("- [x] Task\n", 150, 0);
        let unchecked = render("- [ ] Task\n", 150, 0);
        assert_ne!(checked, unchecked, "task state must survive missing symbol-font glyphs");
        let theme = kwil_core_commands::theme::Theme::builtin(kwil_core_commands::theme::Scheme::Dark);
        let accent = theme_ui::rgb(&theme, kwil_core_commands::theme::UiRole::Accent);
        assert!(checked[24 * 540..49 * 540].contains(&accent), "checked state has a filled native checkbox");
        assert!(!unchecked[24 * 540..49 * 540].contains(&accent), "unchecked state stays hollow");
    }
    #[test]
    fn inline_code_background_never_erases_table_rules() {
        let pixels = render("| Header |\n| --- |\n| `inline_code()` |\n", 250, 0);
        let theme = kwil_core_commands::theme::Theme::builtin(kwil_core_commands::theme::Scheme::Dark);
        let border = theme_ui::rgb(&theme, kwil_core_commands::theme::UiRole::Border);
        for x in 24..516 { assert_eq!(pixels[74 * 540 + x], border, "rule at x={x}"); }
    }
    #[test]
    fn right_aligned_showcase_cell_uses_one_line_at_fractional_dpi() {
        let pixels = render_scaled("| Feature | Example | Status |\n| :--- | :---: | ---: |\n| Escapes | A \\| B | Literal pipe |\n", 1898, 400, 0, 1.5);
        let theme = kwil_core_commands::theme::Theme::builtin(kwil_core_commands::theme::Scheme::Dark);
        let bg = theme_ui::rgb(&theme, kwil_core_commands::theme::UiRole::PanelBackground);
        // Header starts at36px, body at112px; its second row is vertical padding.
        for y in 151..185 { for x in 1270..1840 {
            assert_eq!(pixels[y * 1898 + x], bg, "unnecessary wrapped text at ({x},{y})");
        }}
    }
    #[test]
    fn scrolled_heading_and_wrapped_content_match_full_canvas_crop() {
        let source = format!("# Heading\n\n{}", "A paragraph with **strong**, *emphasis* and `code`.\n\n".repeat(24));
        let full = render(&source, 2400, 0);
        let viewport = render(&source, 250, 4);
        for y in 24..240 {
            assert_eq!(&viewport[y * 540..(y + 1) * 540], &full[(y + 4 * 25) * 540..(y + 4 * 25 + 1) * 540], "row {y}");
        }
    }
    #[test]
    fn table_paints_stable_cell_boundaries_instead_of_spaces() {
        let pixels = render("| Left | Center | Right |\n| :--- | :---: | ---: |\n| A | B | C |\n", 400, 0);
        let theme = kwil_core_commands::theme::Theme::builtin(kwil_core_commands::theme::Scheme::Dark);
        let border = theme_ui::rgb(&theme, kwil_core_commands::theme::UiRole::Border);
        for x in [24, 188, 352, 516] {
            assert_eq!(pixels[30 * 540 + x], border, "column edge {x}");
        }
    }
    #[test]
    fn whole_source_and_invalid_input_are_explicit() {
        let stop = AtomicBool::new(false);
        let mut buffer = kwil_core_text::Buffer::default();
        buffer.insert(0, b"# Heading\n\n[global][ref]\n\n[ref]: https://example.com\n").unwrap();
        let model = parse(&buffer.snapshot(), &stop).unwrap();
        assert!(matches!(model.blocks[0].kind, BlockKind::Heading(1)));
        assert!(
            model.blocks.iter().flat_map(|b| &b.spans).any(|s| s.style.link.as_deref() == Some("https://example.com"))
        );
        buffer.insert(0, &vec![b'x'; 256 * 1024]).unwrap();
        assert!(parse(&buffer.snapshot(), &stop).unwrap_err().contains("256 KiB"));
        let mut buffer = kwil_core_text::Buffer::default();
        buffer.insert(0, b"\xff").unwrap();
        assert!(parse(&buffer.snapshot(), &stop).unwrap_err().contains("opaque"));
    }
    #[test]
    fn stale_completion_never_replaces_current_tab() {
        let key = Key { document: kwil_view_model::SessionDocumentId(1), revision: 1, epoch: 1, base: 0 };
        let mut host = Host { desired: Some(Key { revision: 2, ..key }), busy: true, ..Default::default() };
        host.receive(Completion {
            key,
            result: Ok(kwil_core_markdown::parse("# stale").unwrap()),
            images: Default::default(),
            diagrams: Default::default(),
            highlights: Default::default(),
        });
        assert!(!host.busy);
        assert!(host.model.is_none());
        assert!(host.current.is_none());
    }
}
