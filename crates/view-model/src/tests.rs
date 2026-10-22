use super::*;
use kwil_core_commands::{
    Key, KeyChord, Mode, Modifiers,
    vim::{Invocation, Vim, VimResult},
};
use std::{io, time::Duration};

fn editor(bytes: &[u8]) -> Editor {
    let mut d = Document::new();
    d.buffer.insert(0, bytes).unwrap();
    d.buffer.set_undo_memory_limit(0);
    d.buffer.set_undo_memory_limit(16 << 20);
    let mut e = Editor::new(d);
    e.set_viewport(0, 8);
    e
}
fn bytes(e: &Editor) -> Vec<u8> {
    let mut b = Vec::new();
    e.document().snapshot().write_to(&mut b).unwrap();
    b
}
fn at(e: &mut Editor, offset: u64) {
    e.set_cursors(vec![Cursor::caret(offset)], 0).unwrap();
}
fn type_text(e: &mut Editor, text: &str, ms: u64) {
    e.insert_text(text, Duration::from_millis(ms)).unwrap();
}
fn vim(e: &mut Editor, parser: &mut Vim, input: &str) -> Vec<CommandResult> {
    let mut results = Vec::new();
    for c in input.chars() {
        match parser.feed(KeyChord::new(Key::Character(c), Modifiers::default())) {
            VimResult::Command(call) => results.push(e.execute_vim(&call).unwrap()),
            VimResult::Pending => {}
            other => panic!("{input} {c}: {other:?}"),
        }
    }
    results
}
fn escape(e: &mut Editor, parser: &mut Vim) {
    let VimResult::Command(call) = parser.feed("escape".parse().unwrap()) else { panic!("escape") };
    e.execute_vim(&call).unwrap();
}

#[test]
fn multicursor_typing_deletion_undo_redo_and_primary() {
    let mut e = editor(b"ab\ncd\nef");
    let original = vec![Cursor::caret(1), Cursor::caret(4), Cursor::caret(7)];
    e.set_cursors(original.clone(), 1).unwrap();
    type_text(&mut e, "X", 0);
    type_text(&mut e, "Y", 100);
    assert_eq!(bytes(&e), b"aXYb\ncXYd\neXYf");
    assert_eq!(e.primary_index(), 1);
    assert_eq!(e.primary_cursor().head, 8);
    assert!(e.document().is_dirty());
    e.undo().unwrap();
    assert_eq!(bytes(&e), b"ab\ncd\nef");
    assert_eq!(e.cursors(), original);
    assert!(!e.document().is_dirty());
    e.redo().unwrap();
    assert_eq!(bytes(&e), b"aXYb\ncXYd\neXYf");
    e.delete(true, false).unwrap();
    assert_eq!(bytes(&e), b"aXb\ncXd\neXf");
    e.undo().unwrap();
    assert_eq!(bytes(&e), b"aXYb\ncXYd\neXYf");
    e.delete(false, false).unwrap();
    assert_eq!(bytes(&e), b"aXY\ncXY\neXY");
}

#[test]
fn typing_timeout_movement_and_history_cap_break_bursts() {
    let mut e = editor(b"");
    type_text(&mut e, "a", 0);
    type_text(&mut e, "b", 600);
    e.undo().unwrap();
    assert_eq!(bytes(&e), b"a");
    type_text(&mut e, "c", 700);
    e.move_cursors(Motion::Left, 1, false).unwrap();
    type_text(&mut e, "!", 710);
    e.undo().unwrap();
    assert_eq!(bytes(&e), b"ac");
    e.set_undo_memory_limit(0);
    assert_eq!(e.undo().unwrap(), Damage::default());
    assert_eq!(bytes(&e), b"ac");
}

#[test]
fn overlapping_selections_and_duplicate_cursors_edit_once() {
    let mut e = editor(b"abcdef");
    e.set_cursors(
        vec![
            Cursor { anchor: 1, head: 4, desired_column: None },
            Cursor { anchor: 3, head: 5, desired_column: None },
            Cursor::caret(5),
            Cursor::caret(5),
        ],
        1,
    )
    .unwrap();
    assert_eq!(e.cursors().len(), 2);
    type_text(&mut e, "X", 0);
    assert_eq!(bytes(&e), b"aXXf");
    e.undo().unwrap();
    assert_eq!(bytes(&e), b"abcdef");
}

#[test]
fn box_selection_replaces_short_lines_and_restores_selections() {
    let mut e = editor(b"abcd\nx\n12345");
    e.column_selection(0, 1, 2, 3).unwrap();
    let cursors = e.cursors().to_vec();
    type_text(&mut e, "Z", 0);
    assert_eq!(bytes(&e), b"aZd\nxZ\n1Z45");
    e.undo().unwrap();
    assert_eq!(bytes(&e), b"abcd\nx\n12345");
    assert_eq!(e.cursors(), cursors);
    e.redo().unwrap();
    assert_eq!(bytes(&e), b"aZd\nxZ\n1Z45");
    let mut e = editor(b"abcd\nx\n12345");
    e.column_selection(0, 3, 2, 4).unwrap();
    type_text(&mut e, "Z", 0);
    assert_eq!(bytes(&e), b"abcZ\nx  Z\n123Z5");
    e.undo().unwrap();
    type_text(&mut e, "!", 1000);
    assert_eq!(bytes(&e), b"abc!\nx  !\n123!5");
}

#[test]
fn newline_autoindent_settings_and_crlf() {
    let mut e = editor(b"\t  thing");
    e.document.line_ending = DocumentLineEnding::CrLf;
    at(&mut e, 8);
    e.newline().unwrap();
    assert_eq!(bytes(&e), b"\t  thing\r\n      ");
    e.undo().unwrap();
    e.settings.editor.insert_spaces = false;
    e.settings.editor.tab_size = 2;
    e.newline().unwrap();
    assert_eq!(bytes(&e), b"\t  thing\r\n\t\t");
    e.undo().unwrap();
    e.settings.editor.auto_indent = false;
    e.newline().unwrap();
    assert_eq!(bytes(&e), b"\t  thing\r\n");
}

#[test]
fn character_word_delete_and_invalid_utf8_are_byte_exact() {
    let mut e = editor("one élève\r\n東京 x".as_bytes());
    at(&mut e, 4);
    e.delete(false, true).unwrap();
    assert_eq!(bytes(&e), "one 東京 x".as_bytes());
    e.undo().unwrap();
    at(&mut e, 11);
    e.delete(true, true).unwrap();
    assert_eq!(bytes(&e), "one \r\n東京 x".as_bytes());
    let mut e = editor(b"a\xffb\r\nc");
    at(&mut e, 2);
    e.delete(true, false).unwrap();
    assert_eq!(bytes(&e), b"ab\r\nc");
    at(&mut e, 2);
    e.delete(false, false).unwrap();
    assert_eq!(bytes(&e), b"abc");
    e.undo().unwrap();
    assert_eq!(bytes(&e), b"ab\r\nc");
}

#[test]
fn line_delete_duplicate_and_move_edges() {
    let mut e = editor(b"aa\nbb\ncc");
    at(&mut e, 3);
    e.move_lines(true).unwrap();
    assert_eq!(bytes(&e), b"aa\ncc\nbb");
    assert_eq!(e.primary_cursor().head, 6);
    e.undo().unwrap();
    assert_eq!(bytes(&e), b"aa\nbb\ncc");
    assert_eq!(e.primary_cursor().head, 3);
    e.move_lines(false).unwrap();
    assert_eq!(bytes(&e), b"bb\naa\ncc");
    assert_eq!(e.primary_cursor().head, 0);
    e.move_lines(false).unwrap();
    assert_eq!(bytes(&e), b"bb\naa\ncc");
    e.duplicate_lines().unwrap();
    assert_eq!(bytes(&e), b"bb\nbb\naa\ncc");
    e.undo().unwrap();
    at(&mut e, 6);
    e.duplicate_lines().unwrap();
    assert_eq!(bytes(&e), b"bb\naa\ncc\ncc");
    e.undo().unwrap();
    e.delete_lines(1).unwrap();
    assert_eq!(bytes(&e), b"bb\naa");
    e.undo().unwrap();
    let mut e = editor(b"a\nb\n");
    at(&mut e, 2);
    e.delete_lines(1).unwrap();
    assert_eq!(bytes(&e), b"a\n");
    let mut e = editor(b"only");
    e.delete_lines(1).unwrap();
    assert_eq!(bytes(&e), b"");
}

#[test]
fn indent_outdent_and_mixed_comment_selection() {
    let mut e = editor(b"  a\n  // b\n\n  c");
    e.set_language(Some("rust".into()));
    e.select_all().unwrap();
    e.toggle_comment().unwrap();
    assert_eq!(bytes(&e), b"  // a\n  // b\n\n  // c");
    e.toggle_comment().unwrap();
    assert_eq!(bytes(&e), b"  a\n  b\n\n  c");
    e.indent(false).unwrap();
    assert_eq!(bytes(&e), b"      a\n      b\n    \n      c");
    e.indent(true).unwrap();
    assert_eq!(bytes(&e), b"  a\n  b\n\n  c");
    e.undo().unwrap();
    assert_eq!(bytes(&e), b"      a\n      b\n    \n      c");
    e.set_line_comment_token(Some(";".into()));
    e.toggle_comment().unwrap();
    assert!(bytes(&e).starts_with(b"      ; a"));
}

#[test]
fn trim_is_streamed_one_undo_and_markdown_is_preserved() {
    let mut e = editor(b"a  \r\nb\t\nlast  ");
    e.trim_trailing_whitespace().unwrap();
    assert_eq!(bytes(&e), b"a\r\nb\nlast");
    e.undo().unwrap();
    assert_eq!(bytes(&e), b"a  \r\nb\t\nlast  ");
    e.set_language(Some("markdown".into()));
    e.trim_trailing_whitespace().unwrap();
    assert_eq!(bytes(&e), b"a  \r\nb\t\nlast  ");
}

#[derive(Default)]
struct MemoryClipboard(Option<ClipboardData>);
impl Clipboard for MemoryClipboard {
    fn set(&mut self, data: ClipboardData) -> io::Result<()> {
        self.0 = Some(data);
        Ok(())
    }
    fn get(&mut self) -> io::Result<Option<ClipboardData>> {
        Ok(self.0.clone())
    }
}

#[test]
fn clipboard_cut_copy_paste_and_word_line_all_selection() {
    let mut e = editor(b"one two\nthree");
    let mut clipboard = MemoryClipboard::default();
    e.select_word().unwrap();
    assert_eq!(e.primary_cursor().selection(), 0..3);
    e.copy(&mut clipboard).unwrap();
    let mut copied = Vec::new();
    clipboard.0.as_ref().unwrap().write_to(&mut copied).unwrap();
    assert_eq!(copied, b"one");
    e.cut(&mut clipboard).unwrap();
    assert_eq!(bytes(&e), b" two\nthree");
    e.paste(&mut clipboard).unwrap();
    assert_eq!(bytes(&e), b"one two\nthree");
    e.select_line().unwrap();
    assert_eq!(e.primary_cursor().selection(), 0..8);
    e.select_all().unwrap();
    assert_eq!(e.primary_cursor().selection(), 0..13);
    clipboard.set(ClipboardData::text(b"ok").unwrap()).unwrap();
    e.paste(&mut clipboard).unwrap();
    assert_eq!(bytes(&e), b"ok");
}

#[test]
fn unicode_motions_home_pages_columns_and_selection() {
    let mut e = editor("  café 東京\nlong row\nx\nlast".as_bytes());
    e.move_cursors(Motion::SmartHome, 1, false).unwrap();
    assert_eq!(e.primary_cursor().head, 2);
    e.move_cursors(Motion::Right, 4, true).unwrap();
    assert_eq!(e.primary_cursor().selection(), 2..7);
    e.move_cursors(Motion::WordForward, 1, false).unwrap();
    assert_eq!(e.primary_cursor().head, 8);
    e.move_cursors(Motion::WordBackward, 1, false).unwrap();
    assert_eq!(e.primary_cursor().head, 2);
    e.go_to_line(2, 6, false).unwrap();
    e.move_cursors(Motion::Down, 1, false).unwrap();
    assert_eq!(e.primary_cursor().head, "  café 東京\nlong row\nx".len() as u64);
    e.move_cursors(Motion::Up, 1, false).unwrap();
    assert_eq!(e.primary_cursor().desired_column, Some(5));
    e.set_viewport(0, 2);
    e.move_cursors(Motion::PageDown, 1, true).unwrap();
    assert_eq!(e.document().snapshot().offset_to_line_col(e.primary_cursor().head).unwrap().unwrap().0, 3);
    e.move_cursors(Motion::DocumentStart, 1, false).unwrap();
    assert_eq!(e.primary_cursor().head, 0);
    e.move_cursors(Motion::DocumentEnd, 1, false).unwrap();
    assert_eq!(e.primary_cursor().head, e.document().buffer().len());
    e.go_to_line(999, 999, false).unwrap();
    assert_eq!(e.primary_cursor().head, e.document().buffer().len());
}

#[test]
fn add_cursor_vertical_and_next_literal_occurrence() {
    let mut e = editor(b"same same\nsame\nsame");
    at(&mut e, 1);
    e.add_cursor_vertical(true).unwrap();
    assert_eq!(e.cursors().len(), 2);
    assert_eq!(e.primary_cursor().head, 11);
    e.add_cursor_vertical(false).unwrap();
    assert_eq!(e.cursors().len(), 2);
    at(&mut e, 0);
    e.select_word().unwrap();
    let result = e.add_next_occurrence(100).unwrap();
    assert_eq!(result.status, CommandStatus::Applied);
    assert_eq!(e.cursors().iter().map(Cursor::selection).collect::<Vec<_>>(), vec![0..4, 5..9]);
    e.add_next_occurrence(1).unwrap();
    assert_eq!(e.cursors().len(), 2);
    type_text(&mut e, "X", 0);
    assert_eq!(bytes(&e), b"X X\nsame\nsame");
}

#[test]
fn bracket_match_is_bounded_and_extends() {
    let mut e = editor(b"(a[b(c)d]e)");
    assert_eq!(
        e.match_bracket(3, false).unwrap().status,
        CommandStatus::Message("matching bracket not found within scan budget".into())
    );
    assert_eq!(e.match_bracket(100, true).unwrap().status, CommandStatus::Applied);
    assert_eq!(e.primary_cursor().selection(), 0..10);
    e.match_bracket(100, false).unwrap();
    assert_eq!(e.primary_cursor().head, 0);
}

#[test]
fn vim_parser_dw_ciw_repeat_3dd_and_put_registers() {
    let mut e = editor(b"one two three");
    let mut parser = Vim::default();
    vim(&mut e, &mut parser, "dw");
    assert_eq!(bytes(&e), b"two three");
    vim(&mut e, &mut parser, ".");
    assert_eq!(bytes(&e), b"three");
    vim(&mut e, &mut parser, "p");
    assert_eq!(bytes(&e), b"ttwo hree");
    let mut e = editor(b"one two");
    let mut parser = Vim::default();
    vim(&mut e, &mut parser, "ciw");
    assert_eq!(e.mode, Mode::Insert);
    type_text(&mut e, "NEW", 100);
    escape(&mut e, &mut parser);
    assert_eq!(bytes(&e), b"NEW two");
    e.undo().unwrap();
    assert_eq!(bytes(&e), b"one two");
    e.redo().unwrap();
    e.go_to_line(1, 5, false).unwrap();
    vim(&mut e, &mut parser, ".");
    assert_eq!(bytes(&e), b"NEW NEW");
    let mut e = editor(b"a\nb\nc\nd\n");
    let mut parser = Vim::default();
    vim(&mut e, &mut parser, "3dd");
    assert_eq!(bytes(&e), b"d\n");
    vim(&mut e, &mut parser, "p");
    assert_eq!(bytes(&e), b"d\na\nb\nc\n");
    e.undo().unwrap();
    assert_eq!(bytes(&e), b"d\n");
    let mut e = editor(b"one two");
    let mut parser = Vim::default();
    vim(&mut e, &mut parser, "\"ayiw");
    e.go_to_line(1, 5, false).unwrap();
    vim(&mut e, &mut parser, "\"ap");
    assert_eq!(bytes(&e), b"one tonewo");
}

#[test]
fn unknown_commands_status_and_command_motion_routing() {
    let mut e = editor(b"a\nb");
    assert!(matches!(
        e.execute("does.not.exist", &CommandArgs::default(), 1).unwrap().status,
        CommandStatus::NotImplemented(_)
    ));
    e.execute("edit.newline", &CommandArgs::default(), 1).unwrap();
    assert_eq!(bytes(&e), b"\na\nb");
    e.execute("navigation.goToLine", &CommandArgs { line: Some(3), ..Default::default() }, 1).unwrap();
    assert_eq!(e.primary_cursor().head, 3);
    let call = Invocation {
        id: "motion.lastLine",
        count: 1,
        explicit_count: true,
        motion: None,
        register: None,
        argument: kwil_core_commands::vim::Argument::None,
    };
    e.execute_vim(&call).unwrap();
    assert_eq!(e.primary_cursor().head, 0);
    assert_eq!(e.execute("file.save", &CommandArgs::default(), 1).unwrap().status, CommandStatus::SaveRequested);
}

#[test]
fn damage_single_row_cursor_old_new_selection_and_scroll() {
    let mut e = editor(b"a\nb\nc\nd\ne\nf");
    e.set_viewport(0, 3);
    let d = e.insert_text("x", Duration::ZERO).unwrap();
    assert_eq!(d.rows, vec![0]);
    assert!(d.status);
    assert!(!d.full);
    assert_eq!(d.scroll, 0);
    let d = e.move_cursors(Motion::Down, 1, false).unwrap();
    assert_eq!(d.rows, vec![0, 1]);
    assert!(!d.full);
    let d = e.move_cursors(Motion::Down, 2, false).unwrap();
    assert_eq!(d.scroll, 1);
    assert!(!d.full);
    assert_eq!(d.rows, vec![0, 2]);
    e.set_viewport(0, 3);
    at(&mut e, 0);
    e.move_cursors(Motion::Down, 2, true).unwrap();
    let d = e.move_cursors(Motion::Right, 1, false).unwrap();
    assert_eq!(d.rows, vec![0, 1, 2], "cleared selection interiors need repaint");
}

#[test]
fn saved_revision_and_concurrent_edits_keep_undo_and_dirty_state() {
    let mut e = editor(b"old");
    type_text(&mut e, "new", 0);
    let base = std::env::temp_dir();
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = base.join(format!(
        "kwil-editor-save-{}-{}.txt",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let request = e.save_request(Some(path.clone())).unwrap();
    let saved = std::thread::spawn(|| kwil_core_workspace::save_snapshot(request)).join().unwrap();
    e.finish_save(saved).unwrap();
    assert!(!e.document().is_dirty());
    e.undo().unwrap();
    assert_eq!(bytes(&e), b"old");
    assert!(e.document().is_dirty());
    let request = e.save_request(None).unwrap();
    type_text(&mut e, "!", 1000);
    let saved = std::thread::spawn(|| kwil_core_workspace::save_snapshot(request)).join().unwrap();
    e.finish_save(saved).unwrap();
    assert!(e.document().is_dirty());
    assert_eq!(std::fs::read(&path).unwrap(), b"old");
    assert_eq!(bytes(&e), b"!old");
    drop(e);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn save_rebases_current_root_and_keeps_pre_save_worker_alive() {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "kwil-editor-rebase-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("f.txt");
    std::fs::write(&path, b"old").unwrap();
    let file = kwil_core_workspace::open_for_editing(&path).unwrap();
    let mut e = Editor::new(Document::from_file(path.clone(), file).unwrap());
    type_text(&mut e, "new ", 0);
    let worker = e.document().snapshot();
    let request = e.save_request(None).unwrap();
    e.finish_save(kwil_core_workspace::save_snapshot(request)).unwrap();
    e.set_undo_memory_limit(0);
    let mut output = Vec::new();
    worker.write_to(&mut output).unwrap();
    assert_eq!(output, b"new old");
    drop(worker);
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        1,
        "rebased current root must release the old backing once history/worker references end"
    );
    assert_eq!(bytes(&e), b"new old");
    drop(e);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn trim_save_request_does_no_reads_and_worker_completion_is_undoable() {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "kwil-worker-trim-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("f.txt");
    std::fs::write(&path, b"a  \r\nb\t\nlast  ").unwrap();
    let file = kwil_core_workspace::open_for_editing(&path).unwrap();
    let mut e = Editor::new(Document::from_file(path.clone(), file).unwrap());
    e.set_viewport(0, 4);
    e.settings.editor.trim_trailing_whitespace_on_save = true;
    let len = e.document().buffer().len();
    at(&mut e, len);
    let moved = dir.join("held.txt");
    std::fs::rename(&path, &moved).unwrap();
    let request = e.save_request(None).expect("request creation must not read/scan/mutate the document");
    assert_eq!(request.snapshot.len(), len);
    assert_eq!(e.document().revision(), 0);
    std::fs::rename(&moved, &path).unwrap();
    let saved = std::thread::spawn(|| kwil_core_workspace::save_snapshot(request)).join().unwrap();
    let result = e.finish_save(saved).unwrap();
    assert_eq!(bytes(&e), b"a\r\nb\nlast");
    assert!(!e.document().is_dirty());
    assert_eq!(result.damage.rows, vec![0, 1, 2]);
    assert!(!result.damage.full);
    assert_eq!(e.primary_cursor().head, b"a\r\nb\nlast".len() as u64);
    e.undo().unwrap();
    assert_eq!(bytes(&e), b"a  \r\nb\t\nlast  ");
    assert!(e.document().is_dirty());
    e.redo().unwrap();
    assert_eq!(bytes(&e), b"a\r\nb\nlast");
    assert!(!e.document().is_dirty());
    drop(e);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn concurrent_trim_save_never_marks_untrimmed_undo_version_clean() {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "kwil-concurrent-trim-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("f.txt");
    std::fs::write(&path, b"a  \n").unwrap();
    let file = kwil_core_workspace::open_for_editing(&path).unwrap();
    let mut e = Editor::new(Document::from_file(path.clone(), file).unwrap());
    e.settings.editor.trim_trailing_whitespace_on_save = true;
    let request = e.save_request(None).unwrap();
    type_text(&mut e, "!", 100);
    e.finish_save(std::thread::spawn(|| kwil_core_workspace::save_snapshot(request)).join().unwrap()).unwrap();
    assert_eq!(bytes(&e), b"!a  \n");
    assert_eq!(std::fs::read(&path).unwrap(), b"a\n");
    assert!(e.document().is_dirty());
    e.undo().unwrap();
    assert_eq!(bytes(&e), b"a  \n");
    assert!(e.document().is_dirty());
    drop(e);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn repeated_typing_at_touching_ranges_keeps_distinct_cursors() {
    let mut e = editor(b"abcdef");
    e.set_cursors(vec![Cursor { anchor: 1, head: 5, desired_column: None }, Cursor::caret(5)], 1).unwrap();
    type_text(&mut e, "X", 0);
    type_text(&mut e, "Y", 100);
    assert_eq!(bytes(&e), b"aXYXYf");
    assert_eq!(e.cursors().len(), 2);
    e.undo().unwrap();
    assert_eq!(bytes(&e), b"abcdef");
}

#[test]
fn multiple_line_moves_preserve_all_cursors_and_redo_positions() {
    let mut e = editor(b"a\nb\nc\nd\ne");
    e.set_cursors(vec![Cursor::caret(0), Cursor::caret(4)], 1).unwrap();
    e.move_lines(true).unwrap();
    assert_eq!(bytes(&e), b"b\na\nd\nc\ne");
    assert_eq!(e.cursors().iter().map(|c| c.head).collect::<Vec<_>>(), vec![2, 6]);
    e.undo().unwrap();
    assert_eq!(bytes(&e), b"a\nb\nc\nd\ne");
    e.redo().unwrap();
    assert_eq!(e.primary_cursor().head, 6);
    assert_eq!(e.cursors().len(), 2);
}

#[test]
fn line_move_selection_end_boundary_owns_selected_line_and_restores_orientation() {
    for reverse in [false, true] {
        let mut e = editor(b"a\nb\nc");
        let original =
            Cursor { anchor: if reverse { 4 } else { 2 }, head: if reverse { 2 } else { 4 }, desired_column: None };
        e.set_cursors(vec![original.clone()], 0).unwrap();
        e.move_lines(false).unwrap();
        assert_eq!(bytes(&e), b"b\na\nc");
        let moved = Cursor { anchor: original.anchor - 2, head: original.head - 2, desired_column: None };
        assert_eq!(e.cursors(), std::slice::from_ref(&moved));
        e.undo().unwrap();
        assert_eq!(e.cursors(), &[original]);
        e.redo().unwrap();
        assert_eq!(e.cursors(), &[moved]);
        e.move_lines(true).unwrap();
        assert_eq!(bytes(&e), b"a\nb\nc");
        assert_eq!(e.primary_cursor().selection(), 2..4);
    }
    let mut e = editor(b"a\nb\nc");
    at(&mut e, 4);
    e.move_lines(false).unwrap();
    assert_eq!(bytes(&e), b"a\nc\nb");
    assert_eq!(e.primary_cursor().head, 2);
}

#[test]
fn vim_insert_session_and_repeat_do_not_merge_unrelated_changes() {
    let mut e = editor(b"ab");
    let mut parser = Vim::default();
    vim(&mut e, &mut parser, "i");
    type_text(&mut e, "X", 0);
    type_text(&mut e, "Y", 2000);
    escape(&mut e, &mut parser);
    assert_eq!(bytes(&e), b"XYab");
    e.undo().unwrap();
    assert_eq!(bytes(&e), b"ab");
    e.redo().unwrap();
    at(&mut e, 0);
    vim(&mut e, &mut parser, ".");
    assert_eq!(bytes(&e), b"XYXYab");
    e.undo().unwrap();
    assert_eq!(bytes(&e), b"XYab");
}

#[test]
fn frontend_positions_and_selection_ranges_use_unicode_and_tab_stops() {
    let mut e = editor("\té\nnext".as_bytes());
    assert_eq!(e.position_at(3).unwrap(), (0, 5));
    assert_eq!(e.offset_at(0, 5).unwrap(), 3);
    assert_eq!(e.byte_position(3).unwrap(), (0, 3));
    assert_eq!(e.line_range(0).unwrap(), 0..3);
    e.select_all().unwrap();
    assert_eq!(e.selection_ranges(0).unwrap(), std::iter::once(0..3).collect::<Vec<_>>());
    assert_eq!(e.selection_ranges(1).unwrap(), std::iter::once(4..8).collect::<Vec<_>>());
}

#[test]
fn linewise_put_at_unterminated_eof_and_precise_undo_damage() {
    let mut e = editor(b"last");
    let mut parser = Vim::default();
    vim(&mut e, &mut parser, "yyp");
    assert_eq!(bytes(&e), b"last\nlast\n");
    let mut e = editor(b"a\nb\nc");
    type_text(&mut e, "X", 0);
    assert_eq!(e.undo().unwrap().rows, vec![0]);
    assert_eq!(e.redo().unwrap().rows, vec![0]);
    at(&mut e, 2);
    assert_eq!(e.indent(false).unwrap().rows, vec![0]);
}

#[test]
fn view_defers_goto_and_caches_bounded_long_line_windows() {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "kwil-view-window-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut content = vec![b'a'; 128 << 10];
    content.extend_from_slice(b"\nlast");
    std::fs::write(&path, content).unwrap();
    let file = kwil_core_text::TextFile::open(&path).unwrap();
    let mut v = View::new(Some((path.clone(), file.clone())));
    v.set_rows(2);
    assert_eq!(v.line(0).unwrap().len(), 16 << 10);
    assert_eq!(
        v.execute("navigation.goToLine", &CommandArgs { line: Some(2), column: Some(3), ..Default::default() }, 1)
            .unwrap()
            .status,
        CommandStatus::IndexNotReady
    );
    file.build_index(1).unwrap();
    v.index_ready();
    assert_eq!(v.editor().unwrap().position_at(v.editor().unwrap().primary_cursor().head).unwrap(), (1, 2));
    v.set_horizontal_window(10, 32);
    assert_eq!(v.line(0).unwrap().len(), 32);
    drop((v, file));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn whole_document_line_move_and_clipboard_reuse_keep_original_pieces_owned() {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "kwil-owned-recipes-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("f.txt");
    let mut content = vec![b'a'; 32768];
    content.push(b'\n');
    content.extend(std::iter::repeat_n(b'b', 32767));
    std::fs::write(&path, &content).unwrap();
    let file = kwil_core_workspace::open_for_editing(&path).unwrap();
    let mut e = Editor::new(Document::from_file(path.clone(), file).unwrap());
    e.move_lines(true).unwrap();
    assert_eq!(e.document().buffer().add_memory_bytes(), 0);
    assert_eq!(e.document().snapshot().chunks(0..1).unwrap().next().unwrap().unwrap(), b"b");
    e.undo().unwrap();
    let mut clipboard = MemoryClipboard::default();
    e.select_all().unwrap();
    e.cut(&mut clipboard).unwrap();
    e.paste(&mut clipboard).unwrap();
    assert_eq!(e.document().buffer().add_memory_bytes(), 0);
    assert_eq!(bytes(&e), content);
    let request = e.save_request(None).unwrap();
    e.finish_save(kwil_core_workspace::save_snapshot(request)).unwrap();
    e.select_all().unwrap();
    e.delete(false, false).unwrap();
    e.paste(&mut clipboard).unwrap();
    assert_eq!(e.document().buffer().add_memory_bytes(), 0);
    assert_eq!(bytes(&e), content);
    drop((e, clipboard));
    std::fs::remove_dir_all(dir).unwrap();
}

fn observe(e: &mut Editor) -> std::sync::Arc<std::sync::Mutex<Vec<EditReceipt>>> {
    let receipts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = receipts.clone();
    e.set_edit_observer(Some(Box::new(move |receipt| {
        sink.lock().unwrap().push(receipt);
        Ok(())
    })))
    .unwrap();
    receipts
}
fn replay_receipts(initial: &[u8], receipts: Vec<EditReceipt>) -> Vec<u8> {
    // Exercise the public worker API, using a deliberately simple independent
    // byte-vector oracle only for these small test documents.
    let initial = initial.to_vec();
    std::thread::spawn(move || {
        let mut bytes = initial;
        let mut previous_revision = receipts.first().map_or(0, |r| r.before_revision);
        for (index, receipt) in receipts.iter().enumerate() {
            assert_eq!(receipt.sequence, index as u64 + 1);
            assert_eq!(receipt.before_revision, previous_revision);
            let mut before = Vec::new();
            receipt.before.write_to(&mut before).unwrap();
            assert_eq!(bytes, before);
            let mut count = 0;
            receipt
                .for_each_replacement(&mut |replacement| {
                    let mut insert = Vec::new();
                    replacement.insert.write_to(&mut insert)?;
                    assert_eq!(insert.len() as u64, replacement.insert.len());
                    assert_eq!(replacement.insert.is_empty(), insert.is_empty());
                    bytes.splice(replacement.range.start as usize..replacement.range.end as usize, insert);
                    count += 1;
                    Ok(())
                })
                .unwrap();
            assert_eq!(count, receipt.replacement_count());
            let mut after = Vec::new();
            receipt.after.write_to(&mut after).unwrap();
            assert_eq!(bytes, after, "receipt {} {:?}", receipt.sequence, receipt.kind);
            previous_revision = receipt.after_revision;
        }
        bytes
    })
    .join()
    .unwrap()
}

#[test]
fn receipts_multicursor_coalesced_undo_redo_are_exact() {
    let initial = b"abcdef\nmore words\nend";
    let mut e = editor(initial);
    let receipts = observe(&mut e);
    e.set_cursors(vec![Cursor { anchor: 1, head: 4, desired_column: None }, Cursor::caret(8), Cursor::caret(20)], 1)
        .unwrap();
    type_text(&mut e, "XY", 0);
    type_text(&mut e, "Z", 20);
    e.undo().unwrap();
    assert_eq!(bytes(&e), initial);
    e.redo().unwrap();
    e.delete(true, true).unwrap();
    e.undo().unwrap();
    let receipts = std::mem::take(&mut *receipts.lock().unwrap());
    assert_eq!(receipts.len(), 6);
    assert_eq!(receipts[2].kind, EditKind::Undo);
    assert_eq!(receipts[2].replacement_count(), 3);
    assert_eq!(replay_receipts(initial, receipts), bytes(&e));
}

#[test]
fn receipts_bulk_lines_and_vim_operations_replay_atomically() {
    let initial = b"one  \n  two\t\nthree\n";
    let mut e = editor(initial);
    e.set_language(Some("rust".into()));
    let receipts = observe(&mut e);
    e.select_all().unwrap();
    e.indent(false).unwrap();
    e.indent(true).unwrap();
    e.toggle_comment().unwrap();
    e.undo().unwrap();
    e.redo().unwrap();
    e.trim_trailing_whitespace().unwrap();
    at(&mut e, 0);
    let mut parser = Vim::default();
    vim(&mut e, &mut parser, "dwciw");
    type_text(&mut e, "ok", 50);
    escape(&mut e, &mut parser);
    e.undo().unwrap();
    e.redo().unwrap();
    e.move_lines(true).unwrap();
    e.duplicate_lines().unwrap();
    let receipts = std::mem::take(&mut *receipts.lock().unwrap());
    assert!(receipts.iter().any(|r| r.replacement_count() >= 3));
    assert_eq!(replay_receipts(initial, receipts), bytes(&e));
}

#[test]
fn receipts_backpressure_retains_save_completion_and_stale_save_settles() {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "kwil-receipt-save-{}-{}.txt",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut e = editor(b"a  \nb\t\nlast ");
    e.settings.editor.trim_trailing_whitespace_on_save = true;
    let accepting = Arc::new(AtomicBool::new(false));
    let accepted = Arc::new(Mutex::new(Vec::new()));
    let gate = accepting.clone();
    let sink = accepted.clone();
    e.set_edit_observer(Some(Box::new(move |receipt| {
        if gate.load(Ordering::Relaxed) {
            sink.lock().unwrap().push(receipt);
            Ok(())
        } else {
            Err(receipt)
        }
    })))
    .unwrap();
    type_text(&mut e, "X", 0);
    assert!(e.has_pending_edit_receipt());
    assert_eq!(e.next_edit_sequence(), 2);
    assert_eq!(e.undo().unwrap_err().kind(), io::ErrorKind::WouldBlock);
    assert_eq!(e.indent(false).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    assert_eq!(e.insert_text("blocked", Duration::ZERO).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    assert!(e.set_edit_observer(None).is_err());
    let request = e.save_request(Some(path.clone())).unwrap();
    let saved = kwil_core_workspace::save_snapshot(request);
    assert_eq!(e.finish_save(saved).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    assert!(e.has_pending_save_completion());
    accepting.store(true, Ordering::Relaxed);
    e.flush_edit_receipt().unwrap();
    assert_eq!(e.delete(false, false).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    e.finish_pending_save().unwrap().unwrap();
    assert!(!e.has_pending_save_completion());
    assert!(!e.document().is_dirty());
    e.undo().unwrap();
    e.redo().unwrap();
    // Concurrent ordinary/stale-trim completions never publish phantom edits.
    let request = e.save_request(None).unwrap();
    type_text(&mut e, "  ", 1000);
    let count = accepted.lock().unwrap().len();
    e.finish_save(kwil_core_workspace::save_snapshot(request)).unwrap();
    assert_eq!(accepted.lock().unwrap().len(), count);
    let end = e.line_range(0).unwrap().end;
    at(&mut e, end);
    type_text(&mut e, "  ", 1500);
    let request = e.save_request(None).unwrap();
    accepting.store(false, Ordering::Relaxed);
    type_text(&mut e, "!", 2000);
    assert!(e.has_pending_edit_receipt());
    let count = accepted.lock().unwrap().len();
    let saved = kwil_core_workspace::save_snapshot(request).unwrap();
    assert!(saved.trimmed_rows.is_some(), "exercise a transformed stale result");
    e.finish_save(Ok(saved)).unwrap();
    assert_eq!(accepted.lock().unwrap().len(), count);
    assert!(e.has_pending_edit_receipt());
    assert!(!e.has_pending_save_completion());
    accepting.store(true, Ordering::Relaxed);
    e.flush_edit_receipt().unwrap();
    let receipts = std::mem::take(&mut *accepted.lock().unwrap());
    assert_eq!(receipts[1].kind, EditKind::WorkerTrim);
    assert_eq!(receipts[1].replacement_count(), 3);
    assert_eq!(replay_receipts(b"a  \nb\t\nlast ", receipts), bytes(&e));
    drop(e);
    std::fs::remove_file(path).unwrap();
}

#[test]
#[ignore = "one-shot model measurements on the generated 1 GB fixture"]
fn measure_model_typing_1gb_one_and_100_cursors() {
    let path = std::env::var_os("KWIL_MODEL_FIXTURE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/log-1g.log"));
    let file = kwil_core_text::TextFile::open(&path).unwrap();
    assert!(file.len() >= 1 << 30);
    file.build_index(4).unwrap();
    let make = |count: u64| {
        let mut e = Editor::new(Document::from_file(path.clone(), file.clone()).unwrap());
        let s = e.document().snapshot();
        let first = 10_000_000;
        let cursors = (0..count).map(|n| Cursor::caret(s.line_start(first + n).unwrap().unwrap() + 5)).collect();
        e.set_viewport(first, 60);
        e.set_cursors(cursors, 0).unwrap();
        e
    };
    let mut one = make(1);
    let mut hundred = make(100);
    let len = file.len();
    let start = std::time::Instant::now();
    let d1 = one.insert_text("x", Duration::ZERO).unwrap();
    let one_ms = start.elapsed().as_secs_f64() * 1000.0;
    let start = std::time::Instant::now();
    let d100 = hundred.insert_text("x", Duration::ZERO).unwrap();
    let hundred_ms = start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "fixture_bytes={len} model_typing_one_cursor_ms={one_ms:.6} model_typing_100_cursors_ms={hundred_ms:.6} add_bytes_one={} add_bytes_100={} damaged_rows_one={} damaged_rows_100={}",
        one.document().buffer().add_memory_bytes(),
        hundred.document().buffer().add_memory_bytes(),
        d1.rows.len(),
        d100.rows.len()
    );
    assert_eq!(one.document().buffer().len(), len + 1);
    assert_eq!(hundred.document().buffer().len(), len + 100);
    assert!(!d1.full && !d100.full);
    assert_eq!(one.document().buffer().add_memory_bytes(), 1);
    assert_eq!(hundred.document().buffer().add_memory_bytes(), 100);
    assert!(one_ms < 8.3 && hundred_ms < 8.3, "model operation missed 120 Hz frame budget");
}
