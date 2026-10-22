//! Bounded binary startup reader. No TOML, JSON or XML parser is called here.
use crate::theme::*;
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

pub const MAX_CACHE_BYTES: usize = 256 * 1024;
const MAGIC: &[u8; 8] = b"KWTHM\0\r\n";
const VERSION: u16 = 1;
const HEADER: usize = 22;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedTheme {
    pub selection: ThemeSelection,
    pub theme: Theme,
}

/// Missing cache means built-in fallback, never source parsing. Other failures
/// are explicit so the host can retain its built-in and surface a later notice.
pub fn read_active_cache(path: &Path) -> Result<Option<CachedTheme>, ThemeError> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !file.metadata()?.is_file() || file.metadata()?.len() > MAX_CACHE_BYTES as u64 {
        return Err(error("invalid or oversized theme cache"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_CACHE_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    decode(&bytes).map(Some)
}
fn hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x100000001b3))
}
fn string(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u16).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}
fn id(out: &mut Vec<u8>, id: &ThemeId) {
    match id {
        ThemeId::Dark => out.push(0),
        ThemeId::Light => out.push(1),
        ThemeId::User(s) => {
            out.push(2);
            string(out, s);
        }
    }
}
fn color(out: &mut Vec<u8>, c: Rgba) {
    out.extend_from_slice(&[c.0, c.1, c.2, c.3]);
}
pub(crate) fn encode(cached: &CachedTheme) -> Result<Vec<u8>, ThemeError> {
    cached.theme.validate()?;
    cached.selection.validate()?;
    let mut body = Vec::new();
    match &cached.selection {
        ThemeSelection::Named(i) => {
            body.push(0);
            id(&mut body, i);
        }
        ThemeSelection::FollowOs { dark, light } => {
            body.push(1);
            id(&mut body, dark);
            id(&mut body, light);
        }
    }
    string(&mut body, &cached.theme.name);
    body.push(if cached.theme.scheme == Scheme::Dark { 0 } else { 1 });
    for c in cached.theme.ui.0 {
        color(&mut body, c);
    }
    body.extend_from_slice(&(cached.theme.syntax.len() as u16).to_le_bytes());
    for (scope, style) in &cached.theme.syntax {
        string(&mut body, scope);
        for c in [style.foreground, style.background] {
            body.push(u8::from(c.is_some()));
            if let Some(c) = c {
                color(&mut body, c);
            }
        }
        body.push(style.font_style.unwrap_or(255));
    }
    if body.len() + HEADER > MAX_CACHE_BYTES {
        return Err(error("theme cache exceeds bound"));
    }
    let mut out = Vec::with_capacity(HEADER + body.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&hash(&body).to_le_bytes());
    out.extend(body);
    Ok(out)
}
struct Cursor<'a>(&'a [u8]);
impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ThemeError> {
        if n > self.0.len() {
            return Err(error("truncated theme cache"));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn byte(&mut self) -> Result<u8, ThemeError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<usize, ThemeError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()) as usize)
    }
    fn string(&mut self, max: usize) -> Result<String, ThemeError> {
        let len = self.u16()?;
        if len > max {
            return Err(error("oversized theme cache string"));
        }
        Ok(std::str::from_utf8(self.take(len)?).map_err(|_| error("cache string is not UTF-8"))?.into())
    }
    fn id(&mut self) -> Result<ThemeId, ThemeError> {
        match self.byte()? {
            0 => Ok(ThemeId::Dark),
            1 => Ok(ThemeId::Light),
            2 => ThemeId::user(&self.string(64)?),
            _ => Err(error("invalid cached theme id")),
        }
    }
    fn color(&mut self) -> Result<Rgba, ThemeError> {
        let c = self.take(4)?;
        Ok(Rgba(c[0], c[1], c[2], c[3]))
    }
    fn optional_color(&mut self) -> Result<Option<Rgba>, ThemeError> {
        match self.byte()? {
            0 => Ok(None),
            1 => Ok(Some(self.color()?)),
            _ => Err(error("invalid optional colour tag")),
        }
    }
}
pub(crate) fn decode(bytes: &[u8]) -> Result<CachedTheme, ThemeError> {
    if bytes.len() < HEADER || bytes.len() > MAX_CACHE_BYTES {
        return Err(error("invalid theme cache size"));
    }
    if &bytes[..8] != MAGIC || u16::from_le_bytes(bytes[8..10].try_into().unwrap()) != VERSION {
        return Err(error("unsupported theme cache version/magic"));
    }
    if u32::from_le_bytes(bytes[10..14].try_into().unwrap()) as usize != bytes.len() - HEADER {
        return Err(error("invalid cache length"));
    }
    let body = &bytes[HEADER..];
    if hash(body) != u64::from_le_bytes(bytes[14..22].try_into().unwrap()) {
        return Err(error("theme cache checksum mismatch"));
    }
    let mut c = Cursor(body);
    let selection = match c.byte()? {
        0 => ThemeSelection::Named(c.id()?),
        1 => ThemeSelection::FollowOs { dark: c.id()?, light: c.id()? },
        _ => return Err(error("invalid selection tag")),
    };
    let name = c.string(MAX_NAME_BYTES)?;
    let scheme = match c.byte()? {
        0 => Scheme::Dark,
        1 => Scheme::Light,
        _ => return Err(error("invalid cache scheme")),
    };
    let mut ui = UiColors([Rgba::rgb(0); UI_ROLE_COUNT]);
    for color in &mut ui.0 {
        *color = c.color()?;
    }
    let count = c.u16()?;
    if count > MAX_SCOPES {
        return Err(error("cache scope count exceeds bound"));
    }
    let mut syntax = BTreeMap::new();
    for _ in 0..count {
        let scope = c.string(MAX_SCOPE_BYTES)?;
        let foreground = c.optional_color()?;
        let background = c.optional_color()?;
        let font_style = match c.byte()? {
            255 => None,
            n @ 0..=15 => Some(n),
            _ => return Err(error("invalid cache font flags")),
        };
        if syntax.insert(scope, SyntaxStyle { foreground, background, font_style }).is_some() {
            return Err(error("duplicate cached scope"));
        }
    }
    if !c.0.is_empty() {
        return Err(error("trailing theme cache data"));
    }
    let theme = Theme { name, scheme, ui, syntax };
    theme.validate()?;
    Ok(CachedTheme { selection, theme })
}
/// Worker-only. No settings/registry changes or fallback into owner locations.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ThemeError> {
    atomic_write_with(path, bytes, replace)
}
pub(crate) fn atomic_write_new(path: &Path, bytes: &[u8]) -> Result<(), ThemeError> {
    atomic_write_with(path, bytes, |temp, path| {
        // Atomic create-if-absent on both supported filesystem families.
        // Unlike an exists()+rename pair this cannot overwrite a racing file.
        fs::hard_link(temp, path)?;
        fs::remove_file(temp)
    })
}
fn atomic_write_with(
    path: &Path,
    bytes: &[u8],
    publish: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> Result<(), ThemeError> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().ok_or_else(|| error("theme output needs a parent directory"))?;
    let filename = path.file_name().ok_or_else(|| error("theme output needs a filename"))?.to_string_lossy();
    let temp = parent.join(format!(".{filename}.{}-{}.tmp", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let mut created = false;
    let result = (|| {
        let mut file = fs::OpenOptions::new().create_new(true).write(true).open(&temp)?;
        created = true;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        publish(&temp, path)?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok::<_, ThemeError>(())
    })();
    // Only remove this call's own exclusive temp file, never an existing file
    // after create_new failed (e.g. a stale PID/counter collision).
    if result.is_err() && created {
        let _ = fs::remove_file(&temp);
    }
    result
}
fn replace(temp: &Path, path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};
        let temp: Vec<u16> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        if unsafe { MoveFileExW(temp.as_ptr(), path.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        fs::rename(temp, path)
    }
}

#[cfg(test)]
#[path = "theme_cache_tests.rs"]
mod tests;
