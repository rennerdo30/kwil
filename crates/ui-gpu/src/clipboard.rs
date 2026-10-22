use kwil_view_model::{Clipboard, ClipboardData};
use std::io;

pub struct SystemClipboard(pub isize);
const LIMIT: usize = 16 << 20;

#[cfg(windows)]
impl Clipboard for SystemClipboard {
    fn set(&mut self, data: ClipboardData) -> io::Result<()> {
        let length = data
            .ranges
            .iter()
            .try_fold(0u64, |n, r| n.checked_add(r.end - r.start))
            .and_then(|n| n.checked_add(data.ranges.len() as u64));
        if length.is_none_or(|n| n > LIMIT as u64) {
            return Err(io::Error::other("Selection exceeds the 16 MiB system clipboard limit"));
        }
        let mut bytes = Vec::new();
        data.write_to(&mut bytes)?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| io::Error::other("Selection contains invalid UTF-8; clipboard copy would change bytes"))?;
        if text.contains('\0') {
            return Err(io::Error::other("Selection contains NUL; system clipboard cannot preserve it"));
        }
        let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        unsafe {
            use windows_sys::Win32::{
                Foundation::GlobalFree,
                System::{DataExchange::*, Memory::*},
            };
            if OpenClipboard(self.0 as _) == 0 {
                return Err(io::Error::last_os_error());
            }
            struct Close;
            impl Drop for Close {
                fn drop(&mut self) {
                    unsafe {
                        CloseClipboard();
                    }
                }
            }
            let _close = Close;
            let memory = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
            if memory.is_null() {
                return Err(io::Error::last_os_error());
            }
            let pointer = GlobalLock(memory) as *mut u16;
            if pointer.is_null() {
                GlobalFree(memory);
                return Err(io::Error::last_os_error());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), pointer, wide.len());
            GlobalUnlock(memory);
            if EmptyClipboard() == 0 || SetClipboardData(13, memory).is_null() {
                let error = io::Error::last_os_error();
                GlobalFree(memory);
                return Err(error);
            }
        }
        Ok(())
    }

    fn get(&mut self) -> io::Result<Option<ClipboardData>> {
        unsafe {
            use windows_sys::Win32::System::{DataExchange::*, Memory::*};
            if OpenClipboard(self.0 as _) == 0 {
                return Err(io::Error::last_os_error());
            }
            struct Close;
            impl Drop for Close {
                fn drop(&mut self) {
                    unsafe {
                        CloseClipboard();
                    }
                }
            }
            let _close = Close;
            if IsClipboardFormatAvailable(13) == 0 {
                return Ok(None);
            }
            let memory = GetClipboardData(13);
            if memory.is_null() {
                return Err(io::Error::last_os_error());
            }
            let size = GlobalSize(memory);
            if size > LIMIT * 2 {
                return Err(io::Error::other("Clipboard exceeds 16 MiB text limit"));
            }
            let pointer = GlobalLock(memory) as *const u16;
            if pointer.is_null() {
                return Err(io::Error::last_os_error());
            }
            let wide = std::slice::from_raw_parts(pointer, size / 2);
            let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
            let text = String::from_utf16(&wide[..end]);
            GlobalUnlock(memory);
            let text = text.map_err(|_| io::Error::other("Clipboard contains invalid UTF-16"))?;
            Ok(Some(ClipboardData::text(text.as_bytes())?))
        }
    }
}

#[cfg(not(windows))]
impl Clipboard for SystemClipboard {
    fn set(&mut self, _data: ClipboardData) -> io::Result<()> {
        Err(io::Error::other("System clipboard unavailable on this backend"))
    }
    fn get(&mut self) -> io::Result<Option<ClipboardData>> {
        Err(io::Error::other("System clipboard unavailable on this backend"))
    }
}
