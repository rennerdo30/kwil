//! capture <window title> <out.bmp>: window-DC capture of a (cloaked) window.
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let title: Vec<u16> = a[1].encode_utf16().chain(Some(0)).collect();
    let hwnd = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW(std::ptr::null(), title.as_ptr()) };
    assert!(!hwnd.is_null(), "window not found");
    spiketrace::capture_window_dc_bmp(hwnd as isize, std::path::Path::new(&a[2])).unwrap();
}
