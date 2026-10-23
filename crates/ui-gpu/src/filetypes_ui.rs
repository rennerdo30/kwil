//! "Register file types" page inside the settings window, over the frozen
//! `file_type_registration` service (KW46). Selection lives in the service's
//! `Selection`; previews, registration, skip, status and unregister are its
//! own requests on its own worker. This page never claims an operating-system
//! result the service has not reported: Windows registers "Open with" only and
//! the default app stays the user's choice in Windows Settings.
//!
//! Requires ROOT adoption of `core_commands::file_types` and
//! `view_model::file_type_registration`; until then `hosted_none` is compiled
//! instead and no entry row exists.
use super::*;
use desktop::{Colors, Field, Hits, Sizes, Style, inset};
use kwil_core_commands::file_types::{self, Extension, Group, Preset, Selection};
use kwil_view_model::file_type_registration::{
    ActionError, ActionErrorKind, ExtensionOutcome, FileTypeRegistration, RegistrationStatus,
};
use settings_ui::{HostedPage, Target};

const PRESET: u32 = 0x1000_0000;
const TOGGLE: u32 = 0x2000_0000;
const CUSTOM: u32 = 0x3000_0000;
const GROUP: u32 = 0x4000_0000;
const PRIMARY: u32 = 1;
const SKIP: u32 = 2;
const UNREGISTER: u32 = 3;
const ADD: u32 = 4;
const FIELD: u32 = 5;
const MASK: u32 = 0xf000_0000;

const PRESETS: [(Preset, &str); 4] = [
    (Preset::CommonTextConfig, "Common"),
    (Preset::AllTextTypes, "All text types"),
    (Preset::PlainTextOnly, "Plain text"),
    (Preset::Custom, "Custom"),
];

fn group_title(group: Group) -> &'static str {
    match group {
        Group::PlainText => "Plain text",
        Group::Markup => "Markup and documents",
        Group::Data => "Data",
        Group::Configuration => "Configuration",
        Group::Systems => "C, C++ and systems languages",
        Group::Managed => "Java, .NET and JVM languages",
        Group::Scripts => "Scripts",
        Group::Web => "Web",
        Group::Build => "Build and project files",
        Group::Scientific => "Scientific and shaders",
    }
}

/// What the platform will do, stated before anything is requested.
fn platform_note() -> &'static str {
    if cfg!(windows) {
        "Adds Kwil to “Open with” for your account only. Windows keeps the default app your choice: set it in Settings › Apps › Default apps."
    } else if cfg!(target_os = "macos") {
        "Asks Launch Services to list this Kwil app for the chosen types, for your account only."
    } else {
        "Writes a per-user desktop entry and MIME defaults (xdg-mime). Nothing outside your home folder changes."
    }
}

fn error_text(error: &ActionError) -> String {
    match &error.kind {
        ActionErrorKind::Busy => "A registration request is still running.".into(),
        ActionErrorKind::Queue(_) => "The registration service is not accepting requests; try again.".into(),
        ActionErrorKind::EmptySelection => "Select at least one file type.".into(),
        ActionErrorKind::UnadoptedCapacity { selected, limit } => {
            format!("{selected} types selected; this build registers at most {limit} at once.")
        }
        ActionErrorKind::MissingContentTypes(missing) => {
            let list: Vec<&str> = missing.iter().take(8).map(Extension::as_str).collect();
            format!("No verified content type for: {}{}", list.join(", "), if missing.len() > 8 { ", …" } else { "" })
        }
        ActionErrorKind::ExtendNotAdopted { .. } => {
            "Kwil is already registered. Adding types to an existing registration is not available in this build."
                .into()
        }
        ActionErrorKind::StaleReview => "The selection changed after the review; review again.".into(),
        ActionErrorKind::StaleCurrentFile => "The current file changed; review again.".into(),
        ActionErrorKind::InvalidNativePath => "Kwil's own path cannot be registered.".into(),
        ActionErrorKind::Extension(_) => "Use letters, digits, _, - or + (up to 64), like “log” or “.cfg”.".into(),
        ActionErrorKind::Exhausted => "The registration journal is full; see the settings documentation.".into(),
    }
}

fn status_text(status: &RegistrationStatus) -> (String, bool) {
    match status {
        RegistrationStatus::Idle => ("Not reviewed yet".into(), false),
        RegistrationStatus::Pending => ("Working…".into(), false),
        RegistrationStatus::Ready { .. } => ("Reviewed: nothing has changed yet".into(), false),
        RegistrationStatus::Skipped => ("Skipped: nothing was changed".into(), false),
        RegistrationStatus::Cancelled => ("Review cancelled".into(), false),
        RegistrationStatus::Applied { .. } => ("File-type choices applied".into(), false),
        RegistrationStatus::Unregistered { .. } => ("Unregistered: previous state restored".into(), false),
        RegistrationStatus::RetainedRecovery { .. } => {
            ("Existing registration saved; Unregister restores previous choices".into(), true)
        }
        RegistrationStatus::NoRetainedReceipt { .. } => ("No registration recorded".into(), false),
        RegistrationStatus::Failed { message, .. } => (desktop::visible(message, 200), true),
    }
}

fn outcome_mark(outcome: ExtensionOutcome) -> Option<bool> {
    // Some(false) = success mark, Some(true) = needs attention.
    match outcome {
        ExtensionOutcome::ReceiptApplied | ExtensionOutcome::UnchangedAtPreview => None,
        ExtensionOutcome::VerificationIncomplete
        | ExtensionOutcome::NeedsInspection
        | ExtensionOutcome::PartiallyApplied
        | ExtensionOutcome::Conflict
        | ExtensionOutcome::Unreadable => Some(true),
        ExtensionOutcome::Planned | ExtensionOutcome::RestoredVisible => None,
    }
}

/// Page-local state: custom-extension draft and scroll. Everything else is
/// read from the service on each paint.
#[derive(Default)]
pub(super) struct RegistrationUi {
    pub setup: bool,
    scroll: usize,
    rows: (usize, usize),
    custom: String,
    field_focused: bool,
    error: Option<String>,
}

pub(super) struct Page {
    pub service: FileTypeRegistration,
    pub ui: RegistrationUi,
}

/// The reviewed preview still describes the current selection.
fn review_is_current(service: &FileTypeRegistration) -> Option<kwil_view_model::file_type_registration::ReviewId> {
    let preview = service.reviewed_preview()?;
    let reviewed: Vec<&Extension> = preview.extensions.iter().map(|status| &status.extension).collect();
    let selected: Vec<&Extension> = service.selection().extensions().iter().collect();
    (matches!(service.registration_status(), RegistrationStatus::Ready { .. }) && reviewed == selected)
        .then_some(preview.review.clone())
}

fn applied_journal(status: &RegistrationStatus) -> Option<u64> {
    match status {
        RegistrationStatus::Applied { journal_id } | RegistrationStatus::RetainedRecovery { journal_id, .. } => {
            Some(*journal_id)
        }
        _ => None,
    }
}

/// Flattened list: group headers and rows of checkbox cells.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Line {
    Header(usize),
    Cells(Vec<u32>),
}

fn lines(selection: &Selection, columns: usize) -> Vec<Line> {
    let columns = columns.max(1);
    let mut out = Vec::new();
    for (g, group) in file_types::groups().iter().enumerate() {
        out.push(Line::Header(g));
        let cells: Vec<u32> = file_types::catalog()
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.group == *group)
            .map(|(index, _)| TOGGLE | index as u32)
            .collect();
        out.extend(cells.chunks(columns).map(|chunk| Line::Cells(chunk.to_vec())));
    }
    // Selected extensions outside the catalog: added by hand.
    let custom: Vec<u32> = selection
        .extensions()
        .iter()
        .enumerate()
        .filter(|(_, ext)| !file_types::catalog().iter().any(|entry| entry.extension == ext.as_str()))
        .map(|(index, _)| CUSTOM | index as u32)
        .collect();
    if !custom.is_empty() {
        out.push(Line::Header(usize::MAX));
        out.extend(custom.chunks(columns).map(|chunk| Line::Cells(chunk.to_vec())));
    }
    out
}

impl HostedPage for Page {
    fn title(&self) -> &'static str {
        "Register file types"
    }
    fn summary(&self) -> String {
        let (status, _) = status_text(self.service.registration_status());
        format!("Open supported files with Kwil from your file manager · {status}")
    }
    fn keywords(&self) -> &'static str {
        "register file types extensions associations open with default app explorer finder"
    }
    fn draw(
        &mut self,
        buf: &mut [u32],
        stride: usize,
        font: &mut font::UiFont,
        content: Rect,
        c: &Colors,
        s: &Sizes,
        hover: Option<u32>,
        hits: &mut Hits<Target>,
    ) {
        if content.h < s.control * 10 || content.w < s.control * 18 {
            self.draw_compact(buf, stride, font, content, c, s, hover, hits);
            return;
        }
        let hot = |code| hover == Some(code);
        let area = inset(content, s.pad);
        let mut y = area.y;
        // Platform truth first.
        for line in desktop::wrap(font, platform_note(), area.w, 2) {
            desktop::label(buf, stride, font, Rect { x: area.x, y, w: area.w, h: s.text }, &line, c.muted);
            y += s.text;
        }
        y += s.gap;
        // Presets.
        let labels: Vec<&str> = PRESETS.iter().map(|(_, label)| *label).collect();
        let bar = Rect { x: area.x, y, w: area.w, h: s.control };
        let current = PRESETS.iter().position(|(preset, _)| *preset == self.service.selection().preset());
        if let Some(segments) = desktop::segments(font, bar, &labels, s) {
            let hovered = (0..PRESETS.len()).find(|i| hot(PRESET | *i as u32));
            desktop::segmented(buf, stride, font, &segments, &labels, current, hovered, c, s);
            for (i, rect) in segments.iter().enumerate() {
                hits.push(*rect, Target::Hosted(PRESET | i as u32));
            }
        }
        y += s.control + s.gap;
        // Custom extension entry.
        let add_w = desktop::button_width(font, "Add", s);
        let field = Rect { x: area.x, y, w: area.w.saturating_sub(add_w + s.gap).min(s.control * 10), h: s.control };
        let add = Rect { x: field.x + field.w + s.gap, y, w: add_w, h: s.control };
        desktop::text_field(
            buf,
            stride,
            font,
            field,
            Field {
                text: &self.ui.custom,
                cursor: self.ui.custom.len(),
                placeholder: "Add an extension, like .cfg",
                focused: self.ui.field_focused,
                hovered: hot(FIELD),
                search: false,
                clear: false,
            },
            c,
            s,
        );
        hits.push(field, Target::Hosted(FIELD));
        desktop::button(buf, stride, font, add, "Add", Style::Secondary, hot(ADD), c, s);
        hits.push(add, Target::Hosted(ADD));
        y += s.control + s.gap;
        // Bottom bar: status, summary and the explicit actions.
        let bottom = Rect { x: area.x, y: (area.y + area.h).saturating_sub(s.control), w: area.w, h: s.control };
        let status = self.service.registration_status();
        let journal = applied_journal(status);
        let reviewed = review_is_current(&self.service);
        let mut x = bottom.x + bottom.w;
        let mut action = |x: &mut usize, label: &str, style, code| {
            let w = desktop::button_width(font, label, s);
            *x = x.saturating_sub(w);
            let r = Rect { x: *x, w, ..bottom };
            desktop::button(buf, stride, font, r, label, style, hot(code), c, s);
            hits.push(r, Target::Hosted(code));
            *x = x.saturating_sub(s.gap);
        };
        let pending = matches!(status, RegistrationStatus::Pending);
        if !pending {
            if journal.is_some() {
                action(&mut x, "Unregister", Style::Danger, UNREGISTER);
            } else {
                let label = if reviewed.is_some() { "Register selected" } else { "Review changes" };
                action(&mut x, label, Style::Primary, PRIMARY);
                if !self.ui.setup {
                    action(&mut x, "Skip", Style::Secondary, SKIP);
                }
            }
        }
        let selected = self.service.selection().extensions().len();
        let (state, attention) = status_text(status);
        let summary = format!("{selected} selected · {state}");
        let text = Rect { x: bottom.x, w: x.saturating_sub(bottom.x + s.gap), ..bottom };
        desktop::label(buf, stride, font, text, &summary, if attention { c.error } else { c.muted });
        // Review result or error, above the bar.
        let mut bottom_y = bottom.y.saturating_sub(s.gap);
        let message = self.ui.error.clone().map(|e| (e, true)).or_else(|| {
            let preview = self.service.reviewed_preview().filter(|_| reviewed.is_some())?;
            let changes = preview.extensions.iter().filter(|e| e.outcome == ExtensionOutcome::Planned).count();
            let unchanged = preview.extensions.len() - changes;
            let mut text = format!("Review: {changes} will be added, {unchanged} matched when reviewed.");
            if let Some(note) = preview.notes.first() {
                text.push(' ');
                text.push_str(note);
            }
            Some((text, false))
        });
        if let Some((message, error)) = message {
            let wrapped = desktop::wrap(font, &message, area.w, 2);
            bottom_y = bottom_y.saturating_sub(wrapped.len() * s.text);
            for (i, line) in wrapped.iter().enumerate() {
                let r = Rect { x: area.x, y: bottom_y + i * s.text, w: area.w, h: s.text };
                desktop::label(buf, stride, font, r, line, if error { c.error } else { c.fg });
            }
            bottom_y = bottom_y.saturating_sub(s.gap);
        }
        // Scrollable grid of the catalog, bounded to the visible lines.
        let list = Rect { x: area.x, y, w: area.w, h: bottom_y.saturating_sub(y) };
        let cell_w = (s.control * 4).max(1);
        let columns = (list.w.saturating_sub(s.thumb + s.gap) / cell_w).max(1);
        let row_h = s.nav;
        let all = lines(self.service.selection(), columns);
        let visible = (list.h / row_h).max(1);
        self.ui.scroll = self.ui.scroll.min(all.len().saturating_sub(visible));
        self.ui.rows = (visible, all.len());
        let statuses = self.service.extension_status();
        for (slot, line) in all.iter().skip(self.ui.scroll).take(visible).enumerate() {
            let r = Rect { x: list.x, y: list.y + slot * row_h, w: list.w.saturating_sub(s.thumb + s.gap), h: row_h };
            if r.y + r.h > list.y + list.h {
                break;
            }
            match line {
                Line::Header(group) => {
                    let title = file_types::groups().get(*group).map_or("Added by you", |g| group_title(*g));
                    let pen = desktop::label(buf, stride, font, Rect { w: r.w / 2, ..r }, title, c.fg);
                    if let Some(group) = file_types::groups().get(*group) {
                        let all_on = file_types::catalog().iter().filter(|e| e.group == *group).all(|e| {
                            Extension::parse(e.extension).is_ok_and(|x| self.service.selection().contains(&x))
                        });
                        let label = if all_on { "Clear group" } else { "Select group" };
                        let code = GROUP | *group as u32;
                        let w = desktop::button_width(font, label, s);
                        let b = Rect { x: pen + s.gap, y: r.y + (r.h - s.control) / 2, w, h: s.control };
                        desktop::button(buf, stride, font, b, label, Style::Subtle, hot(code), c, s);
                        hits.push(b, Target::Hosted(code));
                    }
                }
                Line::Cells(cells) => {
                    for (i, code) in cells.iter().enumerate() {
                        let cell = Rect { x: r.x + i * cell_w, w: cell_w - s.gap, ..r };
                        let extension = match code & MASK {
                            TOGGLE => file_types::catalog()
                                .get((code & !MASK) as usize)
                                .and_then(|e| Extension::parse(e.extension).ok()),
                            _ => self.service.selection().extensions().get((code & !MASK) as usize).cloned(),
                        };
                        let Some(extension) = extension else { continue };
                        let checked = self.service.selection().contains(&extension);
                        let text = format!(".{}", extension.as_str());
                        desktop::checkbox(buf, stride, font, cell, &text, checked, hot(*code), true, c, s);
                        if let Some(mark) = statuses
                            .iter()
                            .find(|status| status.extension == extension)
                            .and_then(|status| outcome_mark(status.outcome))
                        {
                            let center = ((cell.x + cell.w) as f32 - s.gap as f32, cell.y as f32 + cell.h as f32 / 2.0);
                            chrome::disc(
                                buf,
                                stride,
                                center,
                                s.gap as f32 / 3.0,
                                if mark { c.error } else { c.accent },
                            );
                        }
                        hits.push(cell, Target::Hosted(*code));
                    }
                }
            }
        }
        desktop::scroll_thumb(
            buf,
            stride,
            Rect { x: list.x + list.w - s.thumb - s.gap, y: list.y, w: s.thumb + s.gap, h: visible * row_h },
            self.ui.scroll,
            visible,
            all.len(),
            c,
            s,
        );
    }
}

impl Page {
    /// Small windows scroll the entire form, not just the extension grid. The
    /// explicit action stays in its own reserved row and cannot cover a field.
    #[allow(clippy::too_many_arguments)]
    fn draw_compact(
        &mut self,
        buf: &mut [u32],
        stride: usize,
        font: &mut font::UiFont,
        content: Rect,
        c: &Colors,
        s: &Sizes,
        hover: Option<u32>,
        hits: &mut Hits<Target>,
    ) {
        let area = inset(content, s.hairline * 2);
        if area.h < s.control {
            return;
        }
        let bottom = Rect { y: area.y + area.h - s.control, h: s.control, ..area };
        let top = Rect { h: bottom.y.saturating_sub(area.y + s.hairline * 2), ..area };
        let columns = (top.w.saturating_sub(s.thumb + s.gap) / (s.control * 3).max(1)).max(1);
        let rows = lines(self.service.selection(), columns);
        let total = rows.len() + 4;
        let visible = top.h / s.nav.max(1);
        self.ui.scroll = self.ui.scroll.min(total.saturating_sub(visible.max(1)));
        self.ui.rows = (visible.max(1), total);
        for slot in 0..visible {
            let index = self.ui.scroll + slot;
            if index >= total {
                break;
            }
            let row = Rect { x: top.x, y: top.y + slot * s.nav, w: top.w.saturating_sub(s.thumb + s.gap), h: s.nav };
            match index {
                0 => {
                    desktop::label(buf, stride, font, row, "Scroll for all types · Open with only", c.muted);
                }
                1 | 2 => {
                    let first = (index - 1) * 2;
                    for (i, (preset, _)) in PRESETS.iter().enumerate().skip(first).take(2) {
                        let width = row.w.saturating_sub(s.gap) / 2;
                        let r =
                            Rect { x: row.x + (i - first) * (width + s.gap), w: width, h: s.control.min(row.h), ..row };
                        let label = match i {
                            0 => "Common",
                            1 => "All text types",
                            2 => "Plain text",
                            _ => "Custom",
                        };
                        let code = PRESET | i as u32;
                        let style = if self.service.selection().preset() == *preset {
                            Style::Primary
                        } else {
                            Style::Secondary
                        };
                        desktop::button(buf, stride, font, r, label, style, hover == Some(code), c, s);
                        hits.push(r, Target::Hosted(code));
                    }
                }
                3 => {
                    let add_w = (s.control * 2).min(row.w / 3);
                    let field = Rect { w: row.w.saturating_sub(add_w + s.gap), h: s.control.min(row.h), ..row };
                    desktop::text_field(
                        buf,
                        stride,
                        font,
                        field,
                        Field {
                            text: &self.ui.custom,
                            cursor: self.ui.custom.len(),
                            placeholder: "Custom extension",
                            focused: self.ui.field_focused,
                            hovered: hover == Some(FIELD),
                            search: false,
                            clear: false,
                        },
                        c,
                        s,
                    );
                    hits.push(field, Target::Hosted(FIELD));
                    let add = Rect { x: field.x + field.w + s.gap, w: add_w, ..field };
                    desktop::button(buf, stride, font, add, "Add", Style::Secondary, hover == Some(ADD), c, s);
                    hits.push(add, Target::Hosted(ADD));
                }
                _ => match &rows[index - 4] {
                    Line::Header(group) => {
                        desktop::label(
                            buf,
                            stride,
                            font,
                            row,
                            file_types::groups().get(*group).map_or("Added by you", |g| group_title(*g)),
                            c.muted,
                        );
                    }
                    Line::Cells(cells) => {
                        for (i, code) in cells.iter().enumerate() {
                            let extension = if code & MASK == TOGGLE {
                                file_types::catalog()
                                    .get((code & !MASK) as usize)
                                    .and_then(|e| Extension::parse(e.extension).ok())
                            } else {
                                self.service.selection().extensions().get((code & !MASK) as usize).cloned()
                            };
                            if let Some(extension) = extension {
                                let width = row.w / columns;
                                let r = Rect { x: row.x + i * width, w: width.saturating_sub(s.gap), ..row };
                                desktop::checkbox(
                                    buf,
                                    stride,
                                    font,
                                    r,
                                    &format!(".{}", extension.as_str()),
                                    self.service.selection().contains(&extension),
                                    hover == Some(*code),
                                    true,
                                    c,
                                    s,
                                );
                                hits.push(r, Target::Hosted(*code));
                            }
                        }
                    }
                },
            }
        }
        desktop::scroll_thumb(
            buf,
            stride,
            Rect { x: area.x + area.w.saturating_sub(s.thumb), w: s.thumb, ..top },
            self.ui.scroll,
            visible,
            total,
            c,
            s,
        );
        let status = self.service.registration_status();
        if matches!(status, RegistrationStatus::Pending) {
            desktop::label(buf, stride, font, bottom, "Working…", c.muted);
            return;
        }
        let (code, label) = if applied_journal(status).is_some() {
            (UNREGISTER, "Unregister")
        } else if review_is_current(&self.service).is_some() {
            (PRIMARY, "Register selected")
        } else {
            (PRIMARY, "Review changes")
        };
        let width = desktop::button_width(font, label, s).min(bottom.w);
        let action = Rect { x: bottom.x + bottom.w - width, w: width, ..bottom };
        desktop::button(buf, stride, font, action, label, Style::Primary, hover == Some(code), c, s);
        hits.push(action, Target::Hosted(code));
        let summary = self.ui.error.clone().unwrap_or_else(|| {
            format!("{} selected · {}", self.service.selection().extensions().len(), status_text(status).0)
        });
        desktop::label(
            buf,
            stride,
            font,
            Rect { w: action.x.saturating_sub(bottom.x + s.gap), ..bottom },
            &summary,
            if self.ui.error.is_some() { c.error } else { c.muted },
        );
    }
    pub(super) fn custom_focused(&self) -> bool {
        self.ui.field_focused
    }
    pub(super) fn blur_custom(&mut self) {
        self.ui.field_focused = false;
    }
    /// One click; every action is a real service call or a local selection edit.
    pub(super) fn click(&mut self, code: u32) {
        self.ui.error = None;
        let result: Result<(), String> = match (code & MASK, code) {
            (PRESET, _) => {
                if let Some((preset, _)) = PRESETS.get((code & !MASK) as usize) {
                    // Custom keeps the current choices; the others replace them.
                    if *preset != Preset::Custom {
                        *self.service.selection_mut() = Selection::new(*preset);
                    } else {
                        if let Some(first) = self.service.selection().extensions().first().cloned() {
                            let _ = self.service.selection_mut().toggle(first, true);
                        }
                        self.ui.field_focused = true;
                        self.ui.scroll = 3;
                    }
                }
                Ok(())
            }
            (TOGGLE, _) => file_types::catalog()
                .get((code & !MASK) as usize)
                .and_then(|entry| Extension::parse(entry.extension).ok())
                .map_or(Ok(()), |extension| {
                    let on = !self.service.selection().contains(&extension);
                    self.service.selection_mut().toggle(extension, on).map_err(|_| "Too many types selected".into())
                }),
            (CUSTOM, _) => {
                let extension = self.service.selection().extensions().get((code & !MASK) as usize).cloned();
                extension.map_or(Ok(()), |extension| {
                    self.service.selection_mut().toggle(extension, false).map_err(|_| "Selection unchanged".into())
                })
            }
            (GROUP, _) => {
                let group = file_types::groups().get((code & !MASK) as usize).copied();
                let entries: Vec<Extension> = file_types::catalog()
                    .iter()
                    .filter(|entry| Some(entry.group) == group)
                    .filter_map(|entry| Extension::parse(entry.extension).ok())
                    .collect();
                let on = !entries.iter().all(|e| self.service.selection().contains(e));
                entries
                    .into_iter()
                    .try_for_each(|e| self.service.selection_mut().toggle(e, on))
                    .map_err(|_| "Too many types selected".into())
            }
            (_, FIELD) => {
                self.ui.field_focused = true;
                Ok(())
            }
            (_, ADD) => self.add_custom(),
            (_, PRIMARY) => match review_is_current(&self.service) {
                Some(review) => self.service.register_selected(review, None).map(drop).map_err(|e| error_text(&e)),
                None => self.service.preview().map(drop).map_err(|e| error_text(&e)),
            },
            (_, SKIP) => self.service.skip().map(drop).map_err(|e| error_text(&e)),
            (_, UNREGISTER) => match applied_journal(self.service.registration_status()) {
                Some(journal) => self.service.unregister(journal).map(drop).map_err(|e| error_text(&e)),
                None => Ok(()),
            },
            _ => Ok(()),
        };
        if code != FIELD && code != ADD && code != (PRESET | 3) {
            self.ui.field_focused = false;
        }
        self.ui.error = result.err();
    }

    fn add_custom(&mut self) -> Result<(), String> {
        let text = self.ui.custom.trim().to_owned();
        if text.is_empty() {
            return Ok(());
        }
        self.service.selection_mut().add_custom(&text).map_err(|error| {
            error_text(&ActionError {
                kind: ActionErrorKind::Extension(error),
                action: kwil_view_model::file_type_registration::RegistrationAction::Status,
            })
        })?;
        self.ui.custom.clear();
        Ok(())
    }

    /// Keys while the page is showing. Text goes only to the custom field;
    /// extensions are ASCII, so IME commits are not accepted here.
    pub(super) fn key(&mut self, key: &Key, text: Option<&str>, mods: ModifiersState) {
        self.ui.error = None;
        match key {
            Key::Named(NamedKey::Home) => self.ui.scroll = 0,
            Key::Named(NamedKey::End) => self.ui.scroll = usize::MAX,
            Key::Named(NamedKey::Enter) if !self.ui.custom.is_empty() => {
                self.ui.error = self.add_custom().err();
            }
            Key::Named(NamedKey::Backspace) => {
                self.ui.custom.pop();
            }
            Key::Named(NamedKey::ArrowDown | NamedKey::PageDown) => {
                let (visible, total) = self.ui.rows;
                self.ui.scroll = desktop::wheel(self.ui.scroll, false, visible, total);
            }
            Key::Named(NamedKey::ArrowUp | NamedKey::PageUp) => {
                let (visible, total) = self.ui.rows;
                self.ui.scroll = desktop::wheel(self.ui.scroll, true, visible, total);
            }
            _ if !(mods.control_key() || mods.super_key()) => {
                if let Some(text) = text.filter(|t| t.chars().all(|c| c.is_ascii_graphic())) {
                    self.ui.field_focused = true;
                    if self.ui.custom.len() + text.len() <= file_types::MAX_EXTENSION_BYTES + 1 {
                        self.ui.custom.push_str(text);
                    }
                }
            }
            _ => {}
        }
    }
}

impl App {
    pub(super) fn settings_hosted_available(&self) -> bool {
        self.file_types.is_some()
    }

    pub(super) fn settings_hosted_click(&mut self, code: u32) {
        if let Some(page) = &mut self.file_types {
            page.click(code);
        }
        self.settings_epoch = self.settings_epoch.wrapping_add(1);
        self.palette_redraw();
    }

    pub(super) fn settings_hosted_key(&mut self, event: &KeyEvent) {
        if event.state != ElementState::Pressed {
            return;
        }
        if let Some(page) = &mut self.file_types {
            page.key(&event.logical_key, event.text.as_deref(), self.mods);
        }
        self.settings_epoch = self.settings_epoch.wrapping_add(1);
        self.palette_redraw();
    }

    /// Opening the page asks the worker for the durable state first, so a
    /// restarted process shows an earlier registration before offering more.
    pub(super) fn settings_hosted_opened(&mut self) {
        if let Some(page) = &mut self.file_types {
            page.ui.error = page.service.status().err().map(|e| error_text(&e));
        }
    }

    /// Wheel over the hosted page.
    pub(super) fn settings_hosted_wheel(&mut self, up: bool) {
        if let Some(page) = &mut self.file_types {
            let (visible, total) = page.ui.rows;
            let next = desktop::wheel(page.ui.scroll, up, visible, total);
            if next != page.ui.scroll {
                page.ui.scroll = next;
                if self.welcome.visible() {
                    self.welcome_view.hits = desktop::Hits::default();
                    self.welcome_view.paint.request();
                } else {
                    self.settings_view.hover = None;
                    self.settings_view.paint.request();
                }
                self.request_frame();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_grid_lists_every_offered_extension_once_with_bounded_lines() {
        let selection = Selection::new(Preset::AllTextTypes);
        for columns in [1, 3, 7] {
            let all = lines(&selection, columns);
            let cells: Vec<u32> = all
                .iter()
                .filter_map(|line| match line {
                    Line::Cells(cells) => Some(cells.clone()),
                    Line::Header(_) => None,
                })
                .flatten()
                .collect();
            assert_eq!(cells.len(), file_types::catalog().len());
            assert!(all.iter().all(|line| !matches!(line, Line::Cells(c) if c.len() > columns)));
            let mut unique = cells.clone();
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(unique.len(), cells.len());
        }
        // Java, C/C++, headers and the common config/data/markup types are offered.
        for ext in ["java", "c", "cpp", "h", "hpp", "json", "toml", "yaml", "yml", "md"] {
            assert!(file_types::catalog().iter().any(|e| e.extension == ext), "{ext} missing");
        }
    }

    #[test]
    fn hand_added_extensions_get_their_own_section_and_codes() {
        let mut selection = Selection::default();
        selection.add_custom(".kwilcfg").unwrap();
        let all = lines(&selection, 4);
        assert!(all.contains(&Line::Header(usize::MAX)));
        assert!(all.iter().any(|line| matches!(line, Line::Cells(c) if c.contains(&CUSTOM))));
    }

    #[test]
    fn errors_never_claim_success_or_a_default_app() {
        let action = kwil_view_model::file_type_registration::RegistrationAction::Status;
        for kind in [
            ActionErrorKind::Busy,
            ActionErrorKind::EmptySelection,
            ActionErrorKind::UnadoptedCapacity { selected: 240, limit: 128 },
            ActionErrorKind::ExtendNotAdopted { journal_id: 3 },
            ActionErrorKind::StaleReview,
            ActionErrorKind::Exhausted,
        ] {
            let text = error_text(&ActionError { kind, action: action.clone() }).to_lowercase();
            assert!(!text.contains("registered for") && !text.contains("default app"), "{text}");
        }
        assert!(platform_note().contains("account"));
        if cfg!(windows) {
            assert!(platform_note().contains("Open with") && platform_note().contains("your choice"));
        }
    }
}
