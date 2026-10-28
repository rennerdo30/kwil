//! Explorer paths and repair of existing Kwil registrations. No native hive is
//! selected here: the same planner/adapter boundary is exercised by fake tests.
use super::*;
use std::path::Path;

/// Do not pass canonicalize's Windows namespace prefix to Explorer. Reject
/// paths whose meaning would change without it, including MAX_PATH (with NUL).
pub fn explorer_executable(path: &Path) -> Result<String, AssociationError> {
    let raw = path.to_str().ok_or_else(|| err("Explorer executable path must be Unicode"))?;
    // The portable fake adapters also model Windows transactions on Unix hosts.
    #[cfg(not(windows))]
    if path.is_absolute() {
        return Ok(raw.to_owned());
    }
    let path = if raw.get(..8).is_some_and(|s| s.eq_ignore_ascii_case(r"\\?\UNC\")) {
        format!(r"\\{}", &raw[8..])
    } else {
        raw.strip_prefix(r"\\?\").unwrap_or(raw).replace('/', r"\")
    };
    let drive = path.as_bytes().get(..3).is_some_and(|p| p[0].is_ascii_alphabetic() && &p[1..] == b":\\");
    let parts = if drive { &path[3..] } else { path.strip_prefix(r"\\").unwrap_or("") };
    if (!drive && (!path.starts_with(r"\\") || parts.split('\\').count() < 3))
        || parts.split('\\').any(|part| {
            part.is_empty()
                || part.ends_with(['.', ' '])
                || part.chars().any(|c| c.is_control() || matches!(c, '"' | '%' | ':' | '<' | '>' | '|' | '?' | '*'))
                || reserved_name(part)
        })
    {
        return Err(err("Explorer cannot launch this executable path; move Kwil to a normal absolute path"));
    }
    if path.encode_utf16().count() >= 260 {
        return Err(err(
            "Explorer registration refused: executable path reaches 260 characters; move Kwil to a shorter folder",
        ));
    }
    Ok(path)
}

fn reserved_name(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|tail| {
                matches!(tail, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
            })
        })
}

pub fn is_build_executable(path: &Path) -> bool {
    let text = path.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
    let parts: Vec<_> = text.split('/').collect();
    parts.windows(2).any(|p| p[0] == "target" && matches!(p[1], "debug" | "release"))
        || parts.windows(3).any(|p| p[0] == "target" && matches!(p[2], "debug" | "release"))
        || (parts.contains(&"cargo-target") && parts.iter().any(|p| matches!(*p, "debug" | "release")))
}

fn root(key: &str) -> bool {
    key.starts_with("Kwil.") && !key.contains('\\') && crate::settings_ui::normalize_extension(&key[5..]).is_ok()
        || key == r"Applications\kwil.exe"
}

#[cfg(windows)]
pub(super) fn native_keys() -> Result<Vec<String>, AssociationError> {
    use windows_sys::Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS},
        System::Registry::*,
    };
    let key: Vec<_> = "Software\\Classes".encode_utf16().chain(Some(0)).collect();
    let mut handle = std::ptr::null_mut();
    let status = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, key.as_ptr(), 0, KEY_ENUMERATE_SUB_KEYS, &mut handle) };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(Vec::new());
    }
    if status != ERROR_SUCCESS {
        return Err(err(std::io::Error::from_raw_os_error(status as i32).to_string()));
    }
    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }
    let handle = Key(handle);
    let mut keys = Vec::new();
    // Registry key names are at most 255 UTF-16 code units. Bound enumeration
    // of foreign names too, without opening or reading any foreign handlers.
    for index in 0..16384 {
        let mut name = [0u16; 256];
        let mut len = name.len() as u32;
        let status = unsafe {
            RegEnumKeyExW(
                handle.0,
                index,
                name.as_mut_ptr(),
                &mut len,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if status == ERROR_NO_MORE_ITEMS {
            keys.push(r"Applications\kwil.exe".into());
            return Ok(keys);
        }
        if status != ERROR_SUCCESS {
            return Err(err("Cannot enumerate per-user Kwil registrations"));
        }
        if let Ok(name) = String::from_utf16(&name[..len as usize])
            && root(&name)
        {
            if keys.len() >= MAX_CHANGES - 1 {
                return Err(err("Too many Kwil registrations to repair"));
            }
            keys.push(name);
        }
    }
    Err(err("Per-user Classes enumeration exceeds repair limit"))
}

fn path_value(location: &Location) -> Option<bool> {
    let Location::WindowsValue { subkey, name } = location else { return None };
    if !name.is_empty() {
        return None;
    }
    for (suffix, icon) in [(r"\shell\open\command", false), (r"\DefaultIcon", true)] {
        if subkey.len() >= suffix.len() {
            let index = subkey.len() - suffix.len();
            if let (Some(key), Some(tail)) = (subkey.get(..index), subkey.get(index..))
                && tail.eq_ignore_ascii_case(suffix)
                && root(key)
            {
                return Some(icon);
            }
        }
    }
    None
}

fn registry_text(value: &StoredValue) -> Option<String> {
    if value.kind != 1 || !value.bytes.len().is_multiple_of(2) || value.bytes.len() > MAX_VALUE_BYTES {
        return None;
    }
    let mut units: Vec<_> = value.bytes.as_chunks::<2>().0.iter().map(|v| u16::from_le_bytes(*v)).collect();
    if units.pop() != Some(0) || units.contains(&0) {
        return None;
    }
    String::from_utf16(&units).ok()
}

fn replacement(
    location: &Location,
    value: &StoredValue,
    executable: &str,
    receipt_owned: bool,
    exists: &mut impl FnMut(&Path) -> Result<bool, AssociationError>,
    budget: &mut ValueBudget,
) -> Result<Option<StoredValue>, AssociationError> {
    let Some(icon) = path_value(location) else { return Ok(None) };
    let Some(text) = registry_text(value) else { return Ok(None) };
    let mut icon_index = 0;
    let parsed = if icon {
        let path = if let Some((path, index)) = text.rsplit_once(',') {
            let Ok(index) = index.parse::<i32>() else { return Ok(None) };
            icon_index = index;
            path
        } else {
            &text
        };
        path.trim_matches('"')
    } else {
        let Some(path) = text.strip_prefix('"').and_then(|s| s.strip_suffix(r#"" "%1""#)) else { return Ok(None) };
        path
    };
    // Without a receipt, a foreign executable inside a Kwil key is never
    // reclaimed. An exact receipt After match also owns renamed Kwil builds.
    if !receipt_owned && !parsed.rsplit(['\\', '/']).next().is_some_and(|s| s.eq_ignore_ascii_case("kwil.exe")) {
        return Ok(None);
    }
    if parsed.starts_with(r"\\?\") || !exists(Path::new(parsed))? {
        return budget
            .registry(&if icon { format!("\"{executable}\",{icon_index}") } else { format!("\"{executable}\" \"%1\"") })
            .map(Some);
    }
    Ok(None)
}

/// Run on a worker, never before the first frame. The immutable original journal
/// stays intact. Effective owned path bytes are derived again on restart so
/// Unregister recognizes repaired values and cannot restore a broken Kwil path.
/// Foreign Before bytes are restored exactly as originally recorded.
pub fn repair(
    adapter: &mut impl AssociationAdapter,
    executable: &Path,
    receipt: Option<&mut AssociationReceipt>,
) -> Result<usize, AssociationError> {
    repair_with(adapter, executable, receipt, &mut |path| match std::fs::metadata(path) {
        Ok(m) => Ok(m.is_file()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(err(format!("Cannot inspect registered executable: {e}"))),
    })
}

fn repair_with(
    adapter: &mut impl AssociationAdapter,
    executable: &Path,
    receipt: Option<&mut AssociationReceipt>,
    exists: &mut impl FnMut(&Path) -> Result<bool, AssociationError>,
) -> Result<usize, AssociationError> {
    let _admission = Admission::acquire(WORKSPACE_BYTES)?;
    let executable = explorer_executable(executable)?;
    let mut keys = adapter.windows_registration_keys()?;
    if keys.len() > MAX_CHANGES {
        return Err(err("Too many Kwil registrations to repair"));
    }
    keys.retain(|key| root(key));
    let mut budget = ValueBudget::new(worker::MAX_RECORD_BYTES);
    let mut repaired = 0;
    if let Some(receipt) = receipt {
        if receipt.owner != adapter.owner() {
            return Err(err("Cannot repair another user's receipt"));
        }
        let (mut changes, admission) = receipt.remaining.copied()?;
        for change in &mut changes {
            if let Some(before) = &change.before
                && let Some(new) = replacement(&change.location, before, &executable, false, exists, &mut budget)?
            {
                let current = adapter.read_bounded(&change.location, &mut budget)?;
                if current == change.before && current.as_ref() != Some(&new) {
                    adapter.write(&change.location, Some(&new))?;
                    repaired += 1;
                }
                change.before = Some(new);
            }
            if let Some(after) = &change.after
                && let Some(new) = replacement(&change.location, after, &executable, true, exists, &mut budget)?
            {
                let current = adapter.read_bounded(&change.location, &mut budget)?;
                if current == change.after && current.as_ref() != Some(&new) {
                    adapter.write(&change.location, Some(&new))?;
                    repaired += 1;
                    change.after = Some(new);
                } else if current.as_ref() == Some(&new) {
                    change.after = Some(new);
                }
            }
        }
        receipt.remaining = SharedChanges::admitted(changes, admission);
    }
    for key in keys {
        for suffix in [r"\shell\open\command", r"\DefaultIcon"] {
            let location = Location::WindowsValue { subkey: format!("{key}{suffix}"), name: String::new() };
            if let Some(old) = adapter.read_bounded(&location, &mut budget)?
                && let Some(new) = replacement(&location, &old, &executable, false, exists, &mut budget)?
                && old != new
                && adapter.read_bounded(&location, &mut budget)?.as_ref() == Some(&old)
            {
                adapter.write(&location, Some(&new))?;
                repaired += 1;
            }
        }
    }
    if repaired != 0 {
        adapter.notify_changes();
    }
    Ok(repaired)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Hive {
        values: Vec<(Location, StoredValue)>,
        writes: usize,
    }
    impl AssociationAdapter for Hive {
        fn owner(&self) -> String {
            "fake-user".into()
        }
        fn windows_registration_keys(&mut self) -> Result<Vec<String>, AssociationError> {
            Ok(vec!["Kwil.txt".into(), "Foreign.txt".into(), r"Applications\kwil.exe".into()])
        }
        fn read(&mut self, location: &Location) -> Result<Option<StoredValue>, AssociationError> {
            self.read_bounded(location, &mut ValueBudget::new(MAX_VALUE_BYTES))
        }
        fn read_bounded(
            &mut self,
            location: &Location,
            budget: &mut ValueBudget,
        ) -> Result<Option<StoredValue>, AssociationError> {
            self.values
                .iter()
                .find(|(key, _)| key == location)
                .map(|(_, value)| budget.copy(value.kind, &value.bytes))
                .transpose()
        }
        fn write(&mut self, location: &Location, value: Option<&StoredValue>) -> Result<(), AssociationError> {
            self.writes += 1;
            self.values.retain(|(key, _)| key != location);
            if let Some(value) = value {
                self.values.push((location.clone(), value.clone()));
            }
            Ok(())
        }
        fn sync_restored(&mut self, _: &[Change]) -> Result<(), AssociationError> {
            Ok(())
        }
    }
    fn command(root: &str) -> Location {
        Location::WindowsValue { subkey: format!(r"{root}\shell\open\command"), name: String::new() }
    }
    fn value(text: &str) -> StoredValue {
        ValueBudget::new(MAX_VALUE_BYTES).registry(text).unwrap()
    }
    #[test]
    fn explorer_repair_verbatim_and_missing_commands_with_foreign_handlers_untouched() {
        let _guard = test_admission_guard();
        for old in [r#""\\?\E:\old\kwil.exe" "%1""#, r#""E:\gone\kwil.exe" "%1""#] {
            let owned = command("Kwil.txt");
            let foreign = command("Foreign.txt");
            let foreign_value = value(r#""\\?\E:\gone\kwil.exe" "%1""#);
            let mut hive = Hive {
                values: vec![(owned.clone(), value(old)), (foreign.clone(), foreign_value.clone())],
                ..Default::default()
            };
            let count = repair_with(&mut hive, Path::new(r"\\?\E:\new\kwil.exe"), None, &mut |_| Ok(false)).unwrap();
            assert_eq!(count, 1);
            assert_eq!(hive.read(&owned).unwrap(), Some(value(r#""E:\new\kwil.exe" "%1""#)));
            assert_eq!(hive.read(&foreign).unwrap(), Some(foreign_value));
            let writes = hive.writes;
            assert_eq!(repair_with(&mut hive, Path::new(r"E:\new\kwil.exe"), None, &mut |_| Ok(true)).unwrap(), 0);
            assert_eq!(hive.writes, writes);
        }
    }
    #[test]
    fn explorer_repair_icons_applications_and_receipts_preserves_undo() {
        let _guard = test_admission_guard();
        let owned = command("Kwil.txt");
        let old = value(r#""\\?\E:\old\kwil.exe" "%1""#);
        let icon = Location::WindowsValue { subkey: r"Kwil.txt\DefaultIcon".into(), name: String::new() };
        let app = command(r"Applications\kwil.exe");
        let before = value("original owner description");
        let mut receipt = AssociationReceipt {
            owner: "fake-user".into(),
            remaining: SharedChanges::admitted(
                vec![Change { location: owned.clone(), before: Some(before.clone()), after: Some(old.clone()) }],
                Admission::acquire(WORKSPACE_BYTES).unwrap(),
            ),
        };
        let saved = serde_json::to_vec(&receipt).unwrap();
        let mut hive = Hive {
            values: vec![
                (owned.clone(), old.clone()),
                (icon.clone(), value(r#""\\?\UNC\server\share\kwil.exe",0"#)),
                (app.clone(), old),
            ],
            ..Default::default()
        };
        assert_eq!(
            repair_with(&mut hive, Path::new(r"E:\new\kwil.exe"), Some(&mut receipt), &mut |_| Ok(false)).unwrap(),
            3
        );
        assert_eq!(hive.read(&icon).unwrap(), Some(value(r#""E:\new\kwil.exe",0"#)));
        assert_eq!(hive.read(&app).unwrap(), Some(value(r#""E:\new\kwil.exe" "%1""#)));
        // The immutable original journal can reconstruct the effective After on restart.
        let mut restart: AssociationReceipt = serde_json::from_slice(&saved).unwrap();
        repair_with(&mut hive, Path::new(r"E:\new\kwil.exe"), Some(&mut restart), &mut |_| Ok(false)).unwrap();
        assert_eq!(restart.pending(), receipt.pending());
        assert!(rollback(&mut hive, &mut restart).is_empty());
        assert_eq!(hive.read(&owned).unwrap(), Some(before));
    }
    #[test]
    fn explorer_repair_keeps_existing_exe_foreign_replacements_and_unowned_receipts() {
        let _guard = test_admission_guard();
        let owned = command("Kwil.txt");
        for text in
            [r#""E:\valid\kwil.exe" "%1""#, r#""\\?\E:\gone\other.exe" "%1""#, r#""E:\gone\kwil.exe" --foreign "%1""#]
        {
            let old = value(text);
            let mut hive = Hive { values: vec![(owned.clone(), old.clone())], ..Default::default() };
            repair_with(&mut hive, Path::new(r"E:\new\kwil.exe"), None, &mut |_| Ok(true)).unwrap();
            assert_eq!(hive.read(&owned).unwrap(), Some(old));
            assert_eq!(hive.writes, 0);
        }
        let old = value(r#""\\?\E:\old\kwil.exe" "%1""#);
        let foreign = value(r#""E:\foreign\other.exe" "%1""#);
        let mut receipt = AssociationReceipt {
            owner: "fake-user".into(),
            remaining: SharedChanges::admitted(
                vec![Change { location: owned.clone(), before: None, after: Some(old.clone()) }],
                Admission::acquire(WORKSPACE_BYTES).unwrap(),
            ),
        };
        let mut hive = Hive { values: vec![(owned.clone(), foreign.clone())], ..Default::default() };
        repair_with(&mut hive, Path::new(r"E:\new\kwil.exe"), Some(&mut receipt), &mut |_| Ok(false)).unwrap();
        assert_eq!(hive.read(&owned).unwrap(), Some(foreign));
        assert_eq!(receipt.pending()[0].after, Some(old));
        receipt.owner = "another user".into();
        assert!(repair_with(&mut hive, Path::new(r"E:\new\kwil.exe"), Some(&mut receipt), &mut |_| Ok(false)).is_err());
        assert_eq!(hive.writes, 0);
    }
    #[test]
    fn explorer_path_semantics_and_build_folder_warning() {
        for (input, expected) in [
            (r"\\?\E:\Kwil builds\kwil.exe", r"E:\Kwil builds\kwil.exe"),
            (r"\\?\UNC\server\share\kwil.exe", r"\\server\share\kwil.exe"),
        ] {
            assert_eq!(explorer_executable(Path::new(input)).unwrap(), expected);
        }
        for input in [
            r"\\?\Volume{abc}\kwil.exe",
            r"\\.\E:\kwil.exe",
            r"\\?\E:\folder.\kwil.exe",
            r"E:\NUL\kwil.exe",
            r"E:\..\kwil.exe",
            r"E:kwil.exe",
        ] {
            assert!(explorer_executable(Path::new(input)).is_err(), "{input}");
        }
        for input in [
            r"E:\project\target\debug\kwil.exe",
            r"E:\project\target\x86_64-pc-windows-msvc\release\kwil.exe",
            r"E:\_cache\cargo-target\kwil\release\kwil.exe",
        ] {
            assert!(is_build_executable(Path::new(input)), "{input}");
        }
        assert!(!is_build_executable(Path::new(r"E:\Apps\Kwil\kwil.exe")));
    }
    #[test]
    fn explorer_unregister_never_restores_a_broken_owned_command() {
        let _guard = test_admission_guard();
        let location = command("Kwil.txt");
        let mut receipt = AssociationReceipt {
            owner: "fake-user".into(),
            remaining: SharedChanges::admitted(
                vec![Change {
                    location: location.clone(),
                    before: Some(value(r#""\\?\E:\older\kwil.exe" "%1""#)),
                    after: Some(value(r#""\\?\E:\old\kwil.exe" "%1""#)),
                }],
                Admission::acquire(WORKSPACE_BYTES).unwrap(),
            ),
        };
        let mut hive = Hive {
            values: vec![(location.clone(), receipt.pending()[0].after.clone().unwrap())],
            ..Default::default()
        };
        repair_with(&mut hive, Path::new(r"E:\new\kwil.exe"), Some(&mut receipt), &mut |_| Ok(false)).unwrap();
        assert!(rollback(&mut hive, &mut receipt).is_empty());
        assert_eq!(hive.read(&location).unwrap(), Some(value(r#""E:\new\kwil.exe" "%1""#)));
    }
    #[cfg(windows)]
    #[test]
    fn explorer_repair_uses_missing_file_metadata_against_fake_hive() {
        let _guard = test_admission_guard();
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let missing = std::env::temp_dir()
            .join(format!("kwil-missing-assoc-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)))
            .join("kwil.exe");
        let location = command("Kwil.txt");
        let mut hive = Hive {
            values: vec![(location.clone(), value(&format!("\"{}\" \"%1\"", missing.display())))],
            ..Default::default()
        };
        assert_eq!(repair(&mut hive, Path::new(r"E:\new\kwil.exe"), None).unwrap(), 1);
        assert_eq!(hive.read(&location).unwrap(), Some(value(r#""E:\new\kwil.exe" "%1""#)));
    }
}
