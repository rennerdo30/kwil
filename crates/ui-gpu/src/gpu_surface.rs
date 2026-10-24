//! A flip-chain gets its own child HWND; the owner remains GDI-compatible.
//! The child never owns input. Hide it before CPU presentation, including on
//! failed GPU retirement. Parent destruction reclaims the bounded single HWND;
//! no in-flight swap-chain allocation is released by this visibility handoff.
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    UI::WindowsAndMessaging::*,
};
pub(super) struct Canvas {
    hwnd: HWND,
}
unsafe extern "system" fn procedure(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if message == WM_NCHITTEST {
        return HTTRANSPARENT as LRESULT;
    }
    unsafe { DefWindowProcW(hwnd, message, w, l) }
}
impl Canvas {
    pub fn create(parent: isize) -> Result<Self, String> {
        let class: Vec<u16> = "STATIC".encode_utf16().chain(Some(0)).collect();
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
                class.as_ptr(),
                std::ptr::null(),
                WS_CHILD | WS_DISABLED,
                0,
                0,
                1,
                1,
                parent as _,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_WNDPROC, procedure as *const () as isize);
        }
        Ok(Self { hwnd })
    }
    pub fn show(&self, size: (usize, usize)) -> isize {
        unsafe {
            SetWindowPos(
                self.hwnd,
                std::ptr::null_mut(),
                0,
                0,
                size.0 as i32,
                size.1 as i32,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
            );
        }
        self.hwnd as isize
    }
    pub fn hide(&self) {
        unsafe {
            ShowWindow(self.hwnd, SW_HIDE);
        }
    }
    #[cfg(feature = "frame-harness")]
    pub fn hwnd(&self) -> isize {
        self.hwnd as isize
    }
}
impl Drop for Canvas {
    fn drop(&mut self) {
        self.hide();
    }
}
