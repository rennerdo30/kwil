//! Worker-only encoded save: logical text and immutable raw provenance stay
//! separate from the actual destination file. No file-sized payload or edit list.
use super::*;
use kwil_core_text::decoded_backing::{BackingError, DecodedBacking, Interpretation};
use kwil_core_text::encoding::{
    Encoding, EncodingError, SavePlan, validate_snapshot_save_cancellable, write_snapshot_cancellable,
};
use std::io::{BufWriter, Write};
use std::sync::atomic::AtomicBool;

/// Completion must be retired on a worker, including stale/cancelled results.
/// Retain `backing` until all snapshots/undo/clipboard references have gone,
/// then call `try_retire` on a worker. Its Drop deliberately does not unlink.
/// `saved.file` watches the actual destination; `saved.snapshot` is a complete
/// logical decoded original, never raw UTF-16 or serialized FF sentinels.
#[must_use = "retain the decoded backing owner for explicit worker retirement"]
pub struct EncodedSavedSnapshot {
    pub saved: SavedSnapshot,
    pub backing: DecodedBacking,
    /// Exact logical byte comparison, not just a length comparison. If true,
    /// install the worker edit rather than checkpoint_current. Trim spool
    /// coordinates refer to the SUBMITTED logical text, before either trim or
    /// ending conversion. Ending conversion can shift offsets on every line;
    /// trim deletions alone cannot map cursors/journal edits to the saved text.
    pub logical_coordinates_changed: bool,
}

/// Validate before creating the same-directory save temp. Trim scans and encoded
/// writes are bounded, with original-coordinate deletions spooled on disk.
/// Prepare and index the private decoded checkpoint from the synced encoded
/// temp BEFORE replacement. Any such failure leaves the destination intact.
/// Once replacement succeeds, cancellation no longer turns it into a failed
/// save. A subsequent raw destination reopen failure explicitly says committed.
/// Core encoding preflight and streaming cooperatively poll this same flag;
/// preflight cancellation returns before creating the save temp.
pub fn save_encoded_snapshot(
    request: SaveRequest,
    plan: SavePlan,
    private_parent: &Path,
    cancel: &AtomicBool,
) -> io::Result<EncodedSavedSnapshot> {
    save_encoded_with_operations(request, plan, private_parent, cancel, replace)
}

fn encoding_error(error: EncodingError) -> io::Error {
    match error {
        EncodingError::Cancelled => io::Error::new(io::ErrorKind::Interrupted, error),
        EncodingError::Io(error) if error.get_ref().is_some_and(|cause| cause.is::<WriteCancelled>()) => {
            io::Error::new(io::ErrorKind::Interrupted, error)
        }
        EncodingError::Io(error) => error,
        other => io::Error::new(io::ErrorKind::InvalidData, other),
    }
}

pub(super) fn validate_encoded_snapshot(
    snapshot: &Snapshot,
    plan: SavePlan,
    cancelled: impl FnMut() -> bool,
) -> io::Result<()> {
    validate_snapshot_save_cancellable(snapshot, plan, cancelled).map_err(encoding_error)
}

#[derive(Debug)]
struct WriteCancelled;
impl std::fmt::Display for WriteCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("encoded save cancelled before replacement")
    }
}
impl std::error::Error for WriteCancelled {}
fn cancelled(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(io::Error::new(io::ErrorKind::Interrupted, "encoded save cancelled before replacement"))
    } else {
        Ok(())
    }
}

struct Cancellable<'a, W> {
    inner: W,
    cancel: &'a AtomicBool,
}
impl<W: Write> Write for Cancellable<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // write_all retries Interrupted forever. Transport cancellation as a
        // distinct non-retryable error, and translate it only at the API edge.
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other(WriteCancelled));
        }
        self.inner.write(&bytes[..bytes.len().min(64 << 10)])
    }
    fn flush(&mut self) -> io::Result<()> {
        cancelled(self.cancel)?;
        self.inner.flush()
    }
}

pub(super) fn bom_len(plan: SavePlan) -> usize {
    if !plan.bom {
        0
    } else if plan.encoding == Encoding::UTF8 {
        3
    } else {
        2
    }
}

pub(super) fn same_logical(snapshot: &Snapshot, bytes: &[u8], cancel: &AtomicBool) -> io::Result<bool> {
    if snapshot.len() != bytes.len() as u64 {
        return Ok(false);
    }
    let mut at = 0;
    for chunk in snapshot.chunks(0..snapshot.len())? {
        for block in chunk.map_err(io::Error::other)?.chunks(64 << 10) {
            cancelled(cancel)?;
            if block != &bytes[at..at + block.len()] {
                return Ok(false);
            }
            at += block.len();
        }
    }
    Ok(true)
}

pub(super) fn same_after_trim(
    snapshot: &Snapshot,
    spool: &TrimSpool,
    bytes: &[u8],
    cancel: &AtomicBool,
) -> io::Result<bool> {
    let mut at = 0usize;
    let mut compare = |range| -> io::Result<bool> {
        for chunk in snapshot.chunks(range)? {
            for block in chunk.map_err(io::Error::other)?.chunks(64 << 10) {
                cancelled(cancel)?;
                let end = at.checked_add(block.len()).ok_or_else(|| io::Error::other("logical comparison overflow"))?;
                if bytes.get(at..end) != Some(block) {
                    return Ok(false);
                }
                at = end;
            }
        }
        Ok(true)
    };
    let mut source = 0;
    for deletion in spool.ranges()? {
        let deletion = deletion?;
        if !compare(source..deletion.start)? {
            return Ok(false);
        }
        source = deletion.end;
    }
    if !compare(source..snapshot.len())? {
        return Ok(false);
    }
    Ok(at == bytes.len())
}

/// Worker-owned preparation can fail before a logical snapshot exists.
struct PreparedCheckpoint {
    // Retain the owner even before the fallible logical index has produced a
    // snapshot. Owner Drop alone deliberately does not unlink private files.
    snapshot: Option<Snapshot>,
    backing: DecodedBacking,
    logical_coordinates_changed: bool,
    normalized: bool,
}

/// Failure cleanup occurs on the saving worker, never in an owner Drop.
/// Surface cleanup paths rather than silently losing a failed retirement.
fn discard_prepared(prepared: Option<PreparedCheckpoint>, cause: io::Error) -> io::Error {
    let Some(PreparedCheckpoint { snapshot, backing, .. }) = prepared else {
        return cause;
    };
    drop(snapshot);
    match backing.try_retire() {
        Ok(()) => cause,
        Err(retirement) => retirement.with_cause(cause),
    }
}

fn save_encoded_with_operations(
    request: SaveRequest,
    plan: SavePlan,
    private_parent: &Path,
    cancel: &AtomicBool,
    replace: impl FnOnce(&Path, &Path, Option<&Path>) -> io::Result<()>,
) -> io::Result<EncodedSavedSnapshot> {
    save_encoded_with_index_cancellation(request, plan, private_parent, cancel, replace, || {
        cancel.load(Ordering::Relaxed)
    })
}

fn save_encoded_with_index_cancellation(
    request: SaveRequest,
    plan: SavePlan,
    private_parent: &Path,
    cancel: &AtomicBool,
    replace: impl FnOnce(&Path, &Path, Option<&Path>) -> io::Result<()>,
    mut index_cancel: impl FnMut() -> bool,
) -> io::Result<EncodedSavedSnapshot> {
    cancelled(cancel)?;
    validate_encoded_snapshot(&request.snapshot, plan, || cancel.load(Ordering::Relaxed))?;
    cancelled(cancel)?;
    let mut spool = if request.trim_trailing_whitespace { Some(TrimSpool::new()?) } else { None };
    let trimmed_rows = if let Some(spool) = &mut spool {
        visit_trimmed(&request.snapshot, spool, || cancelled(cancel), |_| Ok(()))?
    } else {
        None
    };
    // Validate the exact retained slices and ending boundaries too, before
    // examining the destination or creating its save temp.
    if let Some(spool) = spool.as_ref().filter(|spool| !spool.is_empty()) {
        stream(&request.snapshot, Some(spool), plan, &mut io::sink(), cancel)?;
    }
    let submitted = request.snapshot.clone();
    let mut prepared = None;
    let committed = commit_with_operations(
        request,
        |snapshot, file| {
            let mut writer = BufWriter::with_capacity(64 << 10, file);
            stream(snapshot, spool.as_ref(), plan, &mut writer, cancel)?;
            writer.flush()?;
            Ok(trimmed_rows)
        },
        |temp| {
            cancelled(cancel)?;
            let raw = TextFile::open_saved_version(temp).map_err(io::Error::from)?;
            let backing = DecodedBacking::prepare(
                &raw,
                Interpretation::new(plan.encoding, bom_len(plan)).map_err(io::Error::other)?,
                private_parent,
                cancel,
            )
            .map_err(BackingError::into_io_error)?;
            let origin = backing.snapshot();
            // Store the owner before any subsequent fallible work, so all
            // aborted precommit paths can explicitly retire it on this worker.
            prepared = Some(PreparedCheckpoint {
                snapshot: None,
                backing,
                logical_coordinates_changed: false,
                normalized: false,
            });
            let snapshot = Buffer::from_decoded_cancellable(origin.clone(), &mut index_cancel)?.snapshot();
            prepared.as_mut().unwrap().snapshot = Some(snapshot);
            let changed = !same_logical(&submitted, origin.logical_bytes(), cancel)?;
            prepared.as_mut().unwrap().logical_coordinates_changed = changed;
            let normalized = if let Some(spool) = spool.as_ref().filter(|spool| !spool.is_empty()) {
                !same_after_trim(&submitted, spool, origin.logical_bytes(), cancel)?
            } else {
                changed
            };
            prepared.as_mut().unwrap().normalized = normalized;
            cancelled(cancel)
        },
        replace,
        sync_parent,
    );
    let committed = match committed {
        Ok(committed) => committed,
        Err(error) => return Err(discard_prepared(prepared, error)),
    };
    let file = match TextFile::open_saved_version(&committed.path) {
        Ok(file) => file,
        Err(error) => {
            return Err(discard_prepared(
                prepared,
                committed_error(&committed.path, "destination reopen", io::Error::from(error)),
            ));
        }
    };
    let PreparedCheckpoint { snapshot, backing, logical_coordinates_changed, normalized } = prepared.unwrap();
    let mut saved = committed.finish(file, snapshot.expect("successful preparation must finish logical indexing"));
    saved.trim_deletions = spool.filter(|spool| !spool.is_empty());
    saved.normalized = normalized;
    Ok(EncodedSavedSnapshot { saved, backing, logical_coordinates_changed })
}

/// Per-range stateless encoding preserves provenance without constructing an
/// edited piece tree proportional to the number of trimmed lines. A trailing
/// CR is held across range boundaries when converting endings: deleting spaces
/// can join CR and LF, which must be converted as one ending, not two.
pub(super) fn stream(
    snapshot: &Snapshot,
    spool: Option<&TrimSpool>,
    plan: SavePlan,
    writer: &mut impl Write,
    cancel: &AtomicBool,
) -> io::Result<()> {
    let mut writer = Cancellable { inner: writer, cancel };
    let Some(spool) = spool.filter(|spool| !spool.is_empty()) else {
        return write_snapshot_cancellable(snapshot, plan, &mut writer, || cancel.load(Ordering::Relaxed))
            .map_err(encoding_error);
    };
    // Exactly one output BOM, even when every logical byte is trimmed.
    write_snapshot_cancellable(&Buffer::empty().snapshot(), plan, &mut writer, || cancel.load(Ordering::Relaxed))
        .map_err(encoding_error)?;
    let body = SavePlan { bom: false, ..plan };
    let mut pending_cr: Option<Snapshot> = None;
    let mut retain = |mut range: Range<u64>| -> io::Result<()> {
        cancelled(cancel)?;
        if range.is_empty() {
            return Ok(());
        }
        if let Some(cr) = pending_cr.take() {
            let first = snapshot.chunks(range.start..range.start + 1)?.next().unwrap().map_err(io::Error::other)?[0];
            if first == b'\n' {
                let pair = cr.concat(&snapshot.slice(range.start..range.start + 1)?)?;
                write_snapshot_cancellable(&pair, body, &mut writer, || cancel.load(Ordering::Relaxed))
                    .map_err(encoding_error)?;
                range.start += 1;
            } else {
                write_snapshot_cancellable(&cr, body, &mut writer, || cancel.load(Ordering::Relaxed))
                    .map_err(encoding_error)?;
            }
        }
        if range.is_empty() {
            return Ok(());
        }
        if body.line_ending.is_some() {
            let last = snapshot.chunks(range.end - 1..range.end)?.next().unwrap().map_err(io::Error::other)?[0];
            if last == b'\r' {
                pending_cr = Some(snapshot.slice(range.end - 1..range.end)?);
                range.end -= 1;
            }
        }
        if !range.is_empty() {
            write_snapshot_cancellable(&snapshot.slice(range)?, body, &mut writer, || cancel.load(Ordering::Relaxed))
                .map_err(encoding_error)?;
        }
        Ok(())
    };
    let mut from = 0;
    for range in spool.ranges()? {
        let range = range?;
        retain(from..range.start)?;
        from = range.end;
    }
    retain(from..snapshot.len())?;
    if let Some(cr) = pending_cr {
        write_snapshot_cancellable(&cr, body, &mut writer, || cancel.load(Ordering::Relaxed))
            .map_err(encoding_error)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kwil_core_text::decoded_backing::RetirementError;
    use kwil_core_text::encoding::LineEnding;
    use std::cell::{Cell, RefCell};

    fn block_checkpoint(root: &Path) -> PathBuf {
        let directory = fs::read_dir(root)
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| entry.file_type().unwrap().is_dir())
            .unwrap()
            .path();
        fs::write(directory.join("blocker"), b"test blocker").unwrap();
        directory
    }

    #[test]
    fn pending_retirement_encoded_precommit_cleanup_survives_atomic_context() {
        let f = Fixture::new();
        fs::write(f.path(), b"keep destination").unwrap();
        let mut buffer = Buffer::empty();
        buffer.insert(0, b"new").unwrap();
        let error = save_encoded_with_index_cancellation(
            f.request(&buffer, false),
            plan(Encoding::UTF8, false),
            &f.root,
            &AtomicBool::new(false),
            |_, _, _| panic!("cancelled index must not replace destination"),
            || {
                block_checkpoint(&f.root);
                true
            },
        )
        .err()
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(error.to_string().contains("cannot save"));
        assert_eq!(fs::read(f.path()).unwrap(), b"keep destination");
        let Some(RetirementError::Cleanup { paths, .. }) = RetirementError::take_from(&error) else {
            panic!("atomic save context must preserve typed cleanup");
        };
        fs::remove_file(paths.directory.join("blocker")).unwrap();
        paths.cleanup().unwrap();
        assert_eq!(f.private_directories(), 0);
        assert_eq!(fs::read_dir(&f.root).unwrap().count(), 1);
    }

    #[test]
    fn pending_retirement_encoded_postcommit_cleanup_keeps_committed_context() {
        let f = Fixture::new();
        let mut buffer = Buffer::empty();
        buffer.insert(0, b"saved bytes").unwrap();
        let retained = f.root.join("committed-copy");
        let error = save_encoded_with_operations(
            f.request(&buffer, false),
            plan(Encoding::UTF8, false),
            &f.root,
            &AtomicBool::new(false),
            |temp, destination, backup| {
                replace(temp, destination, backup)?;
                fs::rename(destination, &retained)?;
                block_checkpoint(&f.root);
                Ok(())
            },
        )
        .err()
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("file committed at"));
        assert!(error.to_string().contains("destination reopen"));
        let Some(RetirementError::Cleanup { paths, .. }) = RetirementError::take_from(&error) else {
            panic!("postcommit cleanup must retain its resource");
        };
        // Extracting the resource must not erase committed diagnostics.
        assert!(error.to_string().contains("do not retry as an uncommitted save"));
        assert_eq!(fs::read(retained).unwrap(), b"saved bytes");
        fs::remove_file(paths.directory.join("blocker")).unwrap();
        paths.cleanup().unwrap();
    }

    #[test]
    fn pending_retirement_encoded_discard_preserves_live_owner() {
        let f = Fixture::new();
        let owner = DecodedBacking::prepare_from_reader(
            &mut &b"pinned"[..],
            6,
            Interpretation::new(Encoding::UTF8, 0).unwrap(),
            &f.root,
            &AtomicBool::new(false),
        )
        .unwrap();
        let pin = owner.snapshot();
        let directory = pin.paths().directory.clone();
        let prepared = PreparedCheckpoint {
            snapshot: Some(Buffer::from_decoded(owner.snapshot()).snapshot()),
            backing: owner,
            logical_coordinates_changed: false,
            normalized: false,
        };
        let error = discard_prepared(Some(prepared), io::Error::new(io::ErrorKind::Interrupted, "failed save"));
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        let Some(RetirementError::LiveSnapshots(owner)) = RetirementError::take_from(&error) else {
            panic!("encoded discard must preserve the actual pinned owner");
        };
        assert_eq!(pin.raw_bytes(), b"pinned");
        assert!(directory.exists());
        drop(pin);
        assert!(owner.try_retire().is_ok());
        assert!(!directory.exists());
    }

    struct Fixture {
        root: PathBuf,
        next: Cell<u64>,
        owners: RefCell<Vec<DecodedBacking>>,
    }
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "kwil-encoded-save-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self { root, next: Cell::new(0), owners: RefCell::new(Vec::new()) }
        }
        fn path(&self) -> PathBuf {
            self.root.join("destination")
        }
        fn decoded(&self, bytes: &[u8], encoding: Encoding, bom: usize) -> Buffer {
            let n = self.next.get();
            self.next.set(n + 1);
            let path = self.root.join(format!("source-{n}"));
            fs::write(&path, bytes).unwrap();
            let raw = TextFile::open(&path).unwrap();
            let backing = DecodedBacking::prepare(
                &raw,
                Interpretation::new(encoding, bom).unwrap(),
                &self.root,
                &AtomicBool::new(false),
            )
            .unwrap();
            let buffer = Buffer::from_decoded(backing.snapshot());
            self.owners.borrow_mut().push(backing);
            buffer
        }
        fn request(&self, buffer: &Buffer, trim: bool) -> SaveRequest {
            SaveRequest {
                compression: crate::compressed::CompressionFormat::Raw,
                snapshot: buffer.snapshot(),
                path: self.path(),
                original: None,
                revision: 17,
                trim_trailing_whitespace: trim,
            }
        }
        fn save(&self, request: SaveRequest, plan: SavePlan) -> (SavedSnapshot, bool) {
            let result = save_encoded_snapshot(request, plan, &self.root, &AtomicBool::new(false)).unwrap();
            self.owners.borrow_mut().push(result.backing);
            (result.saved, result.logical_coordinates_changed)
        }
        fn private_directories(&self) -> usize {
            fs::read_dir(&self.root).unwrap().filter(|e| e.as_ref().unwrap().file_type().unwrap().is_dir()).count()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            for owner in self.owners.get_mut().drain(..) {
                assert!(owner.try_retire().is_ok(), "test retained a logical saved snapshot");
            }
            fs::remove_dir_all(&self.root).unwrap();
        }
    }
    fn plan(encoding: Encoding, bom: bool) -> SavePlan {
        SavePlan { encoding, bom, line_ending: None }
    }
    fn logical(snapshot: &Snapshot) -> Vec<u8> {
        let mut bytes = Vec::new();
        snapshot.write_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn encoded_utf16_checkpoint_is_logical_and_repeated_save_preserves_undo() {
        let f = Fixture::new();
        fs::write(f.path(), b"old destination").unwrap();
        let mut b = f.decoded(b"\xff\xfeA\0\0\xd8B\0", Encoding::UTF16_LE, 2);
        let old = b.snapshot();
        b.insert(1, "é".as_bytes()).unwrap();
        let (saved, changed) = f.save(f.request(&b, false), plan(Encoding::UTF16_LE, true));
        assert!(!changed);
        assert_eq!(saved.revision, 17);
        assert_eq!(saved.file.path(), f.path());
        assert_eq!(saved.file.len(), 10);
        assert_eq!(logical(&saved.snapshot), b"A\xc3\xa9\xffB");
        assert_eq!(fs::read(f.path()).unwrap(), b"\xff\xfeA\0\xe9\0\0\xd8B\0");
        b.checkpoint_current(saved.snapshot).unwrap();
        assert!(b.undo());
        assert_eq!(logical(&b.snapshot()), logical(&old));
        let mut request = f.request(&b, false);
        request.original = Some(saved.file);
        let (saved, changed) = f.save(request, plan(Encoding::UTF16_LE, true));
        assert!(!changed);
        assert_eq!(fs::read(f.path()).unwrap(), b"\xff\xfeA\0\0\xd8B\0");
        b.checkpoint_current(saved.snapshot).unwrap();
        assert!(b.redo());
        assert_eq!(logical(&b.snapshot()), b"A\xc3\xa9\xffB");
    }

    #[test]
    fn encoded_trim_preserves_malformed_aliases_multiple_origins_and_spool_coordinates() {
        let f = Fixture::new();
        let codec = Encoding::for_label("big5").unwrap();
        let a = f.decoded(b"\x88\x62\xff \t\r\n", codec, 0);
        let b = f.decoded(b"Z  \rQ\t", codec, 0);
        let mut joined = Buffer::empty();
        let snapshot = a.snapshot().concat(&b.snapshot()).unwrap();
        // The submitted immutable tree keeps both complete decoded origins.
        let request = SaveRequest { snapshot, ..f.request(&joined, true) };
        let (saved, changed) = f.save(request, plan(codec, false));
        assert!(changed);
        assert_eq!(fs::read(f.path()).unwrap(), b"\x88\x62\xff\r\nZ\rQ");
        assert_eq!(
            logical(&saved.snapshot),
            "Ê\u{304}".as_bytes().iter().copied().chain([0xff, b'\r', b'\n', b'Z', b'\r', b'Q']).collect::<Vec<_>>()
        );
        assert_eq!(saved.trimmed_rows, Some(0..2));
        let ranges = saved.trim_deletions.as_ref().unwrap().ranges().unwrap().collect::<io::Result<Vec<_>>>().unwrap();
        assert_eq!(ranges, [5..7, 10..12, 14..15]);
        joined.install_worker_edit(saved.snapshot).unwrap();
        assert_eq!(joined.len(), 10);
    }

    #[test]
    fn encoded_trim_line_endings_cross_retained_ranges_and_bom_is_once() {
        let f = Fixture::new();
        let mut b = Buffer::empty();
        b.insert(0, b"A\r \nB \t\r\nC  \rD \nE \t").unwrap();
        let (saved, changed) =
            f.save(f.request(&b, true), plan(Encoding::UTF16_BE, true).change_line_endings(LineEnding::Lf));
        assert!(changed);
        assert_eq!(logical(&saved.snapshot), b"A\nB\nC\nD\nE");
        assert_eq!(fs::read(f.path()).unwrap(), b"\xfe\xff\0A\0\n\0B\0\n\0C\0\n\0D\0\n\0E");
        // Conversion can change equal-length content too; this flag cannot
        // be inferred just from trim_deletions or logical byte lengths.
        let mut equal = Buffer::empty();
        equal.insert(0, b"a\rb").unwrap();
        let (saved, changed) =
            f.save(f.request(&equal, false), plan(Encoding::UTF8, false).change_line_endings(LineEnding::Lf));
        assert!(changed);
        assert_eq!(logical(&saved.snapshot), b"a\nb");
        assert!(saved.trim_deletions.is_none());
        let mut spaces = Buffer::empty();
        spaces.insert(0, b" \t ").unwrap();
        let (saved, changed) = f.save(f.request(&spaces, true), plan(Encoding::UTF8, true));
        assert!(changed);
        assert_eq!(fs::read(f.path()).unwrap(), b"\xef\xbb\xbf");
        assert!(saved.snapshot.is_empty());
        spaces.install_worker_edit(saved.snapshot).unwrap();
    }

    #[test]
    fn encoded_preflight_errors_precede_destination_checks_and_leave_bytes_untouched() {
        let f = Fixture::new();
        let mut b = Buffer::empty();
        b.insert(0, "hello 😀".as_bytes()).unwrap();
        fs::write(f.path(), b"unchanged").unwrap();
        let cp = Encoding::for_label("windows-1252").unwrap();
        let err = save_encoded_snapshot(f.request(&b, true), plan(cp, false), &f.root, &AtomicBool::new(false))
            .err()
            .unwrap();
        assert!(err.to_string().contains("cannot be saved as"), "{err}");
        assert_eq!(fs::read(f.path()).unwrap(), b"unchanged");
        assert_eq!(fs::read_dir(&f.root).unwrap().count(), 1);
        let mut request = f.request(&b, false);
        request.path = f.root.join("nonexistent-parent").join("save");
        let err = save_encoded_snapshot(request, plan(cp, false), &f.root, &AtomicBool::new(false)).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "encoding preflight precedes destination creation");
        let malformed = f.decoded(b"A\xff", Encoding::UTF8, 0);
        let err = save_encoded_snapshot(
            f.request(&malformed, false),
            plan(Encoding::UTF16_LE, true),
            &f.root,
            &AtomicBool::new(false),
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("malformed"), "{err}");
        assert_eq!(fs::read(f.path()).unwrap(), b"unchanged");
    }

    #[test]
    fn encoded_preparation_and_replace_failures_retire_private_checkpoint_before_commit() {
        let f = Fixture::new();
        fs::write(f.path(), b"unchanged").unwrap();
        let mut b = Buffer::empty();
        b.insert(0, b"new text").unwrap();
        let err = save_encoded_snapshot(
            f.request(&b, false),
            plan(Encoding::UTF16_LE, true),
            &f.root.join("missing"),
            &AtomicBool::new(false),
        )
        .err()
        .unwrap();
        assert!(!err.to_string().contains("committed"));
        assert_eq!(fs::read(f.path()).unwrap(), b"unchanged");
        assert_eq!(fs::read_dir(&f.root).unwrap().count(), 1);
        let err = save_encoded_with_operations(
            f.request(&b, true),
            plan(Encoding::UTF16_LE, true),
            &f.root,
            &AtomicBool::new(false),
            |_, destination, _| {
                assert_eq!(f.private_directories(), 1, "prepare decoded checkpoint before replacement");
                assert_eq!(fs::read(destination).unwrap(), b"unchanged");
                Err(io::Error::other("injected encoded replace failure"))
            },
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("injected encoded replace failure"), "{err}");
        assert_eq!(fs::read(f.path()).unwrap(), b"unchanged");
        assert_eq!(fs::read_dir(&f.root).unwrap().count(), 1, "no temporary/private backing leak");
    }

    #[test]
    fn encoded_checkpoint_index_cancellation_retires_backing_before_any_replacement() {
        let f = Fixture::new();
        fs::write(f.path(), b"unchanged original").unwrap();
        let mut bytes = vec![b'x'; 128 << 10];
        bytes[(64 << 10) - 1] = b'\n';
        bytes[(128 << 10) - 1] = b'\n';
        let mut b = Buffer::empty();
        b.insert(0, &bytes).unwrap();
        let cancel = AtomicBool::new(false);
        let replaced = Cell::new(false);
        let mut polls = 0;
        let error = save_encoded_with_index_cancellation(
            f.request(&b, false),
            plan(Encoding::UTF8, false),
            &f.root,
            &cancel,
            |temp, destination, backup| {
                replaced.set(true);
                replace(temp, destination, backup)
            },
            || {
                // This probe is passed only to logical indexing, after backing
                // preparation, never to encoding or the raw/logical file copy.
                assert_eq!(f.private_directories(), 1, "backing must exist before index cancellation");
                let directory = fs::read_dir(&f.root)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .find(|path| path.is_dir())
                    .unwrap();
                for name in ["raw", "logical", "checkpoints"] {
                    assert!(directory.join(name).is_file(), "prepared backing lacks {name}");
                }
                assert_eq!(fs::metadata(directory.join("logical")).unwrap().len(), 128 << 10);
                assert_eq!(fs::read(f.path()).unwrap(), b"unchanged original");
                assert!(!replaced.get());
                polls += 1;
                // Current bounded builder checks entry, before the first block,
                // then after its <=64KiB scan. Cancel inside that index build.
                if polls == 3 {
                    cancel.store(true, Ordering::Relaxed);
                }
                cancel.load(Ordering::Relaxed)
            },
        )
        .err()
        .expect("cooperative index cancellation must abort the precommit save");
        assert_eq!(polls, 3, "cancelled logical indexing must stop polling immediately");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted, "{error}");
        assert!(!replaced.get(), "index cancellation must never enter the replacement operation");
        assert_eq!(fs::read(f.path()).unwrap(), b"unchanged original");
        assert_eq!(f.private_directories(), 0, "index-error backing needs explicit worker retirement");
        assert_eq!(fs::read_dir(&f.root).unwrap().count(), 1, "retire all private files and the save temp");
    }

    #[test]
    fn encoded_cancel_before_commit_and_late_cancel_after_commit_are_distinct() {
        let f = Fixture::new();
        fs::write(f.path(), b"old").unwrap();
        let mut b = Buffer::empty();
        b.insert(0, b"new").unwrap();
        let cancel = AtomicBool::new(true);
        let err =
            save_encoded_snapshot(f.request(&b, false), plan(Encoding::UTF8, false), &f.root, &cancel).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert_eq!(fs::read_dir(&f.root).unwrap().count(), 1);
        cancel.store(false, Ordering::Relaxed);
        let saved = save_encoded_with_operations(
            f.request(&b, false),
            plan(Encoding::UTF8, false),
            &f.root,
            &cancel,
            |temp, destination, backup| {
                replace(temp, destination, backup)?;
                cancel.store(true, Ordering::Relaxed);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(fs::read(f.path()).unwrap(), b"new");
        b.checkpoint_current(saved.saved.snapshot).unwrap();
        f.owners.borrow_mut().push(saved.backing);
    }

    #[test]
    fn encoded_preflight_cooperative_cancellation_retains_interrupted_kind() {
        let mut b = Buffer::empty();
        b.insert(0, b"worker preflight source").unwrap();
        let mut polls = 0;
        let error = validate_encoded_snapshot(&b.snapshot(), plan(Encoding::UTF16_LE, true), || {
            polls += 1;
            // Deterministically cancel after preflight actually started, not
            // at the public entry-point's already-cancelled short circuit.
            polls == 2
        })
        .unwrap_err();
        assert_eq!(polls, 2, "preflight must stop on the first cooperative cancellation");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted, "{error}");
        assert!(error.to_string().contains("cancelled"), "{error}");
    }

    #[test]
    fn encoded_owner_requires_worker_retirement_and_destination_reopen_failure_says_committed() {
        let f = Fixture::new();
        let mut b = Buffer::empty();
        b.insert(0, b"new").unwrap();
        let result =
            save_encoded_snapshot(f.request(&b, false), plan(Encoding::UTF8, false), &f.root, &AtomicBool::new(false))
                .unwrap();
        let directory = result.backing.snapshot().paths().directory.clone();
        let owner = match result.backing.try_retire() {
            Err(RetirementError::LiveSnapshots(owner)) => owner,
            _ => panic!("saved snapshot must retain private decoded storage"),
        };
        drop(result.saved);
        assert!(directory.exists());
        assert!(owner.try_retire().is_ok());
        assert!(!directory.exists());
        let retained = f.root.join("committed-copy");
        let error = save_encoded_with_operations(
            f.request(&b, false),
            plan(Encoding::UTF8, false),
            &f.root,
            &AtomicBool::new(false),
            |temp, destination, backup| {
                replace(temp, destination, backup)?;
                fs::rename(destination, &retained)
            },
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("committed"), "{error}");
        assert_eq!(fs::read(&retained).unwrap(), b"new");
        assert_eq!(f.private_directories(), 0);
    }

    #[test]
    fn encoded_stream_is_bounded_and_reports_write_backpressure_and_cancellation() {
        let f = Fixture::new();
        // Under 1 MiB; one long trim run plus many deletions, never a resident
        // edit list. Test the streaming seam rather than measuring a benchmark.
        let mut b = Buffer::with_add_memory_limit(64 << 10);
        b.insert(0, &b"word \t\r\n".repeat(4096)).unwrap();
        b.insert(b.len(), &vec![b' '; 128 << 10]).unwrap();
        let snapshot = b.snapshot();
        let mut spool = TrimSpool::new().unwrap();
        visit_trimmed(&snapshot, &mut spool, || Ok(()), |_| Ok(())).unwrap();
        assert_eq!(spool.len(), 4097);
        struct Slow<'a> {
            cancel: &'a AtomicBool,
            written: usize,
            fail: bool,
        }
        impl Write for Slow<'_> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                assert!(bytes.len() <= 64 << 10);
                if self.fail && self.written > 4 {
                    return Err(io::Error::other("injected backpressure failure"));
                }
                let n = bytes.len().min(3);
                self.written += n;
                if !self.fail && self.written > 16 {
                    self.cancel.store(true, Ordering::Relaxed);
                }
                Ok(n)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let cancel = AtomicBool::new(false);
        let mut writer = Slow { cancel: &cancel, written: 0, fail: true };
        let err = stream(&snapshot, Some(&spool), plan(Encoding::UTF16_LE, true), &mut writer, &cancel).unwrap_err();
        assert!(err.to_string().contains("injected backpressure failure"));
        let mut writer = Slow { cancel: &cancel, written: 0, fail: false };
        let err = stream(&snapshot, Some(&spool), plan(Encoding::UTF8, false), &mut writer, &cancel).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert!(writer.written <= 19);
        assert!(!f.path().exists());
    }
}
