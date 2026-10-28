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
    let width = w;
    let height = h;
    let area = Rect { x: (w - width) / 2, y: (h - height) / 2, w: width, h: height };
    view.area = Some(area);
    chrome::fill_clipped(pixels, w, area, c.surface);
    let inner = desktop::inset(area, metrics.dip(32.0).min(width / 4));
    let footer_h = s.control.min(inner.h);
    let footer = Rect { y: inner.y + inner.h - footer_h, h: footer_h, ..inner };
    let title = if model.stage() == welcome::Stage::About { "Welcome to Kwil" } else { "Make Kwil yours" };
    let header = if model.stage() == welcome::Stage::About {
        0
    } else if h < metrics.dip(420.0) {
        s.text
    } else {
        s.header
    };
    if header > 0 {
        desktop::label(pixels, w, font.heading(), Rect { h: header.min(inner.h), ..inner }, title, c.fg);
    }
    let content_y = (inner.y + header).min(footer.y);
    let content = Rect { y: content_y, h: footer.y.saturating_sub(content_y + s.gap), ..inner };
    if model.stage() == welcome::Stage::About {
        draw_about(pixels, w, font, content, &c, &s, metrics);
        view.rows = (1, 1);
        view.scroll = 0;
        let button = Rect {
            x: footer.x + footer.w.saturating_sub(metrics.dip(180.0)),
            w: metrics.dip(180.0).min(footer.w),
            ..footer
        };
        desktop::button(pixels, w, font, button, "Continue", Style::Primary, false, &c, &s);
        view.hits.push(button, Action::Next);
        let skip = Rect { w: metrics.dip(120.0).min(footer.w / 3), ..footer };
        desktop::button(pixels, w, font, skip, "Not now", Style::Subtle, false, &c, &s);
        view.hits.push(skip, Action::Skip);
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

fn draw_about(
    pixels: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    area: Rect,
    c: &Colors,
    s: &Sizes,
    m: &chrome::Metrics,
) {
    let compact = area.h < m.dip(430.0);
    let mut y = area.y;
    if !compact {
        let icon_size = m.dip(48.0);
        let rgba = include_bytes!("icon.rgba");
        for dy in 0..icon_size {
            for dx in 0..icon_size {
                let i = ((dy * 32 / icon_size) * 32 + dx * 32 / icon_size) * 4;
                if let Some(pixel) = pixels.get_mut((y + dy) * stride + area.x + dx) {
                    let color = (u32::from(rgba[i]) << 16) | (u32::from(rgba[i + 1]) << 8) | u32::from(rgba[i + 2]);
                    *pixel = font::blend(*pixel, color, rgba[i + 3]);
                }
            }
        }
        desktop::label(
            pixels,
            stride,
            font,
            Rect { x: area.x + icon_size + s.pad, y, w: area.w.saturating_sub(icon_size + s.pad), h: icon_size },
            "KWIL",
            c.muted,
        );
        y += icon_size + m.dip(20.0);
    }
    desktop::label(
        pixels,
        stride,
        font.heading(),
        Rect { y, h: m.dip(46.0), ..area },
        "Make room for your text.",
        c.fg,
    );
    y += m.dip(48.0);
    let subtitle = "A fast, focused place for notes, code and big files. Open your work and get straight to it.";
    for line in desktop::wrap(font, subtitle, area.w, 2) {
        desktop::label(pixels, stride, font, Rect { y, h: s.text, ..area }, &line, c.muted);
        y += s.text;
    }
    y += m.dip(if compact { 12.0 } else { 24.0 });
    let features = [
        ("Big files. No heavy lifting.", welcome::BENEFITS[1]),
        ("A little more focus.", welcome::BENEFITS[2]),
        ("Your work stays yours.", welcome::BENEFITS[3]),
    ];
    if compact {
        for (title, _) in features {
            desktop::label(pixels, stride, font, Rect { y, h: s.text, ..area }, title, c.fg);
            y += s.text + m.dip(5.0);
        }
    } else {
        let gap = m.dip(12.0);
        let width = area.w.saturating_sub(2 * gap) / 3;
        let height = m.dip(126.0);
        for (i, (title, detail)) in features.iter().enumerate() {
            let card = Rect { x: area.x + i * (width + gap), y, w: width, h: height };
            desktop::row(pixels, stride, card, false, false, c, s);
            let content = desktop::inset(card, m.dip(14.0));
            chrome::fill_clipped(
                pixels,
                stride,
                Rect { x: content.x, y: content.y, w: m.dip(22.0), h: m.dip(3.0) },
                c.accent,
            );
            let mut line_y = content.y + m.dip(12.0);
            for line in desktop::wrap(font, title, content.w, 2) {
                desktop::label(pixels, stride, font, Rect { y: line_y, h: s.text, ..content }, &line, c.fg);
                line_y += s.text;
            }
            line_y += m.dip(6.0);
            for line in desktop::wrap(font, detail, content.w, 3) {
                if line_y + s.text <= card.y + card.h {
                    desktop::label(pixels, stride, font, Rect { y: line_y, h: s.text, ..content }, &line, c.muted);
                    line_y += s.text;
                }
            }
        }
        y += height;
    }
    y += m.dip(if compact { 8.0 } else { 20.0 });
    if y + s.text * 3 <= area.y + area.h {
        desktop::label(pixels, stride, font, Rect { y, h: s.text, ..area }, welcome::TECHNOLOGY[0], c.accent);
        y += s.text + m.dip(4.0);
        let detail = "Rust underneath. Shared piece-tree text, sparse indexes and bounded workers. Native, event-driven rendering redraws only what changes.";
        for line in desktop::wrap(font, detail, area.w, 3) {
            if y + s.text <= area.y + area.h {
                desktop::label(pixels, stride, font, Rect { y, h: s.text, ..area }, &line, c.muted);
                y += s.text;
            }
        }
    }
}

struct RegistrationNotificationProducer {
    path: std::path::PathBuf,
    mode: registration_host::HostMode,
    proxy: HostProxy,
    transfer: notification::ProducerTransfer,
}
impl RegistrationNotificationProducer {
    fn run(self) {
        let Self { path, mode, proxy, transfer } = self;
        transfer.start();
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
            .map_err(|error| error.to_string())
        });
        if result.is_err() {
            transfer.no_owner_created();
        }
        let _ = proxy.send_event(UserEvent::RegistrationStarted(transfer.publish(result)));
    }
}

impl App {
    pub(super) fn adopt_registration_notification(
        &mut self,
        mut result: notification::Delivery<
            Result<Box<kwil_view_model::file_type_registration::FileTypeRegistration>, String>,
        >,
    ) -> Damage {
        if !result.matches(&self.proxy, notification::SlotKind::Registration) {
            self.proxy.fail(notification::Reason::Identity);
            return Damage::default();
        }
        self.welcome_view.hits = Default::default();
        self.registration_starting = false;
        match result.value_mut() {
            Ok(service) => {
                service.worker_ready();
                let ui = Default::default();
                if let Err(reason) = result.adopt_into(&mut self.file_types, |result| {
                    let Ok(service) = result else { unreachable!() };
                    filetypes_ui::Page { service: *service, ui }
                }) {
                    self.proxy.fail(reason);
                }
            }
            Err(_) => {
                if let Err(reason) = result.consume_error(|result| {
                    let Err(error) = result else { unreachable!() };
                    self.view.set_notice(format!("File-type setup: {error}"));
                    self.welcome_view.problem = Some(error);
                }) {
                    self.proxy.fail(reason);
                }
            }
        }
        Damage { full: true, status: true, ..Default::default() }
    }
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
        let transfer = match proxy.reserve(
            notification::SlotKind::Registration,
            2 * std::mem::size_of::<RegistrationNotificationProducer>()
                + 2 * std::mem::size_of::<
                    notification::Delivery<
                        Result<Box<kwil_view_model::file_type_registration::FileTypeRegistration>, String>,
                    >,
                >(),
        ) {
            Ok(transfer) => transfer,
            Err(reason) => {
                proxy.fail(reason);
                self.welcome_view.problem = Some(format!("Registration notification admission: {reason:?}"));
                return;
            }
        };
        self.registration_starting = true;
        let capture = RegistrationNotificationProducer { path, mode, proxy, transfer };
        match std::thread::Builder::new().name("kwil-registration-prepare".into()).spawn(move || capture.run()) {
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
