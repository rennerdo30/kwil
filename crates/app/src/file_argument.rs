//! Parse position suffixes without converting native file names to Unicode.
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileArgument {
    pub path: PathBuf,
    pub line: Option<u64>,
    pub column: Option<u64>,
}

impl FileArgument {
    pub fn parse(argument: OsString, literal: bool) -> Result<Self, String> {
        let bytes = argument.as_encoded_bytes();
        if bytes.is_empty() {
            return Err("an empty file name cannot be opened".into());
        }
        let mut end = bytes.len();
        let mut line = None;
        let mut column = None;
        if !literal && let Some((prefix, number)) = numeric_suffix(bytes)? {
            end = prefix;
            line = Some(number);
            if let Some((prefix, number)) = numeric_suffix(&bytes[..end])? {
                end = prefix;
                column = line;
                line = Some(number);
            }
        }
        if end == 0 {
            return Err("a position requires a file name".into());
        }
        // SAFETY: the prefix comes from this OsString and ends immediately before
        // an ASCII colon. ASCII boundaries are self-synchronizing in the platform
        // encoding; no Unicode or native code unit is split or reconstructed.
        let path = unsafe { OsStr::from_encoded_bytes_unchecked(&bytes[..end]) };
        Ok(Self { path: PathBuf::from(path), line, column })
    }

    /// Lexical resolution keeps nonexistent/new files and native spelling intact.
    /// Canonicalization would perform I/O and reject a valid new document path.
    pub fn resolve(mut self, directory: &Path) -> Result<Self, String> {
        if !directory.is_absolute() {
            return Err("the working directory must be absolute".into());
        }
        if !self.path.is_absolute() {
            self.path = directory.join(self.path);
        }
        if !self.path.is_absolute() {
            return Err("the file path could not be resolved absolutely".into());
        }
        Ok(self)
    }
}

fn numeric_suffix(bytes: &[u8]) -> Result<Option<(usize, u64)>, String> {
    let Some(colon) = bytes.iter().rposition(|&byte| byte == b':') else { return Ok(None) };
    // A Windows drive-relative path (C:123) is a path, not a line suffix.
    if cfg!(windows) && colon == 1 && bytes[0].is_ascii_alphabetic() {
        return Ok(None);
    }
    let digits = &bytes[colon + 1..];
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Ok(None);
    }
    let number = digits
        .iter()
        .try_fold(0u64, |number, &digit| number.checked_mul(10)?.checked_add(u64::from(digit - b'0')))
        .ok_or_else(|| "file position exceeds the supported 64-bit range".to_string())?;
    if number == 0 {
        return Err("file line and column positions start at 1".into());
    }
    Ok(Some((colon, number)))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(value: &str) -> FileArgument {
        FileArgument::parse(value.into(), false).unwrap()
    }

    #[test]
    fn cli_positions_keep_native_paths_and_positive_coordinates() {
        assert_eq!(
            parse("folder with spaces/file.rs:120:7"),
            FileArgument { path: "folder with spaces/file.rs".into(), line: Some(120), column: Some(7) }
        );
        assert_eq!(parse("file:20").column, None);
        assert_eq!(parse("file:20").line, Some(20));
        assert_eq!(parse("file:label").path, Path::new("file:label"));
        assert!(FileArgument::parse("file:0".into(), false).is_err());
        assert!(FileArgument::parse("file:1:0".into(), false).is_err());
        assert!(FileArgument::parse("file:18446744073709551616".into(), false).is_err());
        assert!(FileArgument::parse(OsString::new(), false).is_err());
        assert_eq!(FileArgument::parse("file:20:7".into(), true).unwrap().path, Path::new("file:20:7"));
        #[cfg(windows)]
        {
            assert_eq!(parse(r"C:\folder\file.rs:120:7").path, Path::new(r"C:\folder\file.rs"));
            assert_eq!(parse("C:123").line, None);
        }
    }

    #[cfg(unix)]
    #[test]
    fn cli_positions_preserve_non_unicode_unix_names() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let parsed = FileArgument::parse(OsString::from_vec(b"/tmp/\xfffile:9:2".to_vec()), false).unwrap();
        assert_eq!(parsed.path.as_os_str().as_bytes(), b"/tmp/\xfffile");
        assert_eq!((parsed.line, parsed.column), (Some(9), Some(2)));
    }

    #[cfg(windows)]
    #[test]
    fn cli_positions_preserve_unpaired_windows_surrogates() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let units = [b'C' as u16, b':' as u16, b'\\' as u16, 0xd800, b':' as u16, b'9' as u16];
        let parsed = FileArgument::parse(OsString::from_wide(&units), false).unwrap();
        assert_eq!(parsed.path.as_os_str().encode_wide().collect::<Vec<_>>(), units[..4]);
        assert_eq!(parsed.line, Some(9));
    }

    #[test]
    fn cli_positions_resolve_without_file_system_access() {
        let root = if cfg!(windows) { Path::new(r"E:\not-created") } else { Path::new("/not-created") };
        let file = parse("new-file:8:3").resolve(root).unwrap();
        assert_eq!(file.path, root.join("new-file"));
        assert_eq!((file.line, file.column), (Some(8), Some(3)));
        assert!(parse("file").resolve(Path::new("relative")).is_err());
    }
}
