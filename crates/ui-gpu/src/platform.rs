//! Native caption controls stay owned by winit/OS. All deferred Windows work
//! lives in one UI-thread guard, which unregisters before the window dies.
#[cfg(windows)]
pub(crate) mod file_dialog_windows;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;

#[cfg(not(windows))]
pub(crate) fn decorate_window(_hwnd: isize, _dark: bool, _remote: bool, _dialog: bool) {}

#[cfg(not(windows))]
pub(crate) struct Native;
#[cfg(not(windows))]
impl Native {
    pub fn register(_hwnd: isize, _send: impl Fn(super::UserEvent) + Send + Sync + 'static) -> Self {
        Self
    }
    pub fn user_placed(&self) -> bool {
        false
    }
    pub fn theme(&self, _dark: bool, _remote: bool) {}
}

/// X11 has an actual server-side copy command (VNC can encode it as CopyRect).
/// Wayland has no copy protocol: the moved pixels must be included in damage.
#[cfg(target_os = "linux")]
pub(crate) fn x11_copy(
    window: &winit::window::Window,
    origin_x: usize,
    w: usize,
    h: usize,
    from: usize,
    to: usize,
) -> bool {
    use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
    let (Ok(display), Ok(handle)) = (window.display_handle(), window.window_handle()) else { return false };
    let (RawDisplayHandle::Xlib(display), RawWindowHandle::Xlib(handle)) = (display.as_raw(), handle.as_raw()) else {
        return false;
    };
    let Some(display) = display.display else { return false };
    static XLIB: std::sync::OnceLock<Option<x11_dl::xlib::Xlib>> = std::sync::OnceLock::new();
    let Some(x) = XLIB.get_or_init(|| x11_dl::xlib::Xlib::open().ok()) else { return false };
    unsafe {
        let display = display.as_ptr().cast();
        let gc = (x.XCreateGC)(display, handle.window, 0, std::ptr::null_mut());
        if gc.is_null() {
            return false;
        }
        (x.XSetGraphicsExposures)(display, gc, 0);
        (x.XCopyArea)(
            display,
            handle.window,
            handle.window,
            gc,
            origin_x as i32,
            from as i32,
            w as u32,
            h as u32,
            origin_x as i32,
            to as i32,
        );
        (x.XFreeGC)(display, gc);
        (x.XFlush)(display);
    }
    true
}
