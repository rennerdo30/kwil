//! Auto-update from GitHub Releases (owner wish KW-5, spec §7).
//!
//! - Network checks never run on the startup path: frontends call
//!   `spawn_background` after the first frame, at most once a day. Windows
//!   startup checks only the local update lock and recovery journal.
//! - Releases carry `kwil-<target>[.exe]`, Windows `.com`, and `SHA256SUMS`.
//!   Windows installs a verified pair with backups and journal recovery;
//!   other platforms replace one binary. The running editor keeps running;
//!   the next start uses the new version. Unsaved work is never touched.
//! - Off when `KWIL_NO_UPDATE` is set, when a `kwil.no-update` file sits next
//!   to the executable (package managers, portable installs), in debug
//!   builds, and for version 0.0.0.
//! - Drafts and prereleases are never offered (`/releases/latest`).
//!
//! Not yet: signature verification (needs a signing key from the owner,
//! KW-5) and a prerelease channel setting.

use serde::Deserialize;
#[cfg(not(windows))]
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(any(windows, test))]
mod transaction;

/// Check the Windows update lock and recover an interrupted pair before any
/// CLI or frontend work. A repaired startup asks for a restart because the
/// Windows loader may already have mapped one of the interrupted images.
pub fn recover_on_startup() -> Result<(), Error> {
    #[cfg(windows)]
    {
        let exe = std::env::current_exe()?;
        transaction::startup(exe.parent().ok_or_else(|| Error("executable has no folder".into()))?)?;
    }
    Ok(())
}

pub const REPO: &str = "rennerdo30/kwil";
const TARGET: &str = env!("KWIL_TARGET");
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_DOWNLOAD: u64 = 256 << 20;

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    UpToDate,
    /// Installed; takes effect on the next start.
    Installed(String),
}

#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<E: std::error::Error> From<E> for Error {
    fn from(e: E) -> Self {
        Error(e.to_string())
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// Name of this platform's release asset.
pub fn asset_name() -> String {
    let ext = if TARGET.contains("windows") { ".exe" } else { "" };
    format!("kwil-{TARGET}{ext}")
}

/// Why updates are off for this process, if they are.
pub fn disabled_reason(current: &str) -> Option<&'static str> {
    if cfg!(debug_assertions) {
        return Some("debug build");
    }
    if current == "0.0.0" {
        return Some("development version");
    }
    if std::env::var_os("KWIL_NO_UPDATE").is_some() {
        return Some("KWIL_NO_UPDATE is set");
    }
    let marker = std::env::current_exe().ok().and_then(|p| Some(p.parent()?.join("kwil.no-update")));
    if marker.is_some_and(|m| m.exists()) {
        return Some("kwil.no-update next to the executable");
    }
    None
}

/// Checks at most once per day on a background thread and reports an
/// installed update (or a failure) through `notify`. Silent when up to date.
pub fn spawn_background(current: &'static str, notify: impl Fn(String) + Send + 'static) {
    if disabled_reason(current).is_some() {
        return;
    }
    std::thread::spawn(move || {
        let stamp = state_dir().map(|d| d.join("last-update-check"));
        if let Some(s) = &stamp
            && !due(s)
        {
            return;
        }
        let result = check_and_install(current);
        if let Some(s) = &stamp {
            let _ = std::fs::create_dir_all(s.parent().unwrap());
            let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let _ = std::fs::write(s, now.to_string());
        }
        match result {
            Ok(Outcome::Installed(v)) => notify(format!("Updated to {v}; restart Kwil to use it")),
            Ok(Outcome::UpToDate) => {}
            Err(e) => notify(format!("Update check failed: {e}")),
        }
    });
}

/// Checks GitHub for a newer release and installs it. Blocking; used by
/// `spawn_background` and by `kwil --update`.
pub fn check_and_install(current: &str) -> Result<Outcome, Error> {
    let agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(60))).build().new_agent();
    let ua = format!("kwil/{current}");
    let release: Release = agent
        .get(&format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("User-Agent", &ua)
        .header("Accept", "application/vnd.github+json")
        .call()?
        .body_mut()
        .read_json()?;
    let latest = release.tag_name.trim_start_matches('v');
    if !is_newer(latest, current) {
        return Ok(Outcome::UpToDate);
    }
    let want = asset_name();
    let url = |name: &str| release.assets.iter().find(|a| a.name == name).map(|a| a.browser_download_url.clone());
    let bin_url = url(&want).ok_or_else(|| Error(format!("release {latest} has no {want}")))?;
    let sums_url = url("SHA256SUMS").ok_or_else(|| Error(format!("release {latest} has no SHA256SUMS")))?;

    let download = |u: &str| -> Result<Vec<u8>, Error> {
        Ok(agent.get(u).header("User-Agent", &ua).call()?.body_mut().with_config().limit(MAX_DOWNLOAD).read_to_vec()?)
    };
    let sums = String::from_utf8(download(&sums_url)?)?;
    let bytes = download(&bin_url)?;
    #[cfg(windows)]
    {
        let console_name = want.trim_end_matches(".exe").to_owned() + ".com";
        let console_url = url(&console_name).ok_or_else(|| Error(format!("release {latest} has no {console_name}")))?;
        let console = download(&console_url)?;
        let exe = std::env::current_exe()?;
        let name = exe.file_name().and_then(|v| v.to_str()).unwrap_or_default();
        if !name.eq_ignore_ascii_case("kwil.exe") && !name.eq_ignore_ascii_case("kwil.com") {
            return Err(Error("install the Windows pair as kwil.exe and kwil.com before updating".into()));
        }
        let dir = exe.parent().ok_or_else(|| Error("executable has no folder".into()))?;
        transaction::install(dir, [(&want, &bytes), (&console_name, &console)], &sums)?;
    }
    #[cfg(not(windows))]
    {
        let expected = expected_sha256(&sums, &want).ok_or_else(|| Error(format!("SHA256SUMS lists no {want}")))?;
        let actual = hex(&Sha256::digest(&bytes));
        if actual != expected {
            return Err(Error(format!("checksum mismatch for {want}: expected {expected}, got {actual}")));
        }
        install(&bytes)?;
    }
    Ok(Outcome::Installed(format!("v{latest}")))
}

#[cfg(not(windows))]
fn install(bytes: &[u8]) -> Result<(), Error> {
    let exe = std::env::current_exe()?;
    let dir = exe.parent().ok_or_else(|| Error("executable has no folder".into()))?;
    let tmp = dir.join(format!(".kwil-update-{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    let r = self_replace::self_replace(&tmp);
    let _ = std::fs::remove_file(&tmp);
    r?;
    Ok(())
}

/// `true` when `latest` is a newer plain `x.y.z` release than `current`.
/// Prerelease tags (`1.2.0-beta`) are never newer.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse(latest), parse(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.split('.');
    let t = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    it.next().is_none().then_some(t)
}

/// Finds `name` in a `sha256sum`-style file (`<hex>  <name>` or `<hex> *<name>`).
pub fn expected_sha256(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (hash, file) = l.split_once(char::is_whitespace)?;
        (file.trim_start().trim_start_matches('*') == name).then(|| hash.to_ascii_lowercase())
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn due(stamp: &Path) -> bool {
    let Ok(s) = std::fs::read_to_string(stamp) else { return true };
    let Ok(last) = s.trim().parse::<u64>() else { return true };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    now.saturating_sub(last) >= CHECK_INTERVAL.as_secs()
}

/// Per-user state folder: `%LOCALAPPDATA%\kwil`, `~/Library/Caches/kwil` or
/// `$XDG_CACHE_HOME/kwil` (`~/.cache/kwil`).
fn state_dir() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let base = if cfg!(windows) {
        env("LOCALAPPDATA")?
    } else if cfg!(target_os = "macos") {
        env("HOME")?.join("Library/Caches")
    } else {
        env("XDG_CACHE_HOME").or_else(|| Some(env("HOME")?.join(".cache")))?
    };
    Some(base.join("kwil"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically_and_skip_prereleases() {
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("0.2.0-beta", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }

    #[test]
    fn checksum_lookup_matches_exact_names() {
        let sums = "AB12  kwil-x86_64-pc-windows-msvc.exe\ncd34 *kwil-x86_64-unknown-linux-gnu\n";
        assert_eq!(expected_sha256(sums, "kwil-x86_64-pc-windows-msvc.exe").as_deref(), Some("ab12"));
        assert_eq!(expected_sha256(sums, "kwil-x86_64-unknown-linux-gnu").as_deref(), Some("cd34"));
        assert_eq!(expected_sha256(sums, "kwil-x86_64"), None);
    }
}
