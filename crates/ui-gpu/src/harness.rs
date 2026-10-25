//! Dev-only, opt-in finite script. No timers or polling exist in the shipping UI.
use super::{App, Damage, UserEvent};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use winit::application::ApplicationHandler;
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};

static HOLD_TEXT_COMPLETION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static HELD_TEXT_COMPLETION: std::sync::Mutex<
    Option<kwil_view_model::document_worker::result_transport::DocumentDeliveryId>,
> = std::sync::Mutex::new(None);

pub(super) fn hold_preview_completion(
    info: Option<kwil_view_model::workspace_document::ResultInfo>,
    active: kwil_view_model::SessionDocumentId,
) -> bool {
    use kwil_view_model::workspace_document::ResultKind;
    use std::sync::atomic::Ordering;
    let Some(info) = info else { return false };
    let mut held = HELD_TEXT_COMPLETION.try_lock().expect("finite harness scalar gate");
    if held.is_some() {
        assert_eq!(*held, info.delivery(), "held original slot cannot be replaced");
        return true;
    }
    if info.kind == ResultKind::Opened
        && info.document == Some(active)
        && HOLD_TEXT_COMPLETION.swap(false, Ordering::AcqRel)
    {
        *held = Some(info.delivery().expect("original bounded delivery identity"));
        true
    } else {
        false
    }
}
#[derive(Clone, Copy, Debug)]
pub(crate) enum Step {
    MarkdownSplit,
    MarkdownSection(u64),
    MarkdownNarrow,
    ContextProbe,
    ContextClose,
    GzipOpen,
    GzipCheck,
    LanguageOpen,
    LanguageChoose,
    LanguageReset,
    HexOpen,
    HexCheck,
    HexClose,
    HexRestored,
    BinaryOpen,
    BinaryOffer,
    BinaryHex,
    BinaryHexCheck,
    BinaryColdCheck,
    BinaryColdCancelCheck,
    BinaryText,
    BinaryTextCheck,
    BinaryPreviewCheck,
    BinaryReleasePreview,
    BinaryCancel,
    BinaryCancelCheck,
    BinaryClose,
    LogsOpen,
    LogsCheck,
    LogsClose,
    FolderOpen,
    FolderCheck,
    FolderSelect,
    FolderOpened,
    PresenterCpu,
    PresenterEdit,
    PresenterGpu,
    PresenterCheck,
    PresenterRemote,
    PresenterRemoteCheck,
    PresenterLocal,
    GpuFallback,
    GpuFallbackCheck,
    MarkdownPreview,
    MarkdownMenu,
    MarkdownMenuCheck,
    AboutOpen,
    AboutCheck,
    AboutClose,
    MarkdownText,
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
    PlacementSet,
    PlacementCheck,
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
        let placement =
            [(1800, Step::DismissSetup), (200, Step::PlacementSet), (350, Step::PlacementCheck), (100, Step::End)];
        let replay = [(1800, Step::DismissSetup), (250, Step::PlacementCheck), (100, Step::End)];
        let markdown = [
            (1800, Step::DismissSetup),
            (200, Step::MarkdownSplit),
            (500, Step::UsabilityCapture("markdown-split.bmp")),
            (100, Step::MarkdownMenu),
            (100, Step::MarkdownMenuCheck),
            (100, Step::UsabilityCapture("markdown-menu.bmp")),
            (100, Step::MenuClose),
            (100, Step::MarkdownPreview),
            (150, Step::UsabilityCapture("markdown-preview.bmp")),
            (100, Step::MarkdownSection(0)),
            (100, Step::UsabilityCapture("md-00.bmp")),
            (100, Step::MarkdownSection(12)),
            (100, Step::UsabilityCapture("md-12.bmp")),
            (100, Step::MarkdownSection(25)),
            (100, Step::UsabilityCapture("md-25.bmp")),
            (100, Step::MarkdownSection(31)),
            (100, Step::UsabilityCapture("md-31.bmp")),
            (100, Step::MarkdownSection(39)),
            (100, Step::UsabilityCapture("md-39.bmp")),
            (100, Step::MarkdownSection(47)),
            (100, Step::UsabilityCapture("md-47.bmp")),
            (100, Step::MarkdownSection(65)),
            (100, Step::UsabilityCapture("md-65.bmp")),
            (100, Step::MarkdownSection(69)),
            (100, Step::UsabilityCapture("md-69.bmp")),
            (100, Step::MarkdownSection(77)),
            (100, Step::UsabilityCapture("md-77.bmp")),
            (100, Step::MarkdownSection(86)),
            (100, Step::UsabilityCapture("md-86.bmp")),
            (100, Step::MarkdownNarrow),
            (150, Step::MarkdownSection(31)),
            (100, Step::UsabilityCapture("md-narrow-31.bmp")),
            (100, Step::MarkdownSection(69)),
            (100, Step::UsabilityCapture("md-narrow-69.bmp")),
            (100, Step::MarkdownSection(77)),
            (100, Step::UsabilityCapture("md-narrow-77.bmp")),
            (100, Step::MarkdownSection(86)),
            (100, Step::UsabilityCapture("md-narrow-86.bmp")),
            (100, Step::MarkdownText),
            (100, Step::ContextProbe),
            (100, Step::UsabilityCapture("context-menu.bmp")),
            (100, Step::ContextClose),
            (100, Step::UsabilityCapture("markdown-text.bmp")),
            (100, Step::Idle),
            (150, Step::Scroll),
            (150, Step::Repaint),
            (100, Step::AboutOpen),
            (200, Step::AboutCheck),
            (100, Step::UsabilityCapture("about.bmp")),
            (100, Step::AboutClose),
            (100, Step::GzipOpen),
            (400, Step::GzipCheck),
            (100, Step::LanguageOpen),
            (100, Step::UsabilityCapture("language-picker.bmp")),
            (100, Step::LanguageChoose),
            (100, Step::LanguageReset),
            (100, Step::HexOpen),
            (500, Step::HexCheck),
            (100, Step::UsabilityCapture("raw-hex.bmp")),
            (100, Step::HexClose),
            (500, Step::HexRestored),
            (100, Step::LogsOpen),
            (300, Step::LogsCheck),
            (100, Step::UsabilityCapture("inline-log-filter.bmp")),
            (100, Step::LogsClose),
            (100, Step::FolderOpen),
            (400, Step::FolderCheck),
            (100, Step::UsabilityCapture("folder-search.bmp")),
            (100, Step::FolderSelect),
            (400, Step::FolderOpened),
            (100, Step::PresenterCpu),
            (100, Step::PresenterEdit),
            (100, Step::PresenterGpu),
            (200, Step::PresenterCheck),
            (100, Step::PresenterRemote),
            (150, Step::PresenterRemoteCheck),
            (100, Step::PresenterLocal),
            (200, Step::GpuFallback),
            (150, Step::GpuFallbackCheck),
            (100, Step::End),
        ];
        let binary = [
            (1800, Step::DismissSetup),
            (100, Step::BinaryOpen),
            (500, Step::BinaryOffer),
            (100, Step::UsabilityCapture("binary-offer.bmp")),
            (100, Step::BinaryHex),
            (500, Step::BinaryHexCheck),
            (100, Step::UsabilityCapture("binary-hex.bmp")),
            (100, Step::BinaryClose),
            (500, Step::BinaryOpen),
            (500, Step::BinaryOffer),
            (100, Step::BinaryText),
            (500, Step::BinaryPreviewCheck),
            (100, Step::UsabilityCapture("binary-text-preview.bmp")),
            (100, Step::BinaryReleasePreview),
            (500, Step::BinaryTextCheck),
            (100, Step::BinaryClose),
            (500, Step::BinaryOpen),
            (500, Step::BinaryOffer),
            (100, Step::BinaryCancel),
            (500, Step::BinaryCancelCheck),
            (100, Step::End),
        ];
        let binary_cold = [
            (1800, Step::DismissSetup),
            (500, Step::BinaryOffer),
            (100, Step::UsabilityCapture("binary-cold-offer.bmp")),
            (100, Step::BinaryCancel),
            (500, Step::BinaryColdCancelCheck),
            (100, Step::BinaryOpen),
            (500, Step::BinaryOffer),
            (100, Step::BinaryHex),
            (500, Step::BinaryHexCheck),
            (100, Step::BinaryColdCheck),
            (100, Step::End),
        ];
        let steps = if std::env::var_os("KWIL_BINARY_COLD_START").is_some() {
            binary_cold.as_slice()
        } else if std::env::var_os("KWIL_BINARY_OFFER_PROBE").is_some() {
            binary.as_slice()
        } else if std::env::var_os("KWIL_MARKDOWN_PROBE").is_some() {
            markdown.as_slice()
        } else if std::env::var_os("KWIL_PLACEMENT_REPLAY").is_some() {
            replay.as_slice()
        } else if std::env::var_os("KWIL_PLACEMENT_PROBE").is_some() {
            placement.as_slice()
        } else if std::env::var_os("KWIL_FIND_PROBE").is_some() {
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
        Step::BinaryOpen => {
            assert!(app.opts.hidden, "binary harness never admits visible OS dialogs");
            let path = PathBuf::from(
                std::env::var_os("KWIL_BINARY_OFFER_FILE").expect("ROOT supplies a tiny isolated binary fixture"),
            );
            assert!(path.is_absolute());
            app.view.open_path(path).unwrap();
            app.sync_file_ui();
            app.apply(Damage { full: true, status: true, ..Default::default() });
        }
        Step::BinaryOffer => {
            let offer = app.view.binary_offer().expect("finite detection step must reach an offer");
            assert!(offer.len > 0);
            assert!(app.view.editor().is_none());
            assert!(app.view.line(0).unwrap_or_default().is_empty());
            assert!(app.file_modal());
            assert!(!app.confirmation_worker_busy());
        }
        Step::BinaryHex | Step::BinaryText | Step::BinaryCancel => {
            if matches!(step, Step::BinaryText) {
                HOLD_TEXT_COMPLETION.store(true, std::sync::atomic::Ordering::Release);
            }
            let key = match step {
                Step::BinaryHex => super::Key::Named(super::NamedKey::Enter),
                Step::BinaryText => super::Key::Character("t".into()),
                _ => super::Key::Named(super::NamedKey::Escape),
            };
            app.mods = super::ModifiersState::empty();
            assert!(app.binary_key(&super::HostKey { logical_key: key, text: None, repeat: false }));
            assert!(app.close_dialog.is_none());
            assert!(app.view.binary_offer().is_none());
        }
        Step::BinaryHexCheck => {
            assert!(app.view.hex_active());
            assert!(!app.binary_pending());
            assert!(!app.view.document_busy(app.view.active_id()));
            assert!(app.view.hex_session().unwrap().row(app.view.hex_source(), 0).is_some());
        }
        Step::BinaryColdCheck => {
            let row = app.view.hex_session().unwrap().row(app.view.hex_source(), 0).unwrap();
            assert_eq!(row.bytes, [0, 1, 0xfe, 0xff, 0x41]);
            if let Some(dir) = &dir {
                std::fs::write(
                    dir.join("binary-cold-result.txt"),
                    "PASS: cold-start binary bytes reach offer and opens exact physical bytes as Hex",
                )
                .unwrap();
            }
        }
        Step::BinaryColdCancelCheck => {
            assert!(!app.binary_pending(), "cancelled first CLI file must not be delivered twice");
            assert!(app.view.editor().is_some());
            assert!(
                app.view
                    .session()
                    .tabs()
                    .iter()
                    .all(|tab| { tab.path.as_ref().is_none_or(|p| p.file_name().unwrap() != "binary-cold.bin") })
            );
            assert!(!app.external_inbox.has_pending());
            if let Some(dir) = &dir {
                std::fs::write(
                    dir.join("binary-cold-cancel-result.txt"),
                    "PASS: initial delivery callback does not reopen a cancelled first CLI binary",
                )
                .unwrap();
            }
        }
        Step::BinaryTextCheck => {
            assert!(!app.view.hex_active());
            assert!(!app.binary_pending());
            assert!(app.view.editor().is_some());
        }
        Step::BinaryPreviewCheck => {
            assert!(HELD_TEXT_COMPLETION.try_lock().unwrap().is_some());
            assert_eq!(
                *HELD_TEXT_COMPLETION.try_lock().unwrap(),
                app.document.live.transport.as_ref().unwrap().result_info().unwrap().delivery()
            );
            assert!(app.view.editor().is_none());
            assert!(app.binary_pending());
            assert!(!app.binary_blocks_paint());
            assert!(app.view.line(0).unwrap().starts_with(b"header"));
            app.apply(Damage { full: true, ..Default::default() });
            assert!(app.render(), "actual GUI render paints while final completion is held");
        }
        Step::BinaryReleasePreview => {
            let delivery = HELD_TEXT_COMPLETION.try_lock().unwrap().take().unwrap();
            let transport = app.document.live.transport.as_ref().unwrap();
            assert_eq!(transport.result_info().unwrap().delivery(), Some(delivery));
            transport.wake_ready();
        }
        Step::BinaryClose => {
            assert!(app.dispatch_file("file.close"));
            assert!(app.close_dialog.is_none());
            assert!(!app.view.close_pending());
        }
        Step::BinaryCancelCheck => {
            assert!(!app.binary_pending());
            assert!(app.close_dialog.is_none());
            assert!(app.view.editor().is_some());
            if let Some(dir) = &dir {
                std::fs::write(dir.join("binary-offer-result.txt"), "PASS: hidden owned prompt; Hex default/Text/Cancel; worker-owned resolution; no visible OS confirmation").unwrap();
            }
        }
        Step::FolderOpen => {
            use kwil_view_model::search_panel::{SearchField, SearchInput};
            app.set_search_root(PathBuf::from(std::env::var_os("KWIL_WORKSPACE_ROOT").unwrap()));
            assert!(app.dispatch_search("search.findInFiles"));
            assert!(app.dispatch_search("search.setInclude"));
            app.search_input(SearchInput::Text("folder-probe.txt"));
            app.search_input(SearchInput::Focus(SearchField::Query));
            app.search_input(SearchInput::SelectAll);
            app.search_input(SearchInput::Text("kwil-native-folder-needle"));
        }
        Step::FolderCheck => {
            assert!(!app.search_panel.is_pending());
            assert_eq!(app.search_panel.count(), (1, true));
            assert!(app.search_area.is_some(), "folder card must have painted");
        }
        Step::FolderSelect => {
            app.search_input(kwil_view_model::search_panel::SearchInput::SelectRow(0));
        }
        Step::FolderOpened => {
            let editor = app.view.editor().expect("selected file prepared");
            assert_eq!(editor.document().path().and_then(|p| p.file_name()).unwrap(), "folder-probe.txt");
            assert_eq!(editor.cursors()[editor.primary_index()].head, 6, "decoded line two, column one");
            app.search_input(kwil_view_model::search_panel::SearchInput::Cancel);
            if let Some(dir) = &dir {
                std::fs::write(
                    dir.join("folder-search-result.txt"),
                    "PASS: real folder worker, painted card, exact count and native line-two open",
                )
                .unwrap();
            }
        }
        Step::DesktopWindowProbe => {
            app.sync_desktop_window(el);
            let main = app.gfx.as_ref().unwrap().window.id();
            let dialog = app.desktop_window.as_ref().expect("welcome is a separate desktop window");
            assert_ne!(dialog.gfx.window.id(), main);
            assert_eq!(dialog.kind, super::desktop_window::Kind::Welcome);
            let p = super::window_placement::capture(&dialog.gfx.window).unwrap();
            let work = super::window_placement::work(&dialog.gfx.window.current_monitor().unwrap());
            assert!(
                p.x >= work.x
                    && p.y >= work.y
                    && i64::from(p.x) + i64::from(p.width) <= i64::from(work.x) + i64::from(work.width)
                    && i64::from(p.y) + i64::from(p.height) <= i64::from(work.y) + i64::from(work.height),
                "welcome outer bounds remain inside the work area"
            );
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
        Step::PlacementSet => {
            assert!(app.window_placement_loaded);
            app.window_touched = true;
            let window = &app.gfx.as_ref().unwrap().window;
            let monitor = window.current_monitor().unwrap();
            let work = super::window_placement::work(&monitor);
            let p = super::window_placement::Placement {
                x: work.x + 40,
                y: work.y + 60,
                width: (700.0 * work.scale) as u32,
                height: (480.0 * work.scale) as u32,
                scale: work.scale,
                maximized: false,
            }
            .clamp(work);
            super::window_placement::fit_window(window, p);
            app.remember_placement();
        }
        Step::PlacementCheck => {
            assert!(app.window_placement_loaded);
            let window = &app.gfx.as_ref().unwrap().window;
            let p = super::window_placement::capture(window).unwrap();
            let work = super::window_placement::work(&window.current_monitor().unwrap());
            assert!(
                p.x >= work.x
                    && p.y >= work.y
                    && i64::from(p.x) + i64::from(p.width) <= i64::from(work.x) + i64::from(work.width)
                    && i64::from(p.y) + i64::from(p.height) <= i64::from(work.y) + i64::from(work.height)
            );
            if let Some(expected) = std::env::var_os("KWIL_EXPECT_PLACEMENT") {
                let values: Vec<i64> = expected.to_str().unwrap().split(',').map(|v| v.parse().unwrap()).collect();
                assert_eq!(
                    values,
                    vec![i64::from(p.x), i64::from(p.y), i64::from(p.width), i64::from(p.height)],
                    "fresh process restored actual native outer bounds"
                );
            }
            if let Some(dir) = &dir {
                std::fs::write(dir.join("placement-result.txt"), format!("{},{},{},{}", p.x, p.y, p.width, p.height))
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
            app.close_search();
            let button = app.menu.toolbar.iter().find(|t| t.command == "search.find").unwrap().rect;
            app.pointer = (button.x + button.w / 2, button.y + button.h / 2);
            app.menu_mouse(el);
            assert!(app.search_panel.is_open(), "SVG Find button dispatches the real command");
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
        Step::AboutOpen => app.run_menu_command(Some(el), super::menu::ABOUT),
        Step::AboutCheck => {
            app.sync_desktop_window(el);
            assert_eq!(app.desktop_window.as_ref().unwrap().kind, super::desktop_window::Kind::About);
            assert!(app.about.platform.is_some());
            let text = app.diagnostics();
            assert!(text.contains("Build:"));
            assert!(text.contains("Renderer:"));
            assert!(text.contains("License: MIT"));
            if let Some(dir) = &dir {
                std::fs::write(dir.join("about-diagnostics.txt"), text).unwrap();
            }
            assert!(app.render_desktop_window());
        }
        Step::AboutClose => {
            let id = app.desktop_window.as_ref().unwrap().gfx.window.id();
            app.desktop_window_event(el, id, winit::event::WindowEvent::CloseRequested);
            assert!(!app.about.open);
            assert!(app.desktop_window.is_none());
        }
        Step::GzipOpen => {
            app.view
                .open_path(PathBuf::from(std::env::var_os("KWIL_WORKSPACE_ROOT").unwrap()).join("example.log.gz"))
                .unwrap();
            app.sync_file_ui();
            app.apply(Damage { full: true, ..Default::default() });
        }
        Step::GzipCheck => {
            let document = app.view.editor().expect("gzip editor indexed").document();
            assert_eq!(document.compression, kwil_core_workspace::compressed::CompressionFormat::Gzip);
            let mut bytes = Vec::new();
            document.snapshot().write_to(&mut bytes).unwrap();
            assert_eq!(bytes, include_bytes!("../../../examples/smoke/example.log"));
            if let Some(dir) = &dir {
                std::fs::write(
                    dir.join("gzip-result.txt"),
                    "PASS: real GUI open decompresses exact committed log sample",
                )
                .unwrap();
            }
        }
        Step::LanguageOpen => {
            assert!(app.dispatch_language("file.setType"));
            assert!(app.language_open());
        }
        Step::LanguageChoose => {
            for key in [
                winit::keyboard::NamedKey::Home,
                winit::keyboard::NamedKey::ArrowDown,
                winit::keyboard::NamedKey::Enter,
            ] {
                assert!(app.language_key(&super::HostKey {
                    logical_key: winit::keyboard::Key::Named(key),
                    text: None,
                    repeat: false
                }));
            }
            assert!(!app.language_open());
            assert!(matches!(
                app.highlighting.as_ref().unwrap().language_choice(app.view.active_id()),
                kwil_view_model::highlighting::LanguageChoice::Explicit(_)
            ));
        }
        Step::LanguageReset => {
            assert!(app.dispatch_language("file.setType"));
            for key in [winit::keyboard::NamedKey::Home, winit::keyboard::NamedKey::Enter] {
                assert!(app.language_key(&super::HostKey {
                    logical_key: winit::keyboard::Key::Named(key),
                    text: None,
                    repeat: false
                }));
            }
            assert_eq!(
                app.highlighting.as_ref().unwrap().language_choice(app.view.active_id()),
                kwil_view_model::highlighting::LanguageChoice::Automatic
            );
        }
        Step::HexOpen => assert!(app.dispatch_hex("view.toggleHex")),
        Step::HexCheck => {
            assert!(app.view.hex_active());
            assert!(!app.view.document_busy(app.view.active_id()));
            let row = app.view.hex_session().unwrap().row(app.view.hex_source(), 0).unwrap();
            assert_eq!(&row.bytes[..2], &[0x1f, 0x8b], "hex must expose the physical gzip bytes");
        }
        Step::HexClose => assert!(app.dispatch_hex("view.toggleHex")),
        Step::HexRestored => {
            assert!(!app.view.hex_active());
            assert!(!app.view.document_busy(app.view.active_id()));
            let document = app.view.editor().unwrap().document();
            assert_eq!(document.compression, kwil_core_workspace::compressed::CompressionFormat::Gzip);
            let mut bytes = Vec::new();
            document.snapshot().write_to(&mut bytes).unwrap();
            assert_eq!(bytes, include_bytes!("../../../examples/smoke/example.log"));
            if let Some(dir) = &dir {
                std::fs::write(
                    dir.join("language-hex-result.txt"),
                    "PASS: language choice/reset; physical gzip hex; clean return to exact decoded text",
                )
                .unwrap();
            }
        }
        Step::LogsOpen => {
            assert!(app.is_log_document());
            app.filter_log_level(Some(kwil_core_syntax::logs::Level::Error));
            assert!(
                app.logs.owns_body(),
                "log admission: modal={} prompt={} save_path={} settings={} hex={} notice={:?}",
                app.file_modal(),
                app.view.file_prompt().is_some(),
                app.save_path.is_some(),
                app.settings_open(),
                app.view.hex_active(),
                app.view.notice()
            );
        }
        Step::LogsCheck => {
            let (total, exact, _) = app.logs.viewport_extent();
            assert!(
                app.logs.owns_body(),
                "log body closed: is_log={} detected={:?} notice={:?}",
                app.is_log_document(),
                app.highlighting.as_ref().and_then(|h| h.detected_language(&app.view)),
                app.view.notice()
            );
            assert!(exact);
            assert_eq!(total, 1);
            let original = super::chrome::layout(app.size, &app.ui, app.settings.tabs.layout).body;
            assert!(app.body_area().y > original.y, "filter strip reserves height, no overlay list");
            let mut bytes = Vec::new();
            app.view.editor().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
            assert_eq!(bytes, include_bytes!("../../../examples/smoke/example.log"));
            if let Some(dir) = &dir {
                std::fs::write(
                    dir.join("inline-log-result.txt"),
                    "PASS: one ERROR row in editor body; original source untouched",
                )
                .unwrap();
            }
        }
        Step::LogsClose => app.close_logs(),
        Step::MarkdownSection(line) => app.markdown_probe_position(line),
        Step::MarkdownNarrow => {
            let _ = app.gfx.as_ref().unwrap().window.request_inner_size(winit::dpi::LogicalSize::new(640.0, 650.0));
        }
        Step::ContextProbe => {
            let body = app.body_area();
            app.pointer = (body.x + 80, body.y + 20);
            assert_eq!(app.pointer_cursor(), winit::window::CursorIcon::Text);
            app.open_context();
            assert!(app.context.is_open());
            assert!(
                !app.context.probe_labels().any(|label| label.contains("log")),
                "Markdown context must not show log actions"
            );
            app.close_context();
            if let Some(title) = app.menu.titles.first() {
                app.pointer = (title.x + 2, title.y + 2);
                assert_eq!(app.pointer_cursor(), winit::window::CursorIcon::Pointer);
                app.menu_set(super::menu::State::Open { menu: 0, item: None, keyboard: false }, false);
                assert_eq!(app.pointer_cursor(), winit::window::CursorIcon::Pointer);
                app.menu_set(super::menu::State::Closed, false);
            }
            app.pointer = (body.x + 80, body.y + 20);
            app.open_context();
            if let Some(dir) = &dir {
                std::fs::write(
                    dir.join("context-cursor-result.txt"),
                    "PASS: editor I-beam, consistent menu pointer, context menu opens",
                )
                .unwrap();
            }
        }
        Step::ContextClose => app.close_context(),
        Step::PresenterCpu => {
            app.opts.renderer = None;
            app.settings.render.backend = super::RenderBackend::Cpu;
            app.apply(Damage::default());
            #[cfg(all(windows, feature = "gpu"))]
            unsafe {
                assert_eq!(
                    windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible(
                        app.gfx.as_ref().unwrap().gpu_canvas.as_ref().unwrap().hwnd() as _
                    ),
                    0
                );
            }
            assert!(app.pending.full, "presenter handoff schedules full damage even at idle");
        }
        Step::PresenterEdit => app.edit(super::input::Action::Text("CPU presenter marker\n".into())),
        Step::PresenterGpu => {
            app.settings.render.backend = super::RenderBackend::Gpu;
            app.apply(Damage::default());
            assert!(app.pending.full);
        }
        Step::PresenterCheck => {
            #[cfg(feature = "gpu")]
            assert_eq!(app.gpu.as_ref().unwrap().matches(&app.gfx.as_ref().unwrap().pixels), Ok(true));
            if let Some(dir) = &dir {
                std::fs::write(
                    dir.join("presenter-transition-result.txt"),
                    "PASS: CPU edit survives full GPU takeover",
                )
                .unwrap();
            }
        }
        Step::PresenterRemote => app.user_event(el, UserEvent::SessionChanged(true)),
        Step::PresenterRemoteCheck => {
            assert!(app.policy.remote());
            assert!(!app.presenter_gpu);
            #[cfg(all(windows, feature = "gpu"))]
            unsafe {
                assert_eq!(
                    windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible(
                        app.gfx.as_ref().unwrap().gpu_canvas.as_ref().unwrap().hwnd() as _
                    ),
                    0
                );
            }
            assert_eq!(app.pending, Damage::default());
            if let Some(dir) = &dir {
                std::fs::write(
                    dir.join("remote-handoff-result.txt"),
                    "PASS: idle remote transition hides GPU child and completes full CPU handoff",
                )
                .unwrap();
            }
        }
        Step::PresenterLocal => app.user_event(el, UserEvent::SessionChanged(false)),
        Step::GpuFallback => {
            #[cfg(all(windows, feature = "gpu"))]
            {
                let gfx = app.gfx.as_ref().unwrap();
                let canvas = gfx.gpu_canvas.as_ref().expect("GPU child surface");
                assert_ne!(canvas.hwnd(), gfx.hwnd);
                app.gpu.as_mut().expect("GPU before injected failure").inject_retirement_timeout();
                app.apply(Damage { status: true, ..Default::default() });
            }
        }
        Step::GpuFallbackCheck => {
            #[cfg(all(windows, feature = "gpu"))]
            {
                assert!(app.gpu.is_none(), "failure must select CPU");
                let gfx = app.gfx.as_ref().unwrap();
                unsafe {
                    assert_eq!(
                        windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible(
                            gfx.gpu_canvas.as_ref().unwrap().hwnd() as _
                        ),
                        0
                    );
                }
                assert_eq!(app.pending, Damage::default(), "full CPU repair must happen immediately");
                capture(app, dir.as_deref(), "cpu-after-quarantine.bmp");
                if let Some(dir) = &dir {
                    std::fs::write(
                        dir.join("gpu-fallback-result.txt"),
                        "PASS: child flip HWND hidden; parent CPU full repair, bounded GPU quarantine",
                    )
                    .unwrap();
                }
            }
        }
        Step::MarkdownSplit => {
            use winit::keyboard::{Key, ModifiersState};
            app.mods = ModifiersState::CONTROL | ModifiersState::SHIFT;
            app.key_after_settings(
                Some(el),
                &super::HostKey { logical_key: Key::Character("v".into()), text: None, repeat: false },
            );
            app.mods = ModifiersState::empty();
            assert_eq!(app.markdown_mode(), super::markdown_ui::Mode::Split);
        }
        Step::MarkdownMenu => {
            app.menu_set(super::menu::State::Open { menu: 2, item: Some(0), keyboard: true }, true);
        }
        Step::MarkdownMenuCheck => {
            let available = app.menu_available();
            let dropdown = app.menu.dropdown.as_ref().expect("selection menu painted");
            let actual = &app.gfx.as_ref().unwrap().pixels;
            let mut expected = actual.clone();
            super::menu::draw_dropdown(
                &app.menu,
                dropdown,
                &available,
                &mut expected,
                app.size.0,
                app.ui_font.as_mut().unwrap(),
                super::theme_ui::chrome(),
                &app.ui,
            );
            let r = dropdown.area;
            for y in r.y..r.y + r.h {
                let range = y * app.size.0 + r.x..y * app.size.0 + r.x + r.w;
                assert_eq!(
                    actual[range.clone()],
                    expected[range],
                    "menu must paint above Markdown controls and preview"
                );
            }
            if let Some(dir) = &dir {
                std::fs::write(
                    dir.join("markdown-menu-result.txt"),
                    "PASS: native popup pixels remain above the Markdown surface",
                )
                .unwrap();
            }
        }
        Step::MarkdownPreview => {
            if let Some(dir) = &dir {
                std::fs::write(
                    dir.join("markdown-source.txt"),
                    format!(
                        "rows={} line0={:?} mode={:?} body={:?} sourcepixels={} bpixels={}",
                        app.view.rows(),
                        app.view.line(0),
                        app.markdown_mode(),
                        app.body_area(),
                        app.gfx.as_ref().unwrap().pixels.iter().filter(|p| **p == super::fg()).count(),
                        app.body_pixels.iter().filter(|p| **p == super::fg()).count()
                    ),
                )
                .unwrap();
            }
            assert!(app.markdown.ready(), "real snapshot worker must complete");
            assert!(app.markdown_probe_click(el, super::markdown_ui::Mode::Preview));
            assert_eq!(app.markdown_mode(), super::markdown_ui::Mode::Preview);
        }
        Step::MarkdownText => {
            let revision = app.view.editor().unwrap().document().revision();
            app.key_after_settings(
                Some(el),
                &super::HostKey {
                    logical_key: winit::keyboard::Key::Character("x".into()),
                    text: Some("x"),
                    repeat: false,
                },
            );
            assert_eq!(
                app.view.editor().unwrap().document().revision(),
                revision,
                "preview must not edit behind the pane"
            );
            if !app.markdown_probe_click(el, super::markdown_ui::Mode::Text) {
                // Narrow chrome legitimately omits overflow toolbar tools.
                // The same Text command remains reachable through View.
                app.run_menu_command(Some(el), "view.markdownText");
            }
            assert_eq!(app.markdown_mode(), super::markdown_ui::Mode::Text);
        }
        Step::Idle => {
            #[cfg(feature = "gpu")]
            if std::env::var_os("KWIL_REQUIRE_GPU").is_some() {
                assert!(app.gpu.is_some(), "hardware GPU required by this probe");
            }
            // Prime the retained GPU image without an onscreen present, so the
            // following scroll/readback exercises GPU copies, not a fresh upload.
            #[cfg(all(feature = "gpu", not(windows)))]
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
                let equal = gpu.matches(&gfx.pixels);
                println!("kwil-gpu backing_pixels_equal_cpu={equal:?}");
                if std::env::var_os("KWIL_REQUIRE_GPU").is_some() {
                    assert_eq!(equal, Ok(true));
                }
                #[cfg(windows)]
                {
                    println!("kwil-gpu native_presents={}", gpu.presents());
                    assert!(gpu.presents() > 0);
                    if let Some(dir) = &dir {
                        std::fs::write(
                            dir.join("gpu-result.txt"),
                            format!("readback={equal:?} native_presents={}\n", gpu.presents()),
                        )
                        .unwrap();
                    }
                }
            }
            if std::env::var_os("KWIL_REQUIRE_GPU").is_some() {
                assert!(app.gpu.is_some(), "GPU must remain active through scroll");
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
            if app.force_exit() {
                el.exit();
            }
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
    app.user_event(el, UserEvent::SettingsReady(Box::new(changed), Default::default()));
    assert!(app.drag.is_none(), "layout change must release the old coordinate anchor");
    app.user_event(el, UserEvent::SettingsReady(Box::new(settings), Default::default()));
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
        #[cfg(all(windows, feature = "gpu"))]
        if app.desktop_window.is_none()
            && let Some(gpu) = &app.gpu
        {
            let pixels = gpu.read_pixels().expect("native DX12 capture readback");
            assert!(
                pixels.iter().zip(&gfx.pixels).all(|(a, b)| *a == (*b | 0xff000000)),
                "capture must match actual native GPU backing"
            );
            write_pixels(&dir.join(name), app.size, &pixels).expect("DX12 capture write");
            std::fs::write(
                dir.join(format!("{name}.backend.txt")),
                "DX12 retained presentation texture readback (not a DWM/window-DC capture)",
            )
            .unwrap();
            return;
        }
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

#[cfg(all(windows, feature = "gpu"))]
fn write_pixels(path: &Path, size: (usize, usize), pixels: &[u32]) -> std::io::Result<()> {
    let (w, h) = size;
    let len = w * h * 4;
    assert_eq!(pixels.len(), w * h);
    let mut file = Vec::with_capacity(54 + len);
    file.extend_from_slice(b"BM");
    file.extend_from_slice(&((54 + len) as u32).to_le_bytes());
    file.extend_from_slice(&[0; 4]);
    file.extend_from_slice(&54u32.to_le_bytes());
    file.extend_from_slice(&40u32.to_le_bytes());
    file.extend_from_slice(&(w as i32).to_le_bytes());
    file.extend_from_slice(&(-(h as i32)).to_le_bytes());
    file.extend_from_slice(&1u16.to_le_bytes());
    file.extend_from_slice(&32u16.to_le_bytes());
    file.extend_from_slice(&[0; 24]);
    for pixel in pixels {
        file.extend_from_slice(&pixel.to_le_bytes());
    }
    std::fs::write(path, file)
}
