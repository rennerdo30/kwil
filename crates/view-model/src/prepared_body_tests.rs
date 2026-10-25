//! Source-only controls for View's legacy cache boundary. These deliberately
//! allow Editor's semantic reads; they do not claim a no-IO editing engine.
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture {
    dir: PathBuf,
    path: PathBuf,
    held: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "kwil-prepared-body-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("source.txt");
        std::fs::write(&path, b"zero\none\ntwo\nthree\nfour\nfive\nsix\nseven").unwrap();
        Self { held: dir.join("held.txt"), dir, path }
    }

    fn view(&self, indexed: bool) -> View {
        let file = TextFile::open(&self.path).unwrap();
        if indexed {
            file.build_index(1).unwrap();
        }
        let mut view = View::new(Some((self.path.clone(), file)));
        view.set_rows(2);
        view
    }

    fn hide(&self) {
        std::fs::rename(&self.path, &self.held).unwrap();
    }

    fn restore(&self) {
        std::fs::rename(&self.held, &self.path).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Only this test's two exact files and empty directory are touched.
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(&self.held);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

fn no_legacy_body(view: &View) {
    assert_eq!(view.body_policy(), ViewBodyPolicy::Prepared);
    assert!(view.lines.is_empty(), "prepared input must not rebuild a hidden logical cache");
    assert!(view.line_offsets.is_empty());
    assert!(view.line_sources.is_empty());
    assert_eq!(view.line(0), None);
    assert_eq!(view.line_source(0), None);
    assert_eq!(view.line_offset(0), 0);
}

fn bytes(view: &View) -> Vec<u8> {
    let mut bytes = Vec::new();
    view.editor().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
    bytes
}

#[test]
fn prepared_body_resize_scroll_and_refresh_never_read_unavailable_original() {
    let fixture = Fixture::new();
    let mut view = fixture.view(true);
    assert_eq!(view.body_policy(), ViewBodyPolicy::LogicalLines);
    view.scroll(2);
    assert_eq!(view.line(0), Some(&b"two"[..]));
    let source = view.editor().unwrap().document().hex_source(17);
    let token = view.input_incarnation.clone();
    assert!(view.set_body_policy(ViewBodyPolicy::Prepared).full);
    no_legacy_body(&view);
    assert_eq!(view.set_body_policy(ViewBodyPolicy::Prepared), Damage::default());

    // Different logical coordinates catch accidental top synchronization and
    // set_viewport, even though those operations do not necessarily read bytes.
    view.editor_mut().unwrap().set_viewport(5, 1);
    fixture.hide();
    assert!(view.file().unwrap().bytes().is_err(), "negative control: source reads really fail");
    view.set_notice("untouched".into());
    view.set_rows(99); // Legacy clamping would move top 2 to zero.
    assert_eq!(view.top(), 2);
    assert_eq!(view.rows(), 99);
    assert_eq!((view.editor().unwrap().top(), view.editor().unwrap().rows()), (5, 1));
    view.set_horizontal_window(1, 3);
    let damage = Damage { full: true, scroll: 1, rows: vec![0], status: true };
    assert_eq!(view.refresh_after_cursor(damage.clone()), damage);
    assert_eq!(view.refresh_after_edit(damage.clone()), damage);
    view.index_ready();
    view.reload();
    for delta in [-7, 0, 7] {
        assert_eq!(view.scroll(delta), Damage::default());
    }
    assert_eq!(view.scroll_to_start(), Damage::default());
    assert_eq!(view.scroll_to_end(), Damage::default());
    assert_eq!(view.top(), 2);
    assert_eq!(view.notice(), "untouched", "a swallowed reload error must fail this control");
    assert_eq!(view.editor().unwrap().document().hex_source(17), source);
    assert!(std::sync::Arc::ptr_eq(&view.input_incarnation, &token));
    no_legacy_body(&view);

    fixture.restore();
    view.set_rows(2);
    view.set_horizontal_window(0, 16);
    assert!(view.set_body_policy(ViewBodyPolicy::LogicalLines).full);
    assert_eq!(view.top(), 5);
    assert_eq!(view.editor().unwrap().rows(), 2);
    assert_eq!(view.line(0), Some(&b"five"[..]));
    assert_eq!(view.line_source(0), Some(24..28));
    assert_eq!(view.editor().unwrap().document().hex_source(17), source);
}

#[test]
fn prepared_body_execute_preserves_edit_undo_and_reloads_current_bytes_on_release() {
    let mut view = View::new(None);
    view.set_rows(2);
    let epoch = view.editor().unwrap().document().hex_source(1).epoch;
    view.set_body_policy(ViewBodyPolicy::Prepared);
    let args = CommandArgs { text: Some("alpha\nbeta\ngamma".into()), ..Default::default() };
    assert_eq!(view.execute("edit.insert", &args, 1).unwrap().status, CommandStatus::Applied);
    assert_eq!(bytes(&view), b"alpha\nbeta\ngamma");
    assert!(view.editor().unwrap().document().is_dirty());
    let revision = view.editor().unwrap().document().revision();
    assert!(revision > 0);
    assert_eq!(view.top(), 0, "semantic editor top must not become prepared body coordinates");
    no_legacy_body(&view);
    view.execute("edit.undo", &CommandArgs::default(), 1).unwrap();
    assert_eq!(bytes(&view), b"");
    assert!(!view.editor().unwrap().document().is_dirty());
    no_legacy_body(&view);
    view.execute("edit.redo", &CommandArgs::default(), 1).unwrap();
    assert_eq!(view.editor().unwrap().document().revision(), revision);
    assert_eq!(bytes(&view), b"alpha\nbeta\ngamma");
    no_legacy_body(&view);

    view.set_body_policy(ViewBodyPolicy::LogicalLines);
    assert_eq!(view.top(), 1);
    assert_eq!(view.line(0), Some(&b"beta"[..]));
    assert_eq!(view.line(1), Some(&b"gamma"[..]));
    assert_eq!(view.line_source(0), Some(6..10));
    assert_eq!(view.editor().unwrap().document().hex_source(1).epoch, epoch);
    assert_eq!(view.editor().unwrap().document().revision(), revision);
    view.execute("edit.undo", &CommandArgs::default(), 1).unwrap();
    assert_eq!(view.line(0), Some(&b""[..]), "ordinary cache behavior resumes");
}

#[test]
fn prepared_body_execute_layout_command_never_reads_original_for_legacy_cache() {
    let fixture = Fixture::new();
    let mut view = fixture.view(true);
    view.set_body_policy(ViewBodyPolicy::Prepared);
    fixture.hide();
    view.set_notice("untouched".into());
    // This command has full body damage but no semantic source requirement.
    let result = view.execute("view.toggleWhitespace", &CommandArgs::default(), 1).unwrap();
    assert_eq!(result.status, CommandStatus::Applied);
    assert!(result.damage.full);
    assert_eq!(view.notice(), "untouched");
    no_legacy_body(&view);
    fixture.restore();
    view.set_body_policy(ViewBodyPolicy::LogicalLines);
    assert_eq!(view.line(0), Some(&b"zero"[..]));
}

#[test]
fn prepared_body_keeps_metadata_live_while_cache_and_logical_top_are_suspended() {
    let fixture = Fixture::new();
    let mut view = fixture.view(true);
    let original_path = view.file().unwrap().path().to_path_buf();
    let changed_path = fixture.dir.join("saved-as.txt");
    view.set_body_policy(ViewBodyPolicy::Prepared);
    let document = &mut view.editor_mut().unwrap().document;
    document.path = Some(changed_path.clone());
    document.bom = true;
    document.line_ending = DocumentLineEnding::CrLf;
    let source = document.hex_source(19);
    let compression = document.compression;
    let encoding = document.encoding;
    view.refresh_after_edit(Damage::default());
    assert_eq!(view.path.as_ref(), Some(&changed_path));
    assert_eq!(view.file().unwrap().path(), original_path);
    assert!(view.status_text().starts_with(&changed_path.display().to_string()));
    no_legacy_body(&view);
    view.set_body_policy(ViewBodyPolicy::LogicalLines);
    let document = view.editor().unwrap().document();
    assert_eq!(document.hex_source(19), source);
    assert_eq!(document.path(), Some(changed_path.as_path()));
    assert_eq!(document.encoding, encoding);
    assert_eq!(document.compression, compression);
    assert!(document.bom);
    assert_eq!(document.line_ending, DocumentLineEnding::CrLf);
    assert_eq!(view.file().unwrap().path(), original_path);
    assert_eq!(view.line(0), Some(&b"zero"[..]));
}

#[test]
fn prepared_body_unindexed_viewer_never_falls_back_to_first_lines() {
    let fixture = Fixture::new();
    let mut view = fixture.view(false);
    assert!(view.editor().is_none());
    view.set_body_policy(ViewBodyPolicy::Prepared);
    fixture.hide();
    view.set_notice("untouched".into());
    view.set_rows(3);
    view.set_horizontal_window(1, 2);
    view.index_ready();
    view.refresh_after_edit(Damage { full: true, ..Default::default() });
    assert_eq!(view.notice(), "untouched");
    no_legacy_body(&view);
    fixture.restore();
    view.set_horizontal_window(0, 16);
    view.set_body_policy(ViewBodyPolicy::LogicalLines);
    assert!(view.editor().is_none());
    assert_eq!(view.line(0), Some(&b"zero"[..]));
}

#[test]
fn prepared_body_index_install_does_not_set_logical_viewport_or_load_cache() {
    let fixture = Fixture::new();
    let mut view = fixture.view(false);
    view.set_body_policy(ViewBodyPolicy::Prepared);
    view.top = 4;
    view.set_rows(3);
    view.file().unwrap().build_index(1).unwrap();
    view.index_ready();
    assert_eq!(view.top(), 4);
    assert_eq!(view.editor().unwrap().top(), 0);
    assert_eq!(view.editor().unwrap().rows(), 0);
    no_legacy_body(&view);
    view.set_body_policy(ViewBodyPolicy::LogicalLines);
    assert_eq!(view.top(), 0);
    assert_eq!(view.editor().unwrap().rows(), 3);
    assert_eq!(view.line(0), Some(&b"zero"[..]));
}

#[test]
fn prepared_body_pending_goto_retains_semantic_navigation_without_logical_cache() {
    let fixture = Fixture::new();
    let mut view = fixture.view(false);
    view.set_body_policy(ViewBodyPolicy::Prepared);
    let result = view.execute("navigation.goToLine", &CommandArgs { line: Some(7), ..Default::default() }, 1).unwrap();
    assert_eq!(result.status, CommandStatus::IndexNotReady);
    view.file().unwrap().build_index(1).unwrap();
    view.index_ready();
    assert!(view.pending_goto.is_none());
    let editor = view.editor().unwrap();
    assert_eq!(editor.position_at(editor.primary_cursor().head).unwrap(), (6, 0));
    assert_eq!(view.top(), 0);
    assert_eq!(editor.rows(), 0);
    no_legacy_body(&view);
}

#[test]
fn prepared_body_accessors_never_expose_stale_raw_rows() {
    let mut view = View::new(None);
    view.set_body_policy(ViewBodyPolicy::Prepared);
    // Even an accidentally retained legacy cache is not a permitted fallback
    // while the prepared page is pending, stale, failed or has zero rows.
    view.lines.push(b"stale raw body".to_vec());
    view.line_sources.push(11..25);
    view.line_offsets.push(11);
    assert_eq!(view.line(0), None);
    assert_eq!(view.line_source(0), None);
    assert_eq!(view.line_offset(0), 0);
    view.set_body_policy(ViewBodyPolicy::LogicalLines);
    assert!(view.lines.is_empty(), "zero-row switchback must discard even stale caches");
}

#[test]
fn prepared_body_preserves_preview_pending_goto_and_loan_state() {
    let fixture = Fixture::new();
    let original = fixture.view(true);
    let mut view =
        View::from_preview(fixture.path.clone(), original.file.clone().unwrap(), b"decoded preview\nsecond".to_vec());
    view.set_rows(2);
    assert_eq!(view.line(0), Some(&b"decoded preview"[..]));
    view.pending_goto = Some((3, 1, false));
    view.set_body_policy(ViewBodyPolicy::Prepared);
    view.set_rows(3);
    view.index_ready();
    assert!(view.editor().is_none(), "preview ownership must still prevent raw editor attachment");
    assert_eq!(view.pending_goto, Some((3, 1, false)));
    assert_eq!(view.preview.as_deref(), Some(&b"decoded preview\nsecond"[..]));
    no_legacy_body(&view);
    view.set_body_policy(ViewBodyPolicy::LogicalLines);
    assert_eq!(view.line(0), Some(&b"decoded preview"[..]));
    assert!(view.line_source(0).is_none());

    // Existing loan sentinel is preserved; the policy does not seize a body or
    // install an editor. Workspace owns actual loan/input/modal admission.
    view.preview = Some(Vec::new());
    view.set_body_policy(ViewBodyPolicy::Prepared);
    view.index_ready();
    assert!(view.editor().is_none());
    assert_eq!(view.preview, Some(Vec::new()));
    assert_eq!(view.pending_goto, Some((3, 1, false)));
    no_legacy_body(&view);
}

#[test]
fn prepared_body_raw_document_policy_does_not_change_representation_or_edit_bytes() {
    let fixture = Fixture::new();
    let mut view = fixture.view(true);
    let file = view.file.clone().unwrap();
    let document = Document::from_raw_file(fixture.path.clone(), file, 71).unwrap();
    view.editor = Some(Editor::new(document));
    view.set_body_policy(ViewBodyPolicy::Prepared);
    view.refresh_after_edit(Damage { full: true, ..Default::default() });
    let source = view.editor().unwrap().document().hex_source(23);
    assert_eq!(source.representation, hex_session::Representation::RawBytes);
    no_legacy_body(&view);
    view.set_body_policy(ViewBodyPolicy::LogicalLines);
    assert_eq!(view.editor().unwrap().document().hex_source(23), source);
    assert_eq!(bytes(&view), b"zero\none\ntwo\nthree\nfour\nfive\nsix\nseven");
}
