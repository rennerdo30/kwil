//! The Windows installer's "Installed apps" entry (per user, HKCU, written by
//! `installer/kwil.nsi`). It shows the version, so updating the installed
//! files also updates that value. Portable copies have no entry and stay
//! untouched.
use std::path::Path;

#[cfg(windows)]
pub(crate) const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Kwil";

/// Whether the entry's InstallLocation is the folder that was just updated.
pub(crate) fn same_folder(install_location: &str, dir: &Path) -> bool {
    let normal = |s: &str| s.trim().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    !install_location.trim().is_empty() && normal(install_location) == normal(&dir.to_string_lossy())
}

#[cfg(windows)]
pub(crate) fn sync_version(dir: &Path, version: &str) -> Result<(), String> {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
        RegSetValueExW,
    };
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: valid NUL-terminated strings and out pointer; the key is closed below.
    let status = unsafe {
        RegOpenKeyExW(HKEY_CURRENT_USER, wide(UNINSTALL_KEY).as_ptr(), 0, KEY_QUERY_VALUE | KEY_SET_VALUE, &mut key)
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    if status != ERROR_SUCCESS {
        return Err(format!("cannot open the Installed apps entry (error {status})"));
    }
    let result = (|| {
        let mut buffer = vec![0u16; 2048];
        let mut bytes = (buffer.len() * 2) as u32;
        let mut kind = 0;
        // SAFETY: the buffer holds `bytes` bytes; the key is open.
        let status = unsafe {
            RegQueryValueExW(
                key,
                wide("InstallLocation").as_ptr(),
                std::ptr::null(),
                &mut kind,
                buffer.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if status != ERROR_SUCCESS || kind != REG_SZ {
            return Ok(());
        }
        let location = String::from_utf16_lossy(&buffer[..(bytes as usize / 2).min(buffer.len())]);
        if !same_folder(location.trim_end_matches('\0'), dir) {
            return Ok(());
        }
        let value = wide(version);
        // SAFETY: `value` is a NUL-terminated UTF-16 string of the given byte length.
        let status = unsafe {
            RegSetValueExW(
                key,
                wide("DisplayVersion").as_ptr(),
                0,
                REG_SZ,
                value.as_ptr().cast(),
                (value.len() * 2) as u32,
            )
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("cannot update the Installed apps version (error {status})"))
        }
    })();
    // SAFETY: opened above.
    unsafe { RegCloseKey(key) };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_installed_folder_updates_the_entry() {
        let dir = Path::new(r"C:\Users\someone\AppData\Local\Programs\Kwil");
        for location in [
            r"C:\Users\someone\AppData\Local\Programs\Kwil",
            r"c:\users\someone\appdata\local\programs\kwil\",
            "C:/Users/someone/AppData/Local/Programs/Kwil",
        ] {
            assert!(same_folder(location, dir), "{location}");
        }
        for location in ["", "  ", r"C:\Users\someone\Downloads", r"C:\Users\someone\AppData\Local\Programs\Kwil2"] {
            assert!(!same_folder(location, dir), "{location}");
        }
    }
}
