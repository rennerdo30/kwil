//! Dev-only, opt-in finite script. No timers or polling exist in the shipping UI.
use super::{App, Damage, UserEvent};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};

#[derive(Clone, Copy, Debug)]
pub(crate) enum Step {
    Idle,
    Scroll,
    Repaint,
    Status,
    RemoteOn,
    RemoteOff,
    MenuOpen,
    MenuClose,
    ZoomIn,
    InputOwnership,
    UsabilityCapture(&'static str),
    /// Desktop overlays (settings, palette) captured in the hidden window.
    SettingsOpen,
    SettingsKeymap,
    SettingsCapture,
    SettingsClose,
    PaletteOpen,
    PaletteClose,
    WelcomeProbe,
    WelcomeNext,
    WelcomeKeyboard,
    WelcomeScrollDamage,
    WelcomeSkip,
    WelcomeSmall,
    WelcomeSmallCheck,
    WelcomeLateInput,
    RecentMiddleClick,
    DismissSetup,
    ResizeProbe,
    DesktopWindowProbe,
    DesktopSettingsProbe,
    ThemeLight,
    ThemeDark,
    FindOpen,
    FindNext,
    FindCheck,
    End,
}
#[derive(Default, Clone, Copy)]
struct Counts {
    presents: u64,
    full: u64,
    damage: u64,
    copy: u64,
}
#[derive(Default)]
pub(crate) struct Metrics {
    phase: usize,
    per: [Counts; 5],
}
impl Metrics {
    pub fn present(&mut self, full: bool, damage: u64, copy: u64) {
        let c = &mut self.per[self.phase];
        c.presents += 1;
        c.full += u64::from(full);
        c.damage += damage;
        c.copy += copy;
    }
    fn print(&self) {
        for (i, name) in ["settling", "idle", "scroll", "repaint", "status"].iter().enumerate() {
            let c = self.per[i];
            println!(
                "kwil-frame phase={name} presents={} full={} damage_px={} copy_px={}",
                c.presents, c.full, c.damage, c.copy
            );
        }
    }
}
pub(crate) fn start(proxy: EventLoopProxy<UserEvent>) {
    std::thread::spawn(move || {
        let usual = [
            (1800, Step::DismissSetup),
            (200, Step::Idle),
            (250, Step::Scroll),
            (200, Step::Repaint),
            (150, Step::Status),
            (150, Step::RemoteOn),
            (100, Step::RemoteOff),
            (100, Step::End),
        ];
        let usability = [
            (1800, Step::DismissSetup),
            (200, Step::Idle),
            (100, Step::InputOwnership),
            (100, Step::MenuOpen),
            (150, Step::UsabilityCapture("menu.bmp")),
            (100, Step::MenuClose),
            (100, Step::ZoomIn),
            (200, Step::ZoomIn),
            (200, Step::ZoomIn),
            (400, Step::UsabilityCapture("zoom.bmp")),
            // Zoom stays at its raised level: overlay text and sizes must not follow it.
            (100, Step::SettingsOpen),
            (300, Step::UsabilityCapture("settings.bmp")),
            (100, Step::SettingsKeymap),
            (300, Step::UsabilityCapture("settings-keymap.bmp")),
            (100, Step::SettingsCapture),
            (300, Step::UsabilityCapture("settings-capture.bmp")),
            (100, Step::SettingsClose),
            (100, Step::PaletteOpen),
            (300, Step::UsabilityCapture("palette.bmp")),
            (100, Step::PaletteClose),
            (100, Step::End),
        ];
        let welcome = [
            (2000, Step::WelcomeProbe),
            (100, Step::UsabilityCapture("welcome.bmp")),
            (100, Step::WelcomeNext),
            (100, Step::WelcomeKeyboard),
            (100, Step::WelcomeScrollDamage),
            (300, Step::UsabilityCapture("welcome-file-types.bmp")),
            (100, Step::WelcomeSmall),
            (200, Step::WelcomeSmallCheck),
            (100, Step::UsabilityCapture("welcome-small.bmp")),
            (100, Step::WelcomeSkip),
            (100, Step::WelcomeLateInput),
            (100, Step::RecentMiddleClick),
            (200, Step::UsabilityCapture("welcome-complete.bmp")),
            (100, Step::End),
        ];
        let resize = [(1800, Step::DismissSetup), (200, Step::ResizeProbe), (100, Step::End)];
        let desktop = [
            (1800, Step::WelcomeProbe),
            (100, Step::DesktopWindowProbe),
            (100, Step::UsabilityCapture("desktop-welcome.bmp")),
            (100, Step::WelcomeNext),
            (150, Step::UsabilityCapture("desktop-file-types.bmp")),
            (100, Step::WelcomeSkip),
            (100, Step::SettingsOpen),
            (200, Step::DesktopSettingsProbe),
            (100, Step::UsabilityCapture("desktop-settings-dark.bmp")),
            (100, Step::ThemeLight),
            (400, Step::UsabilityCapture("desktop-settings-light.bmp")),
            (100, Step::ThemeDark),
            (400, Step::UsabilityCapture("desktop-theme-dropdown.bmp")),
            (100, Step::SettingsClose),
            (100, Step::ResizeProbe),
            (100, Step::UsabilityCapture("desktop-editor.bmp")),
            (100, Step::End),
        ];
        let find = [
            (1800, Step::DismissSetup),
            (200, Step::FindOpen),
            (400, Step::FindNext),
            (300, Step::FindCheck),
            (100, Step::UsabilityCapture("find.bmp")),
            (100, Step::End),
        ];
        let steps = if std::env::var_os("KWIL_FIND_PROBE").is_some() {
            find.as_slice()
        } else if std::env::var_os("KWIL_DESKTOP_WINDOW_PROBE").is_some() {
            desktop.as_slice()
        } else if std::env::var_os("KWIL_RESIZE_PROBE").is_some() {
            resize.as_slice()
        } else if std::env::var_os("KWIL_WELCOME_PROBE").is_some() {
            welcome.as_slice()
        } else if std::env::var_os("KWIL_UI_USABILITY_CAPTURE").is_some() {
            usability.as_slice()
        } else {
            usual.as_slice()
        };
        for &(delay, step) in steps {
            std::thread::sleep(Duration::from_millis(delay));
            if proxy.send_event(UserEvent::Harness(step)).is_err() {
                break;
            }
        }
    });
}
pub(crate) fn step(app: &mut App, el: &ActiveEventLoop, step: Step) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| step_inner(app, el, step))) {
        if let Some(dir) = std::env::var_os("KWIL_CAPTURE_DIR") {
            let text = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("non-string panic");
            let _ = std::fs::write(PathBuf::from(dir).join("harness-failure.txt"), format!("{step:?}: {text}"));
        }
        std::panic::resume_unwind(payload);
    }
}
fn step_inner(app: &mut App, el: &ActiveEventLoop, step: Step) {
    let dir = std::env::var_os("KWIL_CAPTURE_DIR").map(PathBuf::from);
    match step {
        Step::DesktopWindowProbe => {
            app.sync_desktop_window(el);
            let main = app.gfx.as_ref().unwrap().window.id();
            let dialog = app.desktop_window.as_ref().expect("welcome is a separate desktop window");
            assert_ne!(dialog.gfx.window.id(), main);
            assert_eq!(dialog.kind, super::desktop_window::Kind::Welcome);
            assert!(app.render_desktop_window());
            if let Some(dir) = &dir {
                std::fs::write(
                    dir.join("desktop-windows.txt"),
                    "PASS: actual owned welcome window has a distinct WindowId and native surface\n",
                )
                .unwrap();
            }
        }
        Step::DesktopSettingsProbe => {
            use kwil_core_commands::settings_ui::Category;
            app.sync_desktop_window(el);
            assert_eq!(app.desktop_window.as_ref().unwrap().kind, super::desktop_window::Kind::Settings);
            if let Some(host) = &mut app.settings_panel {
                host.set_category(Some(Category::Appearance));
            }
            app.settings_epoch = app.settings_epoch.wrapping_add(1);
            assert!(app.render_desktop_window());
            assert!(app.settings_view.choice_area("theme.name", None).is_some(), "theme is a real dropdown");
            assert!(!app.settings_view.has_internal_close(), "the native window owns the only close button");
            if let Some(dir) = &dir {
                std::fs::write(
                    dir.join("desktop-settings.txt"),
                    "PASS: separate owned Settings window; theme dropdown painted\n",
                )
                .unwrap();
            }
        }
        Step::FindOpen => {
            use winit::keyboard::{Key, ModifiersState};
            let press = |app: &mut App, key: Key, text: Option<&str>, mods| {
                app.mods = mods;
                app.key_after_settings(Some(el), &super::HostKey { logical_key: key, text, repeat: false });
            };
            press(app, Key::Character("f".into()), None, ModifiersState::CONTROL);
            assert!(app.search_panel.is_open(), "real Ctrl+F route opens Find");
            press(app, Key::Character("value".into()), Some("value"), ModifiersState::empty());
            assert_eq!(app.search_panel.text(kwil_view_model::search_panel::SearchField::Query).0, "value");
        }
        Step::FindNext => {
            assert_eq!(app.search_panel.count(), (60, true), "worker completes the actual fixture scan");
            app.mods = winit::keyboard::ModifiersState::empty();
            app.key_after_settings(
                Some(el),
                &super::HostKey {
                    logical_key: winit::keyboard::Key::Named(winit::keyboard::NamedKey::Enter),
                    text: None,
                    repeat: false,
                },
            );
        }
        Step::FindCheck => {
            let cursor = app.view.editor().unwrap().primary_cursor().clone();
            assert_eq!(cursor.head.abs_diff(cursor.anchor), 5, "Enter selects the actual match");
            assert!(app.render());
            if let Some(dir) = std::env::var_os("KWIL_CAPTURE_DIR").map(PathBuf::from) {
                std::fs::write(
                    dir.join("find-result.txt"),
                    format!("PASS Ctrl+F query=value count=60 exact=true Enter selection={cursor:?}\n"),
                )
                .unwrap();
            }
        }
        Step::ThemeLight => {
            choose_theme(app, el, "light");
        }
        Step::ThemeDark => {
            assert_eq!(
                super::theme_ui::chrome().scheme(),
                kwil_core_commands::theme::Scheme::Light,
                "light selection changes the actual renderer"
            );
            choose_theme(app, el, "dark");
        }
        Step::ResizeProbe => {
            use winit::application::ApplicationHandler;
            use winit::event::WindowEvent;
            let id = app.gfx.as_ref().unwrap().window.id();
            let original = app.size;
            let mut event_ms = Vec::new();
            let mut paint_ms = Vec::new();
            for (w, h) in [(1080, 700), (1180, 740), (1280, 800), (1380, 840), (1480, 900), original] {
                let start = std::time::Instant::now();
                app.window_event(el, id, WindowEvent::Resized(winit::dpi::PhysicalSize::new(w as u32, h as u32)));
                event_ms.push(start.elapsed().as_secs_f64() * 1000.0);
                let start = std::time::Instant::now();
                assert!(app.render());
                paint_ms.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            let text = format!("six bounded App resize events; event_ms={event_ms:?}; paint_ms={paint_ms:?}\n");
            println!("{text}");
            if let Some(dir) = &dir {
                std::fs::write(dir.join("resize-timing.txt"), text).unwrap();
            }
        }
        Step::WelcomeScrollDamage => {
            assert!(app.render());
            let phase = app.metrics.phase;
            let full = app.metrics.per[phase].full;
            app.settings_hosted_wheel(false);
            assert!(!app.pending.full, "file types scroll must not request a full frame");
            assert!(app.welcome_view.paint.pending());
            assert!(app.render());
            assert_eq!(app.metrics.per[phase].full, full, "file types scroll presents only its card");
            app.settings_hosted_wheel(true);
            assert!(app.render());
            let presents = app.metrics.per[phase].presents;
            app.settings_hosted_wheel(true);
            assert!(!app.overlay_paint_pending(), "wheel at top bound must not request a repaint");
            assert!(app.render());
            assert_eq!(app.metrics.per[phase].presents, presents);
        }
        Step::RecentMiddleClick => {
            use winit::application::ApplicationHandler;
            use winit::event::{DeviceId, ElementState, MouseButton, WindowEvent};
            assert!(app.dispatch_file("file.recent"));
            let generation = app.view.file_prompt().unwrap().generation;
            let ids = super::tabs::ids(&app.view);
            let frame = app.tab_frame();
            let slot = &frame.geometry.slots[0];
            app.pointer = (slot.area.x + 1, slot.area.y + 1);
            let id = app.gfx.as_ref().unwrap().window.id();
            app.window_event(
                el,
                id,
                WindowEvent::MouseInput {
                    device_id: DeviceId::dummy(),
                    state: ElementState::Pressed,
                    button: MouseButton::Middle,
                },
            );
            assert_eq!(super::tabs::ids(&app.view), ids, "Recent must retain every tab");
            assert_eq!(app.view.file_prompt().unwrap().generation, generation, "middle click cannot cancel Recent");
            assert_eq!(app.view.file_prompt().unwrap().kind, kwil_view_model::FilePromptKind::Recent);
            super::file_prompt::cancel(&mut app.view);
            app.sync_file_ui();
        }
        Step::DismissSetup => {
            if app.welcome.stage() == super::welcome::Stage::About {
                app.welcome_action(super::welcome_ui::Action::Next);
            }
            if app.welcome.stage() == super::welcome::Stage::FileTypes {
                app.welcome_action(super::welcome_ui::Action::Skip);
            }
        }
        Step::WelcomeSmall => {
            let _ = app.gfx.as_ref().unwrap().window.request_inner_size(winit::dpi::PhysicalSize::new(520u32, 300u32));
        }
        Step::WelcomeSmallCheck => {
            assert!(app.render());
            let hits: Vec<_> = app.welcome_view.hits.iter().copied().collect();
            assert!(
                hits.iter().any(|(_, action)| *action == super::welcome_ui::Action::FileType(1)),
                "small window retains registration action"
            );
            for (i, (a, _)) in hits.iter().enumerate() {
                for (b, _) in &hits[i + 1..] {
                    assert!(
                        a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y,
                        "setup controls must not overlap: {a:?}, {b:?}"
                    );
                }
            }
        }
        Step::WelcomeLateInput => {
            use winit::application::ApplicationHandler;
            use winit::event::{Ime, WindowEvent};
            let id = app.gfx.as_ref().unwrap().window.id();
            let revision = app.view.editor().unwrap().document().revision();
            app.window_event(el, id, WindowEvent::Ime(Ime::Commit("old setup commit".into())));
            assert_eq!(app.view.editor().unwrap().document().revision(), revision);
            app.window_event(el, id, WindowEvent::Ime(Ime::Preedit("new".into(), None)));
            assert!(app.composing);
            app.user_event(el, UserEvent::WelcomeReady);
            assert!(app.composing, "late setup acknowledgment must preserve new editor composition");
            app.window_event(el, id, WindowEvent::Ime(Ime::Commit("new".into())));
            assert!(app.view.editor().unwrap().document().revision() > revision);
        }
        Step::WelcomeProbe => welcome_probe(app, el),
        Step::WelcomeNext => {
            app.welcome_action(super::welcome_ui::Action::Next);
            assert_eq!(app.welcome.stage(), super::welcome::Stage::FileTypes);
        }
        Step::WelcomeKeyboard => {
            use winit::keyboard::{Key, NamedKey};
            assert!(app.render());
            app.welcome_key(&Key::Character("zzzkwil".into()), Some("zzzkwil"));
            app.welcome_key(&Key::Named(NamedKey::Enter), None);
            if let Some(page) = &mut app.file_types {
                page.key(&Key::Named(NamedKey::End), None, app.mods);
            }
            assert!(app.render());
            app.welcome_key(&Key::Character("aaakwil".into()), Some("aaakwil"));
            assert!(app.render());
            let old=app.welcome_view.hits.iter().find(|(_,action)|matches!(action,super::welcome_ui::Action::FileType(code) if code&0xf000_0000==0x3000_0000)).map(|(rect,_)|*rect).expect("painted custom row");
            app.welcome_key(&Key::Named(NamedKey::Enter), None);
            assert_eq!(app.welcome_view.hits.iter().count(), 0, "custom Enter invalidates old sorted indices");
            app.pointer = (old.x + 1, old.y + 1);
            use winit::application::ApplicationHandler;
            app.window_event(
                el,
                app.gfx.as_ref().unwrap().window.id(),
                winit::event::WindowEvent::MouseInput {
                    device_id: winit::event::DeviceId::dummy(),
                    state: winit::event::ElementState::Pressed,
                    button: winit::event::MouseButton::Left,
                },
            );
            for name in ["aaakwil", "zzzkwil"] {
                assert!(
                    app.file_types
                        .as_ref()
                        .unwrap()
                        .service
                        .selection()
                        .contains(&kwil_core_commands::file_types::Extension::parse(name).unwrap())
                );
            }
            app.welcome_action(super::welcome_ui::Action::Back);
            assert_eq!(app.welcome.stage(), super::welcome::Stage::About);
            assert!(app.render());
            app.welcome_key(&Key::Named(NamedKey::Enter), None);
            assert_eq!(
                app.welcome.stage(),
                super::welcome::Stage::FileTypes,
                "hidden custom focus cannot intercept Next"
            );
            *app.file_types.as_mut().unwrap().service.selection_mut() = kwil_core_commands::file_types::Selection::new(
                kwil_core_commands::file_types::Preset::CommonTextConfig,
            );
            app.file_types.as_mut().unwrap().key(&Key::Named(NamedKey::Home), None, app.mods);
            app.apply(Damage { full: true, ..Default::default() });
        }
        Step::WelcomeSkip => {
            app.welcome_action(super::welcome_ui::Action::Skip);
            assert!(!app.welcome.visible());
            assert_eq!(app.welcome.stage(), super::welcome::Stage::Complete);
        }
        Step::Idle => {
            // Prime the retained GPU image without an onscreen present, so the
            // following scroll/readback exercises GPU copies, not a fresh upload.
            #[cfg(feature = "gpu")]
            if let (Some(gpu), Some(gfx), Some(font)) = (&mut app.gpu, &app.gfx, &app.font) {
                gpu.update(
                    &gfx.pixels,
                    app.size,
                    &[super::Rect { x: 0, y: 0, w: app.size.0, h: app.size.1 }],
                    0,
                    font.ch,
                    app.view.rows(),
                );
            }
            #[cfg(windows)]
            if let Some(gfx) = &app.gfx {
                native_probe(gfx.hwnd, dir.as_deref());
            }
            println!("kwil-frame glyphs_pending={}", app.font.as_ref().map_or(0, |f| f.pending_glyphs()));
            let unicode_enabled = app.font.as_ref().is_some_and(|f| f.unicode_enabled());
            println!("kwil-frame unicode_enabled={unicode_enabled}");
            if std::env::var_os("KWIL_REQUIRE_UNICODE").is_some() {
                assert!(unicode_enabled, "Unicode recovery unavailable");
            }
            app.metrics.phase = 1;
            capture(app, dir.as_deref(), "first.bmp");
        }
        Step::Scroll => {
            app.metrics.phase = 2;
            let d = app.view.scroll(3);
            app.apply(d);
        }
        Step::Repaint => {
            capture(app, dir.as_deref(), "scroll.bmp");
            #[cfg(feature = "gpu")]
            if let (Some(gpu), Some(gfx)) = (&app.gpu, &app.gfx) {
                println!("kwil-gpu backing_pixels_equal_cpu={:?}", gpu.matches(&gfx.pixels));
            }
            app.metrics.phase = 3;
            app.apply(Damage { full: true, ..Default::default() });
        }
        Step::Status => {
            capture(app, dir.as_deref(), "repaint.bmp");
            if let Some(dir) = &dir {
                let a = std::fs::read(dir.join("scroll.bmp"));
                let b = std::fs::read(dir.join("repaint.bmp"));
                println!("kwil-frame scroll_copy_matches_repaint={}", a.is_ok() && a.ok() == b.ok());
            }
            app.metrics.phase = 4;
            let d = app.view.set_notice("status damage probe".into());
            app.apply(d);
        }
        Step::RemoteOn | Step::RemoteOff => {
            #[cfg(windows)]
            if let Some(gfx) = &app.gfx {
                unsafe {
                    use windows_sys::Win32::UI::WindowsAndMessaging::{
                        SendMessageW, WM_WTSSESSION_CHANGE, WTS_CONSOLE_CONNECT, WTS_REMOTE_CONNECT,
                    };
                    SendMessageW(
                        gfx.hwnd as _,
                        WM_WTSSESSION_CHANGE,
                        if matches!(step, Step::RemoteOn) { WTS_REMOTE_CONNECT } else { WTS_CONSOLE_CONNECT } as usize,
                        0,
                    );
                }
            }
            #[cfg(not(windows))]
            {
                let _ = app.proxy.send_event(UserEvent::SessionChanged(matches!(step, Step::RemoteOn)));
            }
        }
        Step::MenuOpen => {
            let available = app.menu_available();
            app.menu_set(super::menu::State::open(0, &|id| super::menu::enabled(&available, id)), true);
        }
        Step::InputOwnership => {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| input_ownership(app, el, dir.as_deref())));
            if let Err(payload) = result {
                if let Some(dir) = &dir {
                    let message = payload
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .or_else(|| payload.downcast_ref::<&str>().copied())
                        .unwrap_or("non-string assertion payload");
                    let _ = std::fs::write(dir.join("input-ownership-failure.txt"), message);
                }
                std::panic::resume_unwind(payload);
            }
        }
        Step::MenuClose => app.menu_set(super::menu::State::Closed, false),
        Step::SettingsOpen => {
            app.dispatch_settings("view.settings");
        }
        Step::SettingsKeymap => {
            // The same host calls a click on Keymap -> Custom bindings -> Edit makes.
            if let Some(host) = app.settings_panel.as_mut() {
                host.set_category(Some(kwil_core_commands::settings_ui::Category::Keymap));
                if let Some(index) = host.rows().iter().position(|row| row.spec.id == "bindings") {
                    host.select(index);
                    let _ = host.input(kwil_view_model::settings_session::SettingsInput::Accept);
                }
            }
            app.settings_epoch = app.settings_epoch.wrapping_add(1);
            app.palette_redraw();
        }
        Step::SettingsCapture => {
            if let Some(host) = app.settings_panel.as_mut() {
                let _ = host.panel_input(kwil_view_model::settings_panels::PanelInput::Accept);
            }
            app.settings_epoch = app.settings_epoch.wrapping_add(1);
            app.palette_redraw();
        }
        Step::SettingsClose => {
            if let Some(host) = app.settings_panel.as_mut() {
                // Cancel the capture, leave the page, then close: no edit is applied.
                let _ = host.panel_input(kwil_view_model::settings_panels::PanelInput::Cancel);
                let _ = host.panel_input(kwil_view_model::settings_panels::PanelInput::Cancel);
                host.close();
            }
            app.settings_epoch = app.settings_epoch.wrapping_add(1);
            app.palette_redraw();
        }
        Step::PaletteOpen => {
            app.dispatch_palette("view.commandPalette");
        }
        Step::PaletteClose => app.palette_input(kwil_view_model::palette_session::PaletteInput::Cancel),
        Step::ZoomIn => {
            app.dispatch_zoom("view.zoomIn");
        }
        Step::UsabilityCapture(name) => {
            let metadata = format!(
                "kwil-ui capture={name} scale={} menu={} tabs={} status={} bar={} thumb={} zoom={} editor_cell={:?}",
                app.ui.scale,
                app.ui.menu,
                app.ui.tabs,
                app.ui.status,
                app.ui.bar,
                app.ui.min_thumb,
                app.zoom.percent(),
                app.font.as_ref().map(|font| (font.cw, font.ch)),
            );
            println!("{metadata}");
            if let Some(dir) = &dir {
                std::fs::write(dir.join(format!("{name}.txt")), metadata).expect("capture metadata write");
            }
            capture(app, dir.as_deref(), name);
        }
        Step::End => {
            capture(app, dir.as_deref(), "status.bmp");
            app.metrics.print();
            println!(
                "kwil-frame final_remote={} cursor_blink_allowed={}",
                app.policy.remote(),
                app.policy.cursor_blink_allowed()
            );
            el.exit();
        }
    }
}
fn welcome_probe(app: &mut App, el: &ActiveEventLoop) {
    use kwil_core_commands::associations::{
        AssociationAdapter, AssociationError, AssociationPlatform, DefaultChoice, Location, StoredValue,
    };
    use kwil_view_model::file_type_registration::{FileTypeRegistration, RegistrationContext, RegistrationLimits};
    use winit::application::ApplicationHandler;
    use winit::event::{ElementState, Ime, MouseButton, WindowEvent};
    assert!(app.opts.hidden);
    assert_eq!(app.welcome.stage(), super::welcome::Stage::About, "debug launch must show setup");
    let before = app.view.editor().map(|e| (e.document().revision(), e.cursors().to_vec()));
    let id = app.gfx.as_ref().unwrap().window.id();
    app.window_event(el, id, WindowEvent::Ime(Ime::Preedit("日本語".into(), None)));
    app.window_event(el, id, WindowEvent::Ime(Ime::Commit("must not edit".into())));
    app.pointer = (1, 1);
    app.window_event(
        el,
        id,
        WindowEvent::MouseInput {
            device_id: winit::event::DeviceId::dummy(),
            state: ElementState::Pressed,
            button: MouseButton::Left,
        },
    );
    assert_eq!(app.view.editor().map(|e| (e.document().revision(), e.cursors().to_vec())), before);
    struct NoNative;
    impl AssociationAdapter for NoNative {
        fn read_bounded(
            &mut self,
            _: &Location,
            _: &mut kwil_core_commands::associations::ValueBudget,
        ) -> Result<Option<StoredValue>, AssociationError> {
            Ok(None)
        }
        fn owner(&self) -> String {
            "isolated-welcome-probe".into()
        }
        fn read(&mut self, _: &Location) -> Result<Option<StoredValue>, AssociationError> {
            Ok(None)
        }
        fn write(&mut self, _: &Location, _: Option<&StoredValue>) -> Result<(), AssociationError> {
            panic!("welcome display/skip must not register")
        }
    }
    let root = PathBuf::from(std::env::var_os("KWIL_WORKSPACE_ROOT").expect("isolated fixture"));
    assert!(root.is_absolute());
    let context = RegistrationContext {
        platform: AssociationPlatform::Windows,
        executable: root.join("kwil.exe"),
        content_types: Default::default(),
        default_choice: DefaultChoice::KeepCurrent,
        limits: RegistrationLimits::Base128,
    };
    let proxy = app.proxy.clone();
    let mut service = FileTypeRegistration::start(
        root.join("fake-association-journal"),
        context,
        || Ok(NoNative),
        std::sync::Arc::new(move || {
            let _ = proxy.send_event(UserEvent::RegistrationReady);
        }),
    )
    .unwrap();
    *service.selection_mut() =
        kwil_core_commands::file_types::Selection::new(kwil_core_commands::file_types::Preset::CommonTextConfig);
    app.file_types = Some(super::filetypes_ui::Page { service, ui: Default::default() });
    app.apply(Damage { full: true, ..Default::default() });
    println!(
        "kwil-welcome PASS debug visibility, modal IME/click ownership; explicit fake adapter, no native association writes"
    );
}

fn input_ownership(app: &mut App, el: &ActiveEventLoop, dir: Option<&Path>) {
    use winit::application::ApplicationHandler;
    use winit::event::{ElementState, Ime, MouseButton, WindowEvent};
    let window = app.gfx.as_ref().unwrap().window.id();
    let arm = |app: &mut App| {
        let body = app.body_area();
        let offset = app.view.top();
        let bar = super::scrollbar::Bar::sized(0, body.height, 40, 20_000_000, offset, 48);
        app.drag = Some(super::scrollbar::Drag::new(true, bar, bar.thumb_start + 20, offset));
    };
    // Exercise the real App routes, not only the integer geometry object.
    let scale = app.ui.scale;
    arm(app);
    app.monitor_scale_changed(scale + 0.25);
    assert!(app.drag.is_none(), "DPI change must release the old coordinate anchor");
    app.monitor_scale_changed(scale);
    let settings = app.settings.clone();
    let mut changed = settings.clone();
    changed.tabs.layout = match settings.tabs.layout {
        kwil_core_commands::settings::TabLayout::Horizontal => kwil_core_commands::settings::TabLayout::Vertical,
        kwil_core_commands::settings::TabLayout::Vertical => kwil_core_commands::settings::TabLayout::Horizontal,
    };
    arm(app);
    app.user_event(el, UserEvent::SettingsReady(Box::new(changed)));
    assert!(app.drag.is_none(), "layout change must release the old coordinate anchor");
    app.user_event(el, UserEvent::SettingsReady(Box::new(settings)));
    let size = app.size;
    arm(app);
    app.window_event(el, window, WindowEvent::Resized(winit::dpi::PhysicalSize::new(size.0 as u32 + 1, size.1 as u32)));
    assert!(app.drag.is_none(), "resize must release the old coordinate anchor");
    app.window_event(el, window, WindowEvent::Resized(winit::dpi::PhysicalSize::new(size.0 as u32, size.1 as u32)));

    let before = app.view.editor().map(|editor| (editor.document().revision(), editor.cursors().to_vec()));
    // Find is now enabled: the menu must retain click ownership through cancelled preedit.
    assert!(super::menu::enabled(&app.menu_available(), "search.find"));
    app.menu_set(super::menu::State::Open { menu: 4, item: None, keyboard: false }, true);
    assert!(app.render());
    let row = app.menu.dropdown.as_ref().unwrap().rows[0].1;
    app.window_event(el, window, WindowEvent::Ime(Ime::Preedit("日本語".into(), None)));
    assert!(!app.composing, "cancelled composition cannot suspend menu ownership");
    app.pointer = (row.x + 2, row.y + 2);
    app.window_event(
        el,
        window,
        WindowEvent::MouseInput {
            device_id: winit::event::DeviceId::dummy(),
            state: ElementState::Pressed,
            button: MouseButton::Left,
        },
    );
    assert!(app.search_panel.is_open(), "the menu click opens Find without editing the document");
    app.window_event(el, window, WindowEvent::Ime(Ime::Commit("unexpected edit".into())));
    assert_eq!(app.view.editor().map(|editor| (editor.document().revision(), editor.cursors().to_vec())), before);
    app.menu_set(super::menu::State::Closed, false);
    app.close_search();
    if let Some(dir) = dir {
        std::fs::write(dir.join("input-ownership.txt"), "PASS: App DPI/layout/resize drag cancellation; open menu Preedit/click/Commit retains document and cursors\n").unwrap();
    }
}

fn choose_theme(app: &mut App, el: &ActiveEventLoop, value: &str) {
    use winit::{
        application::ApplicationHandler,
        dpi::PhysicalPosition,
        event::{ElementState, MouseButton, WindowEvent},
    };
    fn click(app: &mut App, el: &ActiveEventLoop, rect: super::Rect) {
        let id = app.desktop_window.as_ref().unwrap().gfx.window.id();
        app.window_event(
            el,
            id,
            WindowEvent::CursorMoved {
                device_id: winit::event::DeviceId::dummy(),
                position: PhysicalPosition::new((rect.x + rect.w / 2) as f64, (rect.y + rect.h / 2) as f64),
            },
        );
        app.window_event(
            el,
            id,
            WindowEvent::MouseInput {
                device_id: winit::event::DeviceId::dummy(),
                state: ElementState::Pressed,
                button: MouseButton::Left,
            },
        );
    }
    assert!(app.render_desktop_window());
    let before = app.view.editor().map(|e| (e.document().revision(), e.cursors().to_vec()));
    click(app, el, app.settings_view.choice_area("theme.name", None).expect("theme combobox"));
    assert!(app.render_desktop_window());
    let rect = app.settings_view.choice_area("theme.name", Some(value)).expect("theme dropdown option");
    click(app, el, rect);
    assert_eq!(
        app.view.editor().map(|e| (e.document().revision(), e.cursors().to_vec())),
        before,
        "settings clicks cannot edit the document"
    );
}

fn capture(app: &App, dir: Option<&Path>, name: &str) {
    if let (Some(dir), Some(gfx)) = (dir, app.desktop_window.as_ref().map(|d| &d.gfx).or(app.gfx.as_ref())) {
        #[cfg(windows)]
        {
            if let Err(error) = capture_dc(gfx.hwnd, &dir.join(name), false) {
                eprintln!("capture failed: {error}");
            }
            if std::env::var_os("KWIL_CAPTURE_WINDOW_FRAME").is_some()
                && let Err(error) = capture_dc(gfx.hwnd, &dir.join(format!("frame-{name}")), true)
            {
                eprintln!("window frame capture failed: {error}");
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (dir, gfx, name);
        }
    }
}
#[cfg(windows)]
fn native_probe(hwnd: isize, output: Option<&Path>) {
    use windows_sys::Win32::{
        Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
        UI::WindowsAndMessaging::*,
    };
    unsafe {
        let hwnd = hwnd as _;
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let required = WS_CAPTION | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX;
        let controls = style & required == required;
        let small = SendMessageW(hwnd, WM_GETICON, ICON_SMALL as usize, 0) != 0;
        let big = SendMessageW(hwnd, WM_GETICON, ICON_BIG as usize, 0) != 0;
        let mut cloaked = 0u32;
        let hr = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED as u32, (&mut cloaked as *mut u32).cast(), 4);
        let metadata = format!(
            "kwil-native caption_resize_min_max={controls} small_icon={small} taskbar_icon={big} cloaked={}",
            hr >= 0 && cloaked != 0
        );
        println!("{metadata}");
        if let Some(output) = output {
            std::fs::write(output.join("native-window.txt"), metadata).expect("native window metadata write");
        }
    }
}
// Based on the admitted M0 window-DC capture; the HWND comes directly from the
// owned window, never FindWindow/title guessing. No GPU/PrintWindow claim here.
#[cfg(windows)]
fn capture_dc(hwnd: isize, path: &Path, frame: bool) -> std::io::Result<()> {
    use windows_sys::Win32::{
        Foundation::RECT,
        Graphics::Gdi::*,
        UI::WindowsAndMessaging::{GetClientRect, GetWindowRect},
    };
    unsafe {
        let hwnd = hwnd as _;
        if frame {
            use windows_sys::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
            let mut cloaked = 0u32;
            let result = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED as u32, (&mut cloaked as *mut u32).cast(), 4);
            if result < 0 || cloaked != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "native caption pixels are unavailable from a DWM-cloaked window",
                ));
            }
        }
        let mut r: RECT = std::mem::zeroed();
        let measured = if frame { GetWindowRect(hwnd, &mut r) } else { GetClientRect(hwnd, &mut r) };
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if measured == 0 || w <= 0 || h <= 0 {
            return Err(std::io::Error::other("window capture bounds unavailable"));
        }
        let dc = if frame { GetWindowDC(hwnd) } else { GetDC(hwnd) };
        let memory = CreateCompatibleDC(dc);
        let bitmap = CreateCompatibleBitmap(dc, w, h);
        let old = SelectObject(memory, bitmap);
        let copied = BitBlt(memory, 0, 0, w, h, dc, 0, 0, SRCCOPY) != 0;
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = 40;
        info.bmiHeader.biWidth = w;
        info.bmiHeader.biHeight = h;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 24;
        let stride = ((w * 3 + 3) & !3) as usize;
        let mut pixels = vec![0u8; stride * h as usize];
        SelectObject(memory, old);
        let lines = GetDIBits(memory, bitmap, 0, h as u32, pixels.as_mut_ptr().cast(), &mut info, DIB_RGB_COLORS);
        DeleteObject(bitmap);
        DeleteDC(memory);
        ReleaseDC(hwnd, dc);
        if !copied || lines != h {
            return Err(std::io::Error::other("window DC capture failed"));
        }
        let mut file = Vec::with_capacity(54 + pixels.len());
        file.extend_from_slice(b"BM");
        file.extend_from_slice(&((54 + pixels.len()) as u32).to_le_bytes());
        file.extend_from_slice(&[0; 4]);
        file.extend_from_slice(&54u32.to_le_bytes());
        file.extend_from_slice(&40u32.to_le_bytes());
        file.extend_from_slice(&w.to_le_bytes());
        file.extend_from_slice(&h.to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&24u16.to_le_bytes());
        file.extend_from_slice(&[0; 24]);
        file.extend_from_slice(&pixels);
        std::fs::write(path, file)
    }
}
