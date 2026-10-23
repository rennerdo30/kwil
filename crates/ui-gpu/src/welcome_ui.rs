//! First-run desktop card. Input remains owned by setup until explicit completion.
use super::*;
use desktop::{Colors, Hits, Sizes, Style};
use settings_ui::{HostedPage, Target};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    Next,
    Back,
    Skip,
    FileType(u32),
}

#[derive(Default)]
pub(super) struct WelcomeView {
    pub paint: desktop::Paint,
    pub rows: (usize, usize),
    pub hits: Hits<Action>,
    pub area: Option<Rect>,
    pub scroll: usize,
    pub problem: Option<String>,
    pub focus: usize,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn draw(
    model: &welcome::Model,
    view: &mut WelcomeView,
    page: Option<&mut filetypes_ui::Page>,
    pixels: &mut [u32],
    size: (usize, usize),
    font: &mut font::UiFont,
    theme: &kwil_core_commands::theme::Theme,
    metrics: &chrome::Metrics,
) -> Option<Rect> {
    view.hits = Hits::default();
    if !model.visible() {
        view.area = None;
        return None;
    }
    let (w, h) = size;
    let s = Sizes::new(metrics);
    let c = Colors::new(theme);
    let margin = s.pad.min(w / 8).min(h / 8);
    let width = metrics.dip(840.0).min(w.saturating_sub(margin * 2));
    let height = metrics.dip(650.0).min(h.saturating_sub(margin * 2));
    let area = Rect { x: (w - width) / 2, y: (h - height) / 2, w: width, h: height };
    view.area = Some(area);
    desktop::card(pixels, w, area, &c, &s);
    let inner = desktop::inset(area, s.pad.min(width / 4));
    let footer_h = s.control.min(inner.h);
    let footer = Rect { y: inner.y + inner.h - footer_h, h: footer_h, ..inner };
    let title = if model.stage() == welcome::Stage::About { "Welcome to Kwil" } else { "Make Kwil yours" };
    let header = if h < metrics.dip(420.0) { s.text } else { s.header };
    desktop::label(pixels, w, font, Rect { h: header.min(inner.h), ..inner }, title, c.fg);
    let content_y = (inner.y + header).min(footer.y);
    let content = Rect { y: content_y, h: footer.y.saturating_sub(content_y + s.gap), ..inner };
    if model.stage() == welcome::Stage::About {
        let mut lines = Vec::new();
        for text in welcome::BENEFITS.iter().chain(welcome::TECHNOLOGY) {
            lines.extend(desktop::wrap(font, text, content.w, 3));
            lines.push(String::new());
        }
        let visible = content.h / s.text.max(1);
        view.rows = (visible, lines.len());
        view.scroll = view.scroll.min(lines.len().saturating_sub(visible));
        for (i, line) in lines.iter().skip(view.scroll).take(visible).enumerate() {
            desktop::label(pixels, w, font, Rect { y: content.y + i * s.text, h: s.text, ..content }, line, c.fg);
        }
        let button = Rect {
            x: footer.x + footer.w.saturating_sub(metrics.dip(180.0)),
            w: metrics.dip(180.0).min(footer.w),
            ..footer
        };
        desktop::button(pixels, w, font, button, "Choose file types →", Style::Primary, false, &c, &s);
        view.hits.push(button, Action::Next);
    } else {
        if let Some(page) = page {
            page.ui.setup = true;
            let mut hits = Hits::default();
            page.draw(pixels, w, font, content, &c, &s, None, &mut hits);
            for (rect, target) in hits.iter() {
                if let Target::Hosted(code) = target {
                    view.hits.push(*rect, Action::FileType(*code));
                }
            }
        } else {
            let text = view
                .problem
                .as_deref()
                .unwrap_or("Preparing file-type registration… You can also set this up later in Settings.");
            for (i, line) in desktop::wrap(font, text, content.w, 4).iter().enumerate() {
                desktop::label(
                    pixels,
                    w,
                    font,
                    Rect { y: content.y + i * s.text, h: s.text, ..content },
                    line,
                    c.muted,
                );
            }
        }
        let bw = metrics.dip(125.0).min(footer.w / 2);
        let back = Rect { w: bw, ..footer };
        desktop::button(pixels, w, font, back, "Back", Style::Secondary, false, &c, &s);
        view.hits.push(back, Action::Back);
        let skip = Rect { x: footer.x + footer.w - bw, w: bw, ..footer };
        let label = if model.stage() == welcome::Stage::Registering { "Working…" } else { "Skip for now" };
        desktop::button(pixels, w, font, skip, label, Style::Subtle, false, &c, &s);
        if model.stage() != welcome::Stage::Registering {
            view.hits.push(skip, Action::Skip);
        }
    }
    view.focus = view.focus.min(view.hits.iter().count().saturating_sub(1));
    if let Some((rect, _)) = view.hits.iter().nth(view.focus) {
        let t = s.focus.min(rect.w).min(rect.h);
        for edge in [
            Rect { h: t, ..*rect },
            Rect { y: rect.y + rect.h - t, h: t, ..*rect },
            Rect { w: t, ..*rect },
            Rect { x: rect.x + rect.w - t, w: t, ..*rect },
        ] {
            chrome::fill_clipped(pixels, w, edge, c.accent);
        }
    }
    Some(area)
}

impl App {
    /// One background preparation per App. Native setup is never started merely
    /// by displaying About; this is called after the user's Next/open-page action.
    pub(super) fn start_registration(&mut self) {
        if self.file_types.is_some() || self.registration_starting {
            return;
        }
        if let Some(preparation) = self.registration_prep.as_ref() {
            if !preparation.is_finished() {
                return;
            }
            let result = self.registration_prep.take().unwrap().join();
            if result.is_err() {
                self.welcome_view.problem = Some("Registration preparation failed".into());
                return;
            }
        }
        let Some(path) = self.registration_path.clone() else {
            self.welcome_view.problem = Some("Settings location is not ready yet. Try again shortly.".into());
            return;
        };
        let mode = if self.opts.hidden {
            registration_host::HostMode::HiddenHarness
        } else {
            registration_host::HostMode::Interactive
        };
        let proxy = self.proxy.clone();
        self.registration_starting = true;
        match std::thread::Builder::new().name("kwil-registration-prepare".into()).spawn(move || {
            let result = registration_host::prepare(&path, mode).and_then(|prepared| {
                let wake = proxy.clone();
                kwil_view_model::file_type_registration::FileTypeRegistration::start(
                    prepared.journal,
                    prepared.context,
                    prepared.factory,
                    std::sync::Arc::new(move || {
                        let _ = wake.send_event(UserEvent::RegistrationReady);
                    }),
                )
                .map(|mut service| {
                    let _ = service.status();
                    Box::new(service)
                })
                .map_err(|e| e.to_string())
            });
            let _ = proxy.send_event(UserEvent::RegistrationStarted(result));
        }) {
            Ok(thread) => self.registration_prep = Some(thread),
            Err(error) => {
                self.registration_starting = false;
                self.welcome_view.problem = Some(error.to_string());
            }
        }
    }

    /// Modal routing runs before all editor/menu input routes, including IME.
    pub(super) fn welcome_input(&mut self, event: &WindowEvent) -> bool {
        if !self.welcome.visible() {
            return false;
        }
        match event {
            WindowEvent::ModifiersChanged(modifiers) => {
                self.mods = modifiers.state();
                true
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer = (position.x.max(0.0) as usize, position.y.max(0.0) as usize);
                true
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                if let Some(action) = self.welcome_view.hits.at(self.pointer) {
                    self.welcome_action(action);
                }
                true
            }
            WindowEvent::MouseInput { .. } | WindowEvent::Touch(_) | WindowEvent::DroppedFile(_) => true,
            WindowEvent::MouseWheel { delta, .. } => {
                let up = match delta {
                    MouseScrollDelta::LineDelta(_, y) => *y > 0.0,
                    MouseScrollDelta::PixelDelta(p) => p.y > 0.0,
                };
                if self.welcome.stage() == welcome::Stage::About {
                    let (visible, total) = self.welcome_view.rows;
                    let next = desktop::wheel(self.welcome_view.scroll, up, visible, total);
                    if next != self.welcome_view.scroll {
                        self.welcome_view.scroll = next;
                        self.welcome_view.hits = Hits::default();
                        self.welcome_view.paint.request();
                        self.request_frame();
                    }
                } else {
                    self.settings_hosted_wheel(up);
                }
                true
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    self.welcome_key(&event.logical_key, event.text.as_deref());
                }
                true
            }
            WindowEvent::Ime(_) => {
                // Compositions are never rebound to the document under setup.
                self.composing = false;
                self.ime_destination = Some(ime::Destination::Cancelled);
                true
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                self.welcome_view.hits = Hits::default();
                false
            }
            _ => false,
        }
    }

    pub(super) fn welcome_key(&mut self, key: &Key, text: Option<&str>) {
        match key {
            Key::Named(NamedKey::Tab) => {
                if let Some(page) = &mut self.file_types {
                    page.blur_custom();
                }
                let count = self.welcome_view.hits.iter().count();
                if count > 0 {
                    self.welcome_view.focus = if self.mods.shift_key() {
                        (self.welcome_view.focus + count - 1) % count
                    } else {
                        (self.welcome_view.focus + 1) % count
                    };
                }
            }
            Key::Named(NamedKey::Enter | NamedKey::Space) => {
                if self.welcome.stage() == welcome::Stage::FileTypes
                    && self.file_types.as_ref().is_some_and(|page| page.custom_focused())
                {
                    if let Some(page) = &mut self.file_types {
                        page.key(key, text, self.mods);
                    }
                    self.welcome_view.hits = Hits::default();
                } else {
                    let action = self.welcome_view.hits.iter().nth(self.welcome_view.focus).map(|(_, action)| *action);
                    if let Some(action) = action {
                        self.welcome_action(action);
                    }
                }
            }
            Key::Named(NamedKey::Escape) => {
                if self.welcome.stage() == welcome::Stage::About {
                    self.welcome_action(Action::Next);
                } else if self.welcome.stage() == welcome::Stage::FileTypes {
                    self.welcome_action(Action::Skip);
                }
            }
            Key::Named(NamedKey::ArrowDown | NamedKey::PageDown) if self.welcome.stage() == welcome::Stage::About => {
                self.welcome_view.scroll = self.welcome_view.scroll.saturating_add(2);
            }
            Key::Named(NamedKey::ArrowUp | NamedKey::PageUp) if self.welcome.stage() == welcome::Stage::About => {
                self.welcome_view.scroll = self.welcome_view.scroll.saturating_sub(2);
            }
            _ if self.welcome.stage() == welcome::Stage::FileTypes => {
                if let Some(page) = &mut self.file_types {
                    page.key(key, text, self.mods);
                }
                self.welcome_view.hits = Hits::default();
            }
            _ => {}
        }

        self.welcome_view.paint.request();
        self.request_frame();
    }

    pub(super) fn welcome_action(&mut self, action: Action) {
        match action {
            Action::Next => {
                self.welcome.next();
                self.start_registration();
            }
            Action::Back => {
                self.welcome.back();
                if let Some(page) = &mut self.file_types {
                    page.blur_custom();
                }
            }
            Action::Skip => self.welcome.skip_registration(),
            Action::FileType(code) => {
                if let Some(page) = &mut self.file_types {
                    page.click(code);
                }
                self.welcome_registration_changed();
            }
        }
        if let Some(worker) = &mut self.welcome_worker
            && let Err(error) = self.welcome.persist_completion(worker)
        {
            self.welcome_view.problem = Some(error.to_string());
        }
        self.drag = None;
        self.welcome_view.focus = 0;
        self.welcome_view.hits = Hits::default();
        if !self.welcome.visible() {
            let _ = self.proxy.send_event(UserEvent::OpenExternal);
        }
        self.selecting = false;
        self.composing = false;
        self.ime_destination = Some(ime::Destination::Cancelled);
        self.apply(Damage { full: true, status: true, ..Default::default() });
    }

    pub(super) fn welcome_registration_changed(&mut self) {
        let Some(page) = &self.file_types else { return };
        let was_visible = self.welcome.visible();
        registration_progress(&mut self.welcome, page.service.registration_status());
        if let Some(worker) = &mut self.welcome_worker
            && let Err(error) = self.welcome.persist_completion(worker)
        {
            self.welcome_view.problem = Some(error.to_string());
        }
        if was_visible && !self.welcome.visible() {
            let _ = self.proxy.send_event(UserEvent::OpenExternal);
        }
    }
}

fn registration_progress(
    model: &mut welcome::Model,
    status: &kwil_view_model::file_type_registration::RegistrationStatus,
) {
    use kwil_view_model::file_type_registration::RegistrationStatus;
    match status {
        RegistrationStatus::Pending if model.stage() == welcome::Stage::FileTypes => {
            model.begin_registration();
        }
        RegistrationStatus::Applied { .. } if model.stage() == welcome::Stage::Registering => {
            model.registration_finished()
        }
        RegistrationStatus::Skipped => model.skip_registration(),
        status if !matches!(status, RegistrationStatus::Pending) => model.registration_failed(),
        _ => {}
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use kwil_view_model::file_type_registration::RegistrationStatus;
    #[test]
    fn welcome_nonpending_undo_recovery_cancel_and_failure_restore_navigation_without_completion() {
        for status in [
            RegistrationStatus::Unregistered { journal_id: 1 },
            RegistrationStatus::RetainedRecovery { journal_id: 1, transactions_left: 31 },
            RegistrationStatus::Cancelled,
            RegistrationStatus::NoRetainedReceipt { transactions_left: 32 },
            RegistrationStatus::Failed { message: "denied".into(), recovery: Some(1) },
        ] {
            let mut model = welcome::Model::default();
            model.ready(true);
            model.next();
            registration_progress(&mut model, &RegistrationStatus::Pending);
            assert_eq!(model.stage(), welcome::Stage::Registering);
            registration_progress(&mut model, &status);
            assert_eq!(model.stage(), welcome::Stage::FileTypes, "{status:?}");
            model.back();
            assert_eq!(model.stage(), welcome::Stage::About);
        }
    }
    #[test]
    fn welcome_only_explicit_skip_and_applied_registration_complete_setup() {
        for status in [RegistrationStatus::Applied { journal_id: 1 }, RegistrationStatus::Skipped] {
            let mut model = welcome::Model::default();
            model.ready(true);
            model.next();
            model.begin_registration();
            if matches!(status, RegistrationStatus::Skipped) {
                model.registration_failed();
            }
            registration_progress(&mut model, &status);
            assert_eq!(model.stage(), welcome::Stage::Complete);
        }
    }
}
