//! Local Markdown image reads stay on the fixed preview worker. No URL fetches.
use kwil_core_markdown::{
    BlockKind, InlineKind, Model, Span,
    image_decode::{self, Decoded, MAX_ENCODED_BYTES},
};
use std::{
    collections::HashMap,
    io::Read,
    path::{Component, Path},
    sync::atomic::{AtomicBool, Ordering},
};
pub(super) type Images = HashMap<String, Result<Decoded, String>>;
pub(super) fn relative(target: &str) -> Result<std::path::PathBuf, String> {
    if target.len() > 4096 || target.contains([':', '\\']) || target.chars().any(char::is_control) {
        return Err("Only local relative image paths are supported".into());
    }
    let mut decoded = Vec::with_capacity(target.len());
    let mut bytes = target.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hex = [bytes.next().ok_or("Invalid image escape")?, bytes.next().ok_or("Invalid image escape")?];
            let text = std::str::from_utf8(&hex).map_err(|_| "Invalid image escape")?;
            decoded.push(u8::from_str_radix(text, 16).map_err(|_| "Invalid image escape")?);
        } else {
            decoded.push(b);
        }
    }
    let text = String::from_utf8(decoded).map_err(|_| "Image path is not UTF-8")?;
    if text.contains([':', '\\']) || text.chars().any(char::is_control) {
        return Err("Unsafe image path".into());
    }
    let path = Path::new(&text);
    if path.is_absolute() || path.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
        return Err("Image must stay beside the document".into());
    }
    Ok(path.to_path_buf())
}
#[cfg(windows)]
fn opened_path(file: &std::fs::File) -> Result<std::path::PathBuf, String> {
    use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFinalPathNameByHandleW(file: *mut std::ffi::c_void, path: *mut u16, len: u32, flags: u32) -> u32;
    }
    let mut path = [0u16; 32768];
    let len =
        unsafe { GetFinalPathNameByHandleW(file.as_raw_handle(), path.as_mut_ptr(), path.len() as u32, 0) } as usize;
    if len == 0 || len >= path.len() {
        return Err("Cannot verify opened image path".into());
    }
    Ok(std::path::PathBuf::from(std::ffi::OsString::from_wide(&path[..len])))
}
#[cfg(target_os = "linux")]
fn opened_path(file: &std::fs::File) -> Result<std::path::PathBuf, String> {
    use std::os::fd::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).map_err(|e| e.to_string())
}
#[cfg(target_os = "macos")]
fn opened_path(file: &std::fs::File) -> Result<std::path::PathBuf, String> {
    use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};
    unsafe extern "C" {
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }
    let mut path = [0u8; 1024];
    if unsafe { fcntl(file.as_raw_fd(), 50, path.as_mut_ptr()) } < 0 {
        return Err("Cannot verify opened image path".into());
    }
    let len = path.iter().position(|b| *b == 0).ok_or("Invalid image path")?;
    Ok(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&path[..len])))
}
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn opened_path(_file: &std::fs::File) -> Result<std::path::PathBuf, String> {
    Err("Image path verification unavailable".into())
}
fn load(root: &Path, target: &str, stop: &AtomicBool) -> Result<Decoded, String> {
    let path = relative(target)?;
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    #[cfg(windows)]
    if root.to_string_lossy().to_ascii_lowercase().starts_with(r"\\?\unc\") {
        return Err("Network image loading is unavailable".into());
    }
    let mut candidate = root.clone();
    for component in path.components() {
        candidate.push(component);
        if std::fs::symlink_metadata(&candidate).map_err(|e| e.to_string())?.file_type().is_symlink() {
            return Err("Symbolic image paths are unavailable".into());
        }
    }
    let expected = candidate.canonicalize().map_err(|e| e.to_string())?;
    if !expected.starts_with(&root) {
        return Err("Image escapes the document directory".into());
    }
    if !std::fs::symlink_metadata(&expected).map_err(|e| e.to_string())?.is_file() {
        return Err("Only regular image files are supported".into());
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let mut file = options.open(&expected).map_err(|e| e.to_string())?;
    if opened_path(&file)?.canonicalize().map_err(|e| e.to_string())? != expected {
        return Err("Image path changed during admission".into());
    }
    let before = file.metadata().map_err(|e| e.to_string())?;
    if !before.is_file() || before.len() > MAX_ENCODED_BYTES as u64 {
        return Err("Image exceeds the 4 MiB encoded limit".into());
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    for _ in 0..=(MAX_ENCODED_BYTES / 16384) {
        if stop.load(Ordering::Acquire) {
            return Err("Image cancelled".into());
        }
        let mut block = [0; 16384];
        let n = file.read(&mut block).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        if bytes.len() + n > MAX_ENCODED_BYTES {
            return Err("Image grew beyond its limit".into());
        }
        bytes.extend_from_slice(&block[..n]);
    }
    let after = file.metadata().map_err(|e| e.to_string())?;
    if after.len() != before.len()
        || after.modified().ok() != before.modified().ok()
        || bytes.len() != before.len() as usize
    {
        return Err("Image changed while loading".into());
    }
    image_decode::decode(&bytes, Default::default(), &|| stop.load(Ordering::Acquire))
        .map_err(|e| format!("Image unavailable: {e:?}"))
}
fn targets<'a>(spans: &'a [Span], out: &mut Vec<&'a str>) {
    for span in spans {
        if let InlineKind::Image { url, .. } = &span.kind {
            out.push(url);
        }
    }
}
pub(super) fn load_images(model: &Model, path: Option<&Path>, stop: &AtomicBool) -> Images {
    let mut urls = Vec::new();
    for block in &model.blocks {
        if let BlockKind::Image { url, .. } = &block.kind {
            urls.push(url.as_str());
        }
        targets(&block.spans, &mut urls);
        if let BlockKind::Table { rows, .. } = &block.kind {
            for row in rows {
                for cell in row {
                    targets(cell, &mut urls);
                }
            }
        }
    }
    let mut images = Images::new();
    let mut retained = 0usize;
    for url in urls {
        if images.contains_key(url) {
            continue;
        }
        if images.len() >= 8 {
            break;
        }
        let result = if retained + image_decode::MAX_RGBA_BYTES > 16 * 1024 * 1024 {
            Err("Image cache exceeds its 16 MiB limit".into())
        } else {
            path.and_then(Path::parent)
                .ok_or("Save the document before loading relative images".into())
                .and_then(|root| load(root, url, stop))
        };
        if let Ok(image) = &result {
            retained += image.rgba.capacity();
        }
        images.insert(url.into(), result);
    }
    images
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schemes_escape_and_encoded_traversal_never_reach_filesystem() {
        for path in [
            "https://x/a.png",
            "file:///a.png",
            "../a.png",
            "%2e%2e/a.png",
            "%2fetc/a.png",
            "a%5Cb.png",
            "a%00.png",
            "C:/a.png",
        ] {
            assert!(relative(path).is_err(), "{path}");
        }
        assert_eq!(relative("images/an%20image.png").unwrap(), Path::new("images/an image.png"));
    }
}
