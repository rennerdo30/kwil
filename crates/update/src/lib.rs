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
//! - The existing settings select stable (default) or prerelease updates.
//!   Drafts are never offered. Stable never installs a prerelease.
//!
//! Not yet: signature verification (needs a signing key from the owner,
//! KW-5).

use kwil_core_commands::settings::{Settings, UpdateChannel, UpdateSettings};
use serde::Deserialize;
#[cfg(not(windows))]
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
const RELEASES_PER_PAGE: usize = 100;
const MAX_RELEASE_PAGES: usize = 10;
const MAX_METADATA: u64 = 8 << 20;

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
    draft: bool,
    prerelease: bool,
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
        let settings = match load_update_settings() {
            Ok(settings) if settings.check => settings,
            Ok(_) => return,
            Err(error) => {
                notify(format!("Update check failed: {error}"));
                return;
            }
        };
        let stamp = state_dir().map(|d| {
            d.join(match settings.channel {
                UpdateChannel::Stable => "last-update-check",
                UpdateChannel::Prerelease => "last-prerelease-update-check",
            })
        });
        if let Some(s) = &stamp
            && !due(s)
        {
            return;
        }
        let result = check_and_install_with_settings(current, &settings);
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
    check_and_install_with_settings(current, &load_update_settings()?)
}

fn load_update_settings() -> Result<UpdateSettings, Error> {
    let path = Settings::path(&std::env::current_exe()?)?;
    Ok(Settings::load(&path)?.settings.update)
}

fn check_and_install_with_settings(current: &str, settings: &UpdateSettings) -> Result<Outcome, Error> {
    if !settings.check {
        return Err(Error("updates are off (update.check = false)".into()));
    }
    let agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(60))).build().new_agent();
    let ua = format!("kwil/{current}");
    let metadata_deadline = Instant::now() + Duration::from_secs(60);
    let release = find_release(current, settings.channel, |path| {
        let remaining = metadata_deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| Error("release metadata deadline expired".into()))?;
        let mut response = match agent
            .get(&format!("https://api.github.com/repos/{REPO}/{path}"))
            .header("User-Agent", &ua)
            .header("Accept", "application/vnd.github+json")
            .config()
            .timeout_global(Some(remaining))
            .build()
            .call()
        {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(404)) if path == "releases/latest" => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let body = response.body_mut().with_config().limit(MAX_METADATA);
        let releases = if path == "releases/latest" { vec![body.read_json()?] } else { body.read_json()? };
        if Instant::now() >= metadata_deadline {
            return Err(Error("release metadata deadline expired".into()));
        }
        Ok(releases)
    })?;
    let Some(release) = release else {
        return Ok(Outcome::UpToDate);
    };
    let latest = release.tag_name.strip_prefix('v').unwrap_or(&release.tag_name);
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

// List order is publication order, not version order. Scan bounded pages and
// refuse an incomplete scan rather than installing an arbitrary older result.
fn find_release(
    current: &str,
    channel: UpdateChannel,
    mut fetch: impl FnMut(&str) -> Result<Vec<Release>, Error>,
) -> Result<Option<Release>, Error> {
    let current = parse(current).ok_or_else(|| Error("invalid current update version".into()))?;
    let mut best: Option<Release> = None;
    for page in 1..=MAX_RELEASE_PAGES {
        let releases = fetch(&match channel {
            UpdateChannel::Stable => "releases/latest".into(),
            UpdateChannel::Prerelease => format!("releases?per_page={RELEASES_PER_PAGE}&page={page}"),
        })?;
        let complete = channel == UpdateChannel::Stable || releases.len() < RELEASES_PER_PAGE;
        if releases.len() > RELEASES_PER_PAGE {
            return Err(Error("release page exceeds its requested limit".into()));
        }
        for release in releases {
            let tag = release.tag_name.strip_prefix('v').unwrap_or(&release.tag_name);
            let Some(version) = parse(tag) else { continue };
            if release.draft
                || release.prerelease != version.pre.is_some()
                || channel == UpdateChannel::Stable && release.prerelease
                || version.cmp(&current) != Ordering::Greater
            {
                continue;
            }
            if best.as_ref().is_none_or(|old| {
                let old_tag = old.tag_name.strip_prefix('v').unwrap_or(&old.tag_name);
                version.cmp(&parse(old_tag).expect("selected valid release")) == Ordering::Greater
            }) {
                best = Some(release);
            }
        }
        if complete {
            return Ok(best);
        }
    }
    Err(Error("release history exceeds the bounded update scan".into()))
}

/// Stable-channel comparison. A beta current version can advance to stable;
/// a prerelease candidate requires the explicit prerelease channel.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse(latest), parse(current)) {
        (Some(l), Some(c)) => l.pre.is_none() && l.cmp(&c) == Ordering::Greater,
        _ => false,
    }
}

#[derive(Debug)]
struct Version<'a> {
    core: (u64, u64, u64),
    pre: Option<&'a str>,
}

impl Version<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core.cmp(&other.core).then_with(|| match (self.pre, other.pre) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(a), Some(b)) => {
                let mut a = a.split('.');
                let mut b = b.split('.');
                loop {
                    let order = match (a.next(), b.next()) {
                        (None, None) => return Ordering::Equal,
                        (None, Some(_)) => return Ordering::Less,
                        (Some(_), None) => return Ordering::Greater,
                        (Some(x), Some(y)) => match (numeric(x), numeric(y)) {
                            (true, true) => x.len().cmp(&y.len()).then_with(|| x.cmp(y)),
                            (true, false) => Ordering::Less,
                            (false, true) => Ordering::Greater,
                            (false, false) => x.cmp(y),
                        },
                    };
                    if order != Ordering::Equal {
                        return order;
                    }
                }
            }
        })
    }
}

fn numeric(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit())
}

fn identifiers(s: &str, prerelease: bool) -> bool {
    s.split('.').all(|id| {
        !id.is_empty()
            && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
            && !(prerelease && numeric(id) && id.len() > 1 && id.starts_with('0'))
    })
}

fn parse(v: &str) -> Option<Version<'_>> {
    if v.len() > 256 {
        return None;
    }
    let base = if let Some((base, build)) = v.split_once('+') {
        if !identifiers(build, false) {
            return None;
        }
        base
    } else {
        v
    };
    let (core, pre) = if let Some((core, pre)) = base.split_once('-') {
        if !identifiers(pre, true) {
            return None;
        }
        (core, Some(pre))
    } else {
        (base, None)
    };
    let mut it = core.split('.');
    let mut number = || {
        let s = it.next()?;
        if !numeric(s) || s.len() > 1 && s.starts_with('0') {
            return None;
        }
        s.parse::<u64>().ok()
    };
    let core = (number()?, number()?, number()?);
    it.next().is_none().then_some(Version { core, pre })
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
        assert!(is_newer("0.1.0", "0.1.0-beta.10"));
        assert!(!is_newer("0.1.0-beta.11", "0.1.0-beta.10"));
    }

    fn release(tag: &str, draft: bool, prerelease: bool) -> Release {
        Release { tag_name: tag.into(), draft, prerelease, assets: Vec::new() }
    }

    #[test]
    fn semver_precedence_and_malformed_versions() {
        let ordered = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
        ];
        for pair in ordered.windows(2) {
            assert_eq!(parse(pair[0]).unwrap().cmp(&parse(pair[1]).unwrap()), Ordering::Less);
        }
        assert_eq!(parse("1.0.0+build.2").unwrap().cmp(&parse("1.0.0+build.1").unwrap()), Ordering::Equal);
        assert_eq!(
            parse("1.0.0-beta.999999999999999999999999").unwrap().cmp(&parse("1.0.0-beta.10").unwrap()),
            Ordering::Greater
        );
        for bad in [
            "",
            "v1.0.0",
            "01.0.0",
            "1.0",
            "1.0.0.1",
            "1.0.0-01",
            "1.0.0-",
            "1.0.0+",
            "1.0.0+a+b",
            "1.0.0-beta..1",
            "1.0.0-beta_1",
            "1.0.0\n",
            "+1.0.0",
            "18446744073709551616.0.0",
        ] {
            assert!(parse(bad).is_none(), "accepted {bad:?}");
        }
    }

    #[test]
    fn stable_channel_rejects_drafts_prereleases_and_inconsistent_labels() {
        for candidate in [
            release("v2.0.0", true, false),
            release("v2.0.0-beta.1", false, true),
            release("v2.0.0-beta.1", false, false),
            release("v2.0.0", false, true),
        ] {
            let mut candidate = Some(candidate);
            assert!(
                find_release("1.0.0", UpdateChannel::Stable, |path| {
                    assert_eq!(path, "releases/latest");
                    Ok(vec![candidate.take().expect("one fetch")])
                })
                .unwrap()
                .is_none()
            );
        }
        let found = find_release("1.0.0-beta.1", UpdateChannel::Stable, |_| Ok(vec![release("v1.0.0", false, false)]))
            .unwrap()
            .unwrap();
        assert_eq!(found.tag_name, "v1.0.0");
        assert!(find_release("1.0.0", UpdateChannel::Stable, |_| Ok(Vec::new())).unwrap().is_none());
    }

    #[test]
    fn prerelease_channel_uses_version_order_across_pages_then_graduates() {
        let mut pages = 0;
        let found = find_release("0.1.0-beta.9", UpdateChannel::Prerelease, |path| {
            pages += 1;
            assert_eq!(path, format!("releases?per_page=100&page={pages}"));
            if pages == 1 {
                let mut releases = (0..99).map(|_| release("v0.1.0-beta.2", false, true)).collect::<Vec<_>>();
                releases.push(release("v0.1.0-beta.10", false, true));
                Ok(releases)
            } else {
                Ok(vec![
                    release("v0.1.0-beta.99", true, true),
                    release("v0.1.0-beta.11", false, true),
                    release("v0.1.0", false, false),
                ])
            }
        })
        .unwrap()
        .unwrap();
        assert_eq!(pages, 2);
        assert_eq!(found.tag_name, "v0.1.0");
        let found = find_release("0.1.0-beta.9", UpdateChannel::Prerelease, |_| {
            Ok(vec![release("v0.1.0-beta.10", false, true), release("v0.1.0-beta.2", false, true)])
        })
        .unwrap()
        .unwrap();
        assert_eq!(found.tag_name, "v0.1.0-beta.10");
        assert!(
            find_release("0.1.0", UpdateChannel::Prerelease, |_| Ok(vec![release("v0.1.0-beta.99", false, true)]))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn incomplete_scan_and_fetch_failures_never_select_an_update() {
        let mut calls = 0;
        let error = find_release("0.1.0", UpdateChannel::Prerelease, |_| {
            calls += 1;
            Ok((0..100).map(|_| release("v1.0.0", false, false)).collect())
        })
        .err()
        .unwrap();
        assert_eq!(calls, MAX_RELEASE_PAGES);
        assert!(error.0.contains("bounded update scan"));
        let error =
            find_release("0.1.0", UpdateChannel::Prerelease, |_| Err(Error("fetch failed".into()))).err().unwrap();
        assert_eq!(error.0, "fetch failed");
        assert!(
            find_release("invalid", UpdateChannel::Prerelease, |_| panic!("invalid current version must not fetch"))
                .is_err()
        );
    }

    #[test]
    fn existing_settings_control_network_admission_and_channel() {
        let disabled = Settings::parse("[update]\ncheck = false\nchannel = 'prerelease'\n").unwrap().settings.update;
        assert_eq!(disabled.channel, UpdateChannel::Prerelease);
        assert_eq!(
            check_and_install_with_settings("0.1.0-beta.1", &disabled).unwrap_err().0,
            "updates are off (update.check = false)"
        );
        let defaults = Settings::parse("").unwrap().settings.update;
        assert!(defaults.check);
        assert_eq!(defaults.channel, UpdateChannel::Stable);
    }

    #[test]
    fn checksum_lookup_matches_exact_names() {
        let sums = "AB12  kwil-x86_64-pc-windows-msvc.exe\ncd34 *kwil-x86_64-unknown-linux-gnu\n";
        assert_eq!(expected_sha256(sums, "kwil-x86_64-pc-windows-msvc.exe").as_deref(), Some("ab12"));
        assert_eq!(expected_sha256(sums, "kwil-x86_64-unknown-linux-gnu").as_deref(), Some("cd34"));
        assert_eq!(expected_sha256(sums, "kwil-x86_64"), None);
    }
}
