use super::*;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

struct Fake {
    directory: PathBuf,
    script: PathBuf,
}
impl Fake {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "kwil-format-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let script = directory.join(if cfg!(windows) { "fake.ps1" } else { "fake.sh" });
        #[cfg(windows)]
        let source = r#"param([string]$mode, [int]$first=0, [int]$last=0)
$text = [Console]::In.ReadToEnd()
if ($mode -eq 'fail') { exit 7 }
if ($mode -eq 'wait') { Start-Sleep -Seconds 4 }
if ($mode -eq 'range') {
  $line = 0
  foreach ($match in [regex]::Matches($text, '[^\n]*\n|[^\n]+$')) {
    $line++
    $value = $match.Value
    if ($line -ge $first -and $line -le $last) { $value = $value.Replace('bad', 'good') }
    [Console]::Out.Write($value)
  }
} else { [Console]::Out.Write($text.Replace('bad', 'good')) }
"#;
        #[cfg(not(windows))]
        let source = r#"case "$1" in
fail) exit 7 ;;
wait) sleep 4 ;;
range) sed "${2},${3}s/bad/good/g"; exit ;;
esac
sed 's/bad/good/g'
"#;
        std::fs::write(&script, source).unwrap();
        Self { directory, script }
    }
    fn config(&self, mode: &str) -> Formatter {
        #[cfg(windows)]
        let (executable, mut args) = (
            "powershell.exe",
            vec!["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        );
        #[cfg(not(windows))]
        let (executable, mut args) = ("/bin/sh", Vec::new());
        args.extend([self.script.to_string_lossy().into_owned(), mode.into()]);
        Formatter {
            executable: executable.into(),
            args,
            range_args: vec!["{start_line}".into(), "{end_line}".into()],
            // Generous: several PowerShell starts in parallel on a busy runner.
            timeout_ms: 60_000,
        }
    }
}
impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.script);
        let _ = std::fs::remove_dir(&self.directory);
    }
}
fn editor(text: &str) -> Editor {
    let mut editor = Editor::new(Document::new());
    editor.insert_text(text, Duration::ZERO).unwrap();
    editor
}
fn bytes(editor: &Editor) -> Vec<u8> {
    let mut output = Vec::new();
    editor.document().snapshot().write_to(&mut output).unwrap();
    output
}

#[test]
fn formatting_external_is_one_undoable_edit() {
    let fake = Fake::new();
    let mut editor = editor("bad bad\n");
    editor.settings.formatters.insert("custom".into(), fake.config("all"));
    editor.apply_formatting(false, Some("custom".into())).unwrap();
    assert_eq!(bytes(&editor), b"good good\n");
    editor.execute("edit.undo", &CommandArgs::default(), 1).unwrap();
    assert_eq!(bytes(&editor), b"bad bad\n");
    editor.execute("edit.redo", &CommandArgs::default(), 1).unwrap();
    assert_eq!(bytes(&editor), b"good good\n");
}

#[test]
fn formatting_external_handles_unicode_at_eof_without_a_newline() {
    let fake = Fake::new();
    let mut editor = editor("bad😀");
    editor.settings.formatters.insert("custom".into(), fake.config("all"));
    editor.apply_formatting(false, Some("custom".into())).unwrap();
    assert_eq!(String::from_utf8(bytes(&editor)).unwrap().trim_end(), "good😀");
    editor.execute("edit.undo", &CommandArgs::default(), 1).unwrap();
    assert_eq!(bytes(&editor), "bad😀".as_bytes());
}

#[test]
fn formatting_selection_expands_to_lines_and_preserves_crlf_and_unicode() {
    let fake = Fake::new();
    let input = "bad 😀\r\nbad selected\r\nbad outside\r\n";
    let mut editor = editor(input);
    let start = input.find("selected").unwrap() as u64;
    editor.set_cursors(vec![Cursor { anchor: start, head: start + 2, desired_column: None }], 0).unwrap();
    editor.settings.formatters.insert("custom".into(), fake.config("range"));
    editor.apply_formatting(true, Some("custom".into())).unwrap();
    assert_eq!(bytes(&editor), "bad 😀\r\ngood selected\r\nbad outside\r\n".as_bytes());
    editor.execute("edit.undo", &CommandArgs::default(), 1).unwrap();
    assert_eq!(bytes(&editor), input.as_bytes());
}

#[test]
fn formatting_refuses_outside_edits_missing_tools_failures_and_timeouts() {
    let fake = Fake::new();
    for mode in ["all", "fail", "wait", "missing", "no-ranges"] {
        let mut editor = editor("bad first\nbad selected\nbad last\n");
        let before = bytes(&editor);
        editor.set_cursors(vec![Cursor { anchor: 12, head: 13, desired_column: None }], 0).unwrap();
        let mut config = fake.config(mode);
        match mode {
            "wait" => config.timeout_ms = 30,
            "missing" => {
                config.executable = fake.directory.join("missing-formatter.exe").to_string_lossy().into_owned()
            }
            "no-ranges" => config.range_args.clear(),
            _ => {}
        }
        editor.settings.formatters.insert("custom".into(), config);
        let message = editor.apply_formatting(true, Some("custom".into())).unwrap_err().to_string();
        assert!(
            message.contains(match mode {
                "all" => "outside the selection",
                "fail" => "exited with",
                "wait" => "timed out",
                "missing" => "formatters.custom.executable",
                _ => "formatters.custom.range_args",
            }),
            "{mode}: {message}"
        );
        assert_eq!(bytes(&editor), before);
    }
}

#[test]
fn formatting_defaults_and_missing_configuration_are_clear() {
    assert_eq!(default_formatter("rust").unwrap().executable, "rustfmt");
    assert!(default_formatter("rust").unwrap().range_args.is_empty());
    assert_eq!(default_formatter("python").unwrap().executable, "black");
    assert_eq!(default_formatter("javascript").unwrap().executable, "prettier");
    let mut unknown = editor("plain text\n");
    assert!(
        unknown
            .apply_formatting(false, Some("unknown".into()))
            .unwrap_err()
            .to_string()
            .contains("formatters.unknown.executable")
    );
    let mut json = editor("{\"a\":1}\n");
    json.apply_formatting(false, Some("json".into())).unwrap();
    let formatted = bytes(&json);
    assert!(String::from_utf8(formatted.clone()).unwrap().contains("    \"a\": 1"));
    let revision = json.document().revision();
    json.apply_formatting(false, Some("json".into())).unwrap();
    assert_eq!(bytes(&json), formatted);
    assert_eq!(json.document().revision(), revision, "unchanged output must not create an edit");
    assert!(json.apply_formatting(true, Some("json".into())).unwrap_err().to_string().contains("Select the lines"));
}
