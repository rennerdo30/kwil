//! One owned desktop dialog. Settings/setup never borrow the editor's surface.
use super::*;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::window::WindowButtons;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Welcome,
    Settings,
}

pub(super) struct Dialog {
    pub gfx: Gfx,
    pub kind: Kind,
    pub size: (usize, usize),
    allocated: (usize, usize),
    pub metrics: chrome::Metrics,
    pub font: Option<font::UiFont>,
    pointer: (usize, usize),
    owner: isize,
}

#[cfg(windows)]
fn enable_owner(hwnd: isize, enabled: bool) {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn EnableWindow(hwnd: *mut std::ffi::c_void, enabled: i32) -> i32;
    }
    unsafe {
        EnableWindow(hwnd as _, i32::from(enabled));
    }
}
#[cfg(not(windows))]
fn enable_owner(_hwnd: isize, _enabled: bool) {}

impl Drop for Dialog {
    fn drop(&mut self) {
        enable_owner(self.owner, true);
    }
}

pub(super) fn owns_input(event: &WindowEvent) -> bool {
    matches!(
        event,
        WindowEvent::KeyboardInput { .. }
            | WindowEvent::Ime(_)
            | WindowEvent::MouseInput { .. }
            | WindowEvent::MouseWheel { .. }
            | WindowEvent::CursorMoved { .. }
            | WindowEvent::DroppedFile(_)
    )
}

impl App {
    pub(super) fn sync_desktop_window(&mut self, el: &ActiveEventLoop) {
        let wanted = if self.file_dialogs.exiting() {
            None
        } else if self.welcome.visible() {
            Some(Kind::Welcome)
        } else if self.settings_open() {
            Some(Kind::Settings)
        } else {
            None
        };
        if self.desktop_window.as_ref().map(|d| d.kind) == wanted {
            return;
        }
        self.desktop_window.take();
        self.composing = false;
        self.ime_destination = Some(ime::Destination::Cancelled);
        self.drag = None;
        self.selecting = false;
        let Some(kind) = wanted else {
            self.palette_dirty = true;
            self.request_frame();
            return;
        };
        let Some(owner) = self.gfx.as_ref() else { return };
        let logical = if kind == Kind::Settings { (1080.0, 780.0) } else { (880.0, 660.0) };
        let scale = owner.window.scale_factor();
        let monitor = owner.window.current_monitor().or_else(|| el.primary_monitor());
        let work = monitor.as_ref().map(window_placement::work).unwrap_or(window_placement::Work {
            x: 0,
            y: 0,
            width: 1280,
            height: 800,
            scale,
        });
        let placement = window_placement::Placement::centered(work, window_placement::capture(&owner.window), logical);
        let position = PhysicalPosition::new(placement.x, placement.y);
        let inner = (placement.width as f64 / scale - 16.0, placement.height as f64 / scale - 48.0);
        #[allow(unused_mut)]
        let mut attrs = Window::default_attributes()
            .with_title(if kind == Kind::Settings { "Settings — Kwil" } else { "Welcome to Kwil" })
            .with_inner_size(LogicalSize::new(inner.0.max(1.0), inner.1.max(1.0)))
            .with_min_inner_size(LogicalSize::new(560.0f64.min(inner.0).max(1.0), 440.0f64.min(inner.1).max(1.0)))
            .with_position(position)
            .with_resizable(true)
            .with_enabled_buttons(WindowButtons::CLOSE)
            .with_window_icon(winit::window::Icon::from_rgba(include_bytes!("icon.rgba").to_vec(), 32, 32).ok())
            .with_visible(false);
        #[cfg(windows)]
        {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs = attrs
                .with_owner_window(owner.hwnd as _)
                .with_skip_taskbar(true)
                .with_drag_and_drop(false)
                .with_active(!self.opts.hidden);
        }
        let result = (|| {
            let window = Rc::new(el.create_window(attrs).map_err(|e| e.to_string())?);
            window_placement::fit_window(&window, placement);
            if !self.opts.hidden {
                window.set_visible(true);
            }
            window.set_ime_allowed(true);
            let context = Context::new(window.clone()).map_err(|e| e.to_string())?;
            let surface = Surface::new(&context, window.clone()).map_err(|e| e.to_string())?;
            let hwnd = match raw_window_handle::HasWindowHandle::window_handle(&*window).map(|h| h.as_raw()) {
                #[cfg(windows)]
                Ok(raw_window_handle::RawWindowHandle::Win32(handle)) => handle.hwnd.get(),
                _ => 0,
            };
            let size = window.inner_size();
            let metrics = chrome::Metrics::new(window.scale_factor());
            let font = self.ui_font.as_ref().and_then(|font| font.variant(metrics.text_px()));
            platform::decorate_window(hwnd, self.settings.theme.name != "light", self.policy.remote(), true);
            #[cfg(windows)]
            if self.opts.hidden {
                cloak_and_show(hwnd);
            }
            Ok::<_, String>(Dialog {
                gfx: Gfx { native: None, window, surface, hwnd, pixels: Vec::new() },
                kind,
                size: (size.width as usize, size.height as usize),
                allocated: (0, 0),
                metrics,
                font,
                pointer: (0, 0),
                owner: owner.hwnd,
            })
        })();
        match result {
            Ok(dialog) => {
                enable_owner(dialog.owner, false);
                dialog.gfx.window.request_redraw();
                self.desktop_window = Some(dialog);
                self.palette_dirty = true;
                self.redraw_requested = true;
                if let Some(gfx) = &self.gfx {
                    gfx.window.request_redraw();
                }
            }
            Err(error) => {
                self.error = Some(GuiError::Unavailable(format!("Desktop dialog: {error}")));
                if self.force_exit() {
                    el.exit();
                }
            }
        }
    }

    pub(super) fn desktop_window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::RedrawRequested => {
                if !self.render_desktop_window() && self.force_exit() {
                    el.exit();
                }
            }
            WindowEvent::Resized(size) => {
                if let Some(dialog) = &mut self.desktop_window {
                    dialog.size = (size.width as usize, size.height as usize);
                    dialog.gfx.window.request_redraw();
                }
                self.settings_epoch = self.settings_epoch.wrapping_add(1);
                self.welcome_view.hits = desktop::Hits::default();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(dialog) = &mut self.desktop_window {
                    dialog.metrics = chrome::Metrics::new(scale_factor);
                    dialog.font = self.ui_font.as_ref().and_then(|font| font.variant(dialog.metrics.text_px()));
                    dialog.gfx.window.request_redraw();
                }
                self.settings_epoch = self.settings_epoch.wrapping_add(1);
                self.welcome_view.hits = desktop::Hits::default();
            }
            WindowEvent::CloseRequested => {
                if self.welcome.visible() {
                    if self.welcome.stage() != welcome::Stage::Registering {
                        self.welcome_action(welcome_ui::Action::Skip);
                    }
                } else if let Some(panel) = &mut self.settings_panel {
                    panel.close();
                }
                self.sync_desktop_window(el);
            }
            WindowEvent::Destroyed => {
                self.desktop_window.take();
            }
            other => {
                let Some(dialog) = &self.desktop_window else { return };
                let old_size = std::mem::replace(&mut self.size, dialog.size);
                let old_ui = std::mem::replace(&mut self.ui, dialog.metrics);
                let old_pointer = std::mem::replace(&mut self.pointer, dialog.pointer);
                self.desktop_dispatch = true;
                self.window_event(el, id, other);
                self.desktop_dispatch = false;
                if let Some(dialog) = &mut self.desktop_window {
                    dialog.pointer = self.pointer;
                }
                self.pointer = old_pointer;
                self.size = old_size;
                self.ui = old_ui;
                self.sync_desktop_window(el);
            }
        }
    }

    pub(super) fn render_desktop_window(&mut self) -> bool {
        let Some(mut dialog) = self.desktop_window.take() else { return true };
        let result = self.paint_desktop(&mut dialog);
        self.desktop_window = Some(dialog);
        result
    }

    fn paint_desktop(&mut self, dialog: &mut Dialog) -> bool {
        let (w, h) = dialog.size;
        let (Some(width), Some(height)) = (NonZeroU32::new(w as u32), NonZeroU32::new(h as u32)) else { return true };
        if dialog.allocated != dialog.size {
            if let Err(error) = dialog.gfx.surface.resize(width, height) {
                self.error = Some(GuiError::Unavailable(error.to_string()));
                return false;
            }
            dialog.gfx.pixels.resize(w.saturating_mul(h), 0);
            dialog.allocated = dialog.size;
        }
        let theme = theme_ui::chrome();
        let colors = desktop::Colors::new(theme);
        dialog.gfx.pixels.fill(colors.sidebar);
        let Some(font) = dialog.font.as_mut() else { return true };
        match dialog.kind {
            Kind::Welcome => {
                self.welcome_view.paint.take();
                welcome_ui::draw(
                    &self.welcome,
                    &mut self.welcome_view,
                    self.file_types.as_mut(),
                    &mut dialog.gfx.pixels,
                    dialog.size,
                    font,
                    theme,
                    &dialog.metrics,
                );
            }
            Kind::Settings => {
                self.settings_view.paint.take();
                if let Some(panel) = self.settings_panel.as_ref().filter(|panel| panel.is_open()) {
                    if let Some(page) = &mut self.file_types {
                        page.ui.setup = false;
                    }
                    let hosted = self.file_types.as_mut().map(|page| page as &mut dyn settings_ui::HostedPage);
                    settings_ui::draw(
                        panel,
                        &mut dialog.gfx.pixels,
                        dialog.size,
                        font,
                        theme,
                        &dialog.metrics,
                        self.settings_epoch,
                        &mut self.settings_view,
                        hosted,
                    );
                }
            }
        }
        if font.take_fallback_request()
            && let Some(main_font) = self.ui_font.as_ref()
        {
            if main_font.fallback_finished() {
                font.copy_fallback_from(main_font);
                dialog.gfx.window.request_redraw();
            } else if let Some(id) = self.ui_fallback.request(main_font.id()) {
                spawn_ui_fallback(&self.proxy, id);
            }
        }
        let result = dialog.gfx.surface.buffer_mut().and_then(|mut target| {
            target.copy_from_slice(&dialog.gfx.pixels);
            target.present()
        });
        if let Err(error) = result {
            self.error = Some(GuiError::Unavailable(error.to_string()));
            return false;
        }
        true
    }
}
