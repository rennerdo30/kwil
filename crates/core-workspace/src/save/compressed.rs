//! Atomic gzip saves of encoded logical snapshots. All work runs on a worker.
use super::encoded::{bom_len, same_after_trim, same_logical, stream, validate_encoded_snapshot};
use super::*;
use crate::compressed::{PreparedFile, prepare_gzip_payload, recompress_payload};
use kwil_core_text::decoded_backing::{BackingError, DecodedBacking, Interpretation, RetirementError};
use kwil_core_text::encoding::SavePlan;
use std::sync::{Mutex, atomic::AtomicBool};

/// Keep both private owners in the worker registry. The watched/journal path is
/// `saved.path` and `saved.file`, never either private backing pathname.
#[must_use = "retain both checkpoint owners for explicit worker retirement"]
pub struct CompressedSavedSnapshot {
    pub saved: SavedSnapshot,
    pub owners: CompressedCheckpointOwners,
    pub logical_coordinates_changed: bool,
}

#[must_use = "retire both private owners on a worker after releasing snapshots"]
pub struct CompressedCheckpointOwners {
    pub body: PreparedFile,
    pub decoded: DecodedBacking,
}
impl CompressedCheckpointOwners {
    #[allow(clippy::result_large_err)] // Return both owners intact without allocating on retirement.
    pub fn try_retire(self) -> Result<(), CompressedSaveRetirement> {
        CompressedSaveRetirement { body: Some(self.body), decoded: Some(RetirementError::LiveSnapshots(self.decoded)) }
            .retry()
    }
}

/// A single registry token retains EVERY resource that failed retirement.
/// Neither this token nor its error transport performs I/O in Drop.
#[must_use = "retain the complete retirement token for worker retry"]
pub struct CompressedSaveRetirement {
    pub body: Option<PreparedFile>,
    pub decoded: Option<RetirementError>,
}
impl CompressedSaveRetirement {
    #[allow(clippy::result_large_err)] // Failure transfers every still-pinned resource intact.
    pub fn retry(mut self) -> Result<(), Self> {
        self.body = self.body.take().and_then(|body| body.try_finish().err());
        self.decoded = self.decoded.take().and_then(|decoded| match decoded {
            RetirementError::LiveSnapshots(owner) => owner.try_retire().err(),
            RetirementError::Cleanup { paths, .. } => {
                paths.cleanup().err().map(|error| RetirementError::Cleanup { paths, error })
            }
        });
        if self.body.is_none() && self.decoded.is_none() { Ok(()) } else { Err(self) }
    }

    /// Extract once through atomic-save/committed contexts, without parsing text.
    pub fn take_from(error: &io::Error) -> Option<Self> {
        let mut current: &(dyn std::error::Error + 'static) = error;
        loop {
            if let Some(pending) = current.downcast_ref::<PendingRetirement>() {
                return pending.owners.lock().unwrap_or_else(|p| p.into_inner()).take();
            }
            current =
                if let Some(io) = current.downcast_ref::<io::Error>() { io.get_ref()? } else { current.source()? };
        }
    }

    fn with_cause(self, cause: io::Error) -> io::Error {
        io::Error::new(cause.kind(), PendingRetirement { cause, owners: Mutex::new(Some(self)) })
    }
}

struct PendingRetirement {
    cause: io::Error,
    owners: Mutex<Option<CompressedSaveRetirement>>,
}
impl std::fmt::Debug for PendingRetirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingCompressedRetirement").field("cause", &self.cause).finish_non_exhaustive()
    }
}
impl std::fmt::Display for PendingRetirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.cause.fmt(f)?;
        let owners = self.owners.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(owners) = owners.as_ref() {
            if let Some(body) = &owners.body {
                if let Some(path) = body.backing_path() {
                    write!(f, "; gzip body retirement pending at {}", path.display())?;
                }
                if let Some(error) = body.cleanup_error() {
                    write!(f, ": {error}")?;
                }
            }
            if let Some(decoded) = &owners.decoded {
                match decoded {
                    RetirementError::LiveSnapshots(owner) => {
                        write!(f, "; decoded checkpoint pinned at {}", owner.snapshot().paths().directory.display())?
                    }
                    RetirementError::Cleanup { paths, error } => {
                        write!(f, "; decoded cleanup pending at {}: {error}", paths.directory.display())?
                    }
                }
            }
        } else {
            f.write_str("; gzip checkpoint retirement retained by worker")?;
        }
        Ok(())
    }
}
impl std::error::Error for PendingRetirement {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

#[derive(Default)]
struct PreparedCheckpoint {
    snapshot: Option<Snapshot>,
    body: Option<PreparedFile>,
    decoded: Option<DecodedBacking>,
    changed: bool,
    normalized: bool,
}
fn discard(mut prepared: PreparedCheckpoint, cause: io::Error) -> io::Error {
    drop(prepared.snapshot.take());
    let owners = CompressedSaveRetirement {
        body: prepared.body.take().or_else(|| PreparedFile::take_from(&cause)),
        decoded: prepared
            .decoded
            .take()
            .map(RetirementError::LiveSnapshots)
            .or_else(|| RetirementError::take_from(&cause)),
    };
    match owners.retry() {
        Ok(()) => cause,
        Err(owners) => owners.with_cause(cause),
    }
}

fn cancelled(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(io::Error::new(io::ErrorKind::Interrupted, "gzip save cancelled before replacement"))
    } else {
        Ok(())
    }
}

/// Streams the exact encoded payload into a finalized gzip temp. Decompresses,
/// decodes and indexes the logical checkpoint BEFORE replacement. Cancellation
/// after replacement never turns a committed write into a retryable save.
pub fn save_compressed_snapshot(
    request: SaveRequest,
    plan: SavePlan,
    private_parent: &Path,
    cancel: &AtomicBool,
) -> io::Result<CompressedSavedSnapshot> {
    save_with_operations(request, plan, private_parent, cancel, replace, sync_parent, || cancel.load(Ordering::Relaxed))
}

fn save_with_operations(
    request: SaveRequest,
    plan: SavePlan,
    private_parent: &Path,
    cancel: &AtomicBool,
    replace: impl FnOnce(&Path, &Path, Option<&Path>) -> io::Result<()>,
    sync: impl FnOnce(&Path, &Path) -> io::Result<()>,
    mut index_cancel: impl FnMut() -> bool,
) -> io::Result<CompressedSavedSnapshot> {
    cancelled(cancel)?;
    validate_encoded_snapshot(&request.snapshot, plan, || cancel.load(Ordering::Relaxed))?;
    cancelled(cancel)?;
    let mut spool = if request.trim_trailing_whitespace { Some(TrimSpool::new()?) } else { None };
    let trimmed_rows = if let Some(spool) = &mut spool {
        visit_trimmed(&request.snapshot, spool, || cancelled(cancel), |_| Ok(()))?
    } else {
        None
    };
    if let Some(spool) = spool.as_ref().filter(|spool| !spool.is_empty()) {
        stream(&request.snapshot, Some(spool), plan, &mut io::sink(), cancel)?;
    }
    let submitted = request.snapshot.clone();
    let destination = std::path::absolute(&request.path)?;
    let mut prepared = PreparedCheckpoint::default();
    let committed = commit_with_operations(
        request,
        |snapshot, file| {
            recompress_payload(file, cancel, |mut writer| stream(snapshot, spool.as_ref(), plan, &mut writer, cancel))
                .map_err(crate::compressed::CompressionError::into_io_error)?;
            Ok(trimmed_rows)
        },
        |temp| {
            prepare_checkpoint(
                temp,
                &destination,
                &submitted,
                spool.as_ref(),
                plan,
                private_parent,
                cancel,
                &mut index_cancel,
                &mut prepared,
            )
        },
        |temp, destination, backup| {
            cancelled(cancel)?;
            replace(temp, destination, backup)
        },
        sync,
    );
    let committed = match committed {
        Ok(committed) => committed,
        Err(error) => return Err(discard(prepared, error)),
    };
    let file = match TextFile::open_saved_version(&committed.path) {
        Ok(file) => file,
        Err(error) => {
            return Err(discard(
                prepared,
                committed_error(&committed.path, "compressed destination reopen", error.into()),
            ));
        }
    };
    let mut saved = committed.finish(file, prepared.snapshot.take().expect("prepared logical checkpoint"));
    saved.trim_deletions = spool.filter(|spool| !spool.is_empty());
    saved.normalized = prepared.normalized;
    Ok(CompressedSavedSnapshot {
        saved,
        owners: CompressedCheckpointOwners {
            body: prepared.body.take().unwrap(),
            decoded: prepared.decoded.take().unwrap(),
        },
        logical_coordinates_changed: prepared.changed,
    })
}

#[allow(clippy::too_many_arguments)] // One preparation context, shared with fault tests.
fn prepare_checkpoint(
    temp: &Path,
    destination: &Path,
    submitted: &Snapshot,
    spool: Option<&TrimSpool>,
    plan: SavePlan,
    private_parent: &Path,
    cancel: &AtomicBool,
    index_cancel: &mut impl FnMut() -> bool,
    prepared: &mut PreparedCheckpoint,
) -> io::Result<()> {
    cancelled(cancel)?;
    // This reader closes before Windows attempts replacement. Both retained
    // owners refer exclusively to different private files.
    let body = prepare_gzip_payload(destination, &mut File::open(temp)?, private_parent, cancel)
        .map_err(crate::compressed::CompressionError::into_io_error)?;
    prepared.body = Some(body);
    let decoded = DecodedBacking::prepare(
        prepared.body.as_ref().unwrap().text_file().unwrap(),
        Interpretation::new(plan.encoding, bom_len(plan)).map_err(BackingError::into_io_error)?,
        private_parent,
        cancel,
    )
    .map_err(BackingError::into_io_error)?;
    prepared.decoded = Some(decoded);
    let origin = prepared.decoded.as_ref().unwrap().snapshot();
    prepared.snapshot = Some(Buffer::from_decoded_cancellable(origin.clone(), index_cancel)?.snapshot());
    prepared.changed = !same_logical(submitted, origin.logical_bytes(), cancel)?;
    prepared.normalized = if let Some(spool) = spool.filter(|spool| !spool.is_empty()) {
        !same_after_trim(submitted, spool, origin.logical_bytes(), cancel)?
    } else {
        prepared.changed
    };
    cancelled(cancel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compressed::{CompressionFormat, CompressionMode, prepare_file};
    use flate2::{Compression, read::MultiGzDecoder, write::GzEncoder};
    use kwil_core_text::encoding::{Encoding, LineEnding};
    use std::{
        cell::{Cell, RefCell},
        io::{Read, Write},
    };

    struct Fixture {
        root: PathBuf,
        opened: RefCell<Vec<CompressedCheckpointOwners>>,
    }
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "kwil-gzip-save-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self { root, opened: RefCell::new(Vec::new()) }
        }
        fn path(&self) -> PathBuf {
            self.root.join("compressed-original")
        }
        fn request(&self, buffer: &Buffer, trim: bool) -> SaveRequest {
            SaveRequest {
                compression: crate::compressed::CompressionFormat::Gzip,
                snapshot: buffer.snapshot(),
                path: self.path(),
                original: None,
                revision: 29,
                trim_trailing_whitespace: trim,
            }
        }
        fn open(&self, payload: &[u8], encoding: Encoding, bom: usize) -> Buffer {
            fs::write(self.path(), payload).unwrap();
            let body = prepare_file(&self.path(), CompressionMode::Auto, &self.root, &AtomicBool::new(false)).unwrap();
            assert_eq!(body.format(), CompressionFormat::Gzip);
            let decoded = DecodedBacking::prepare(
                body.text_file().unwrap(),
                Interpretation::new(encoding, bom).unwrap(),
                &self.root,
                &AtomicBool::new(false),
            )
            .unwrap();
            let buffer = Buffer::from_decoded(decoded.snapshot());
            self.opened.borrow_mut().push(CompressedCheckpointOwners { body, decoded });
            buffer
        }
        fn save(&self, request: SaveRequest, plan: SavePlan) -> (SavedSnapshot, bool) {
            let result = save_compressed_snapshot(request, plan, &self.root, &AtomicBool::new(false)).unwrap();
            assert_eq!(result.owners.body.source_path(), self.path());
            assert_eq!(result.owners.body.text_file().unwrap().resident_original_bytes(), 0);
            assert_eq!(result.owners.decoded.snapshot().raw_bytes(), unzip(&fs::read(self.path()).unwrap()));
            self.opened.borrow_mut().push(result.owners);
            (result.saved, result.logical_coordinates_changed)
        }
        fn only_destination(&self) {
            assert_eq!(fs::read_dir(&self.root).unwrap().count(), 1, "private temp/checkpoint was not retired");
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            for owner in self.opened.get_mut().drain(..) {
                assert!(owner.try_retire().is_ok(), "test leaked a logical/body snapshot");
            }
            fs::remove_dir_all(&self.root).unwrap();
        }
    }
    fn plan(encoding: Encoding, bom: bool) -> SavePlan {
        SavePlan { encoding, bom, line_ending: None }
    }
    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }
    fn unzip(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        MultiGzDecoder::new(bytes).read_to_end(&mut out).unwrap();
        out
    }
    fn logical(snapshot: &Snapshot) -> Vec<u8> {
        let mut out = Vec::new();
        snapshot.write_to(&mut out).unwrap();
        out
    }
    fn plain(bytes: &[u8]) -> Buffer {
        let mut buffer = Buffer::empty();
        buffer.insert(0, bytes).unwrap();
        buffer
    }

    #[test]
    fn gzip_utf16_bom_malformed_edit_save_undo_and_repeated_save() {
        let f = Fixture::new();
        let mut buffer = f.open(&gzip(b"\xff\xfeA\0\0\xd8B\0"), Encoding::UTF16_LE, 2);
        let old = buffer.snapshot();
        buffer.insert(1, "é".as_bytes()).unwrap();
        let (saved, changed) = f.save(f.request(&buffer, false), plan(Encoding::UTF16_LE, true));
        assert!(!changed);
        assert_eq!(unzip(&fs::read(f.path()).unwrap()), b"\xff\xfeA\0\xe9\0\0\xd8B\0");
        assert_eq!(logical(&saved.snapshot), b"A\xc3\xa9\xffB");
        assert_eq!(saved.file.path(), f.path());
        assert_eq!(saved.path, f.path());
        assert_eq!(saved.revision, 29);
        buffer.checkpoint_current(saved.snapshot).unwrap();
        assert!(buffer.undo());
        assert_eq!(logical(&buffer.snapshot()), logical(&old));
        let mut request = f.request(&buffer, false);
        request.original = Some(saved.file);
        let (saved, changed) = f.save(request, plan(Encoding::UTF16_LE, true));
        assert!(!changed);
        assert_eq!(unzip(&fs::read(f.path()).unwrap()), b"\xff\xfeA\0\0\xd8B\0");
        buffer.checkpoint_current(saved.snapshot).unwrap();
        assert!(buffer.redo());
        assert_eq!(logical(&buffer.snapshot()), b"A\xc3\xa9\xffB");
    }

    #[test]
    fn gzip_multiple_members_invalid_utf8_and_bom_keep_exact_typed_provenance() {
        let f = Fixture::new();
        let mut buffer =
            f.open(&[gzip(b"\xef\xbb\xbfA\xc0\xaf"), gzip(b""), gzip(b"\r\nB\xff")].concat(), Encoding::UTF8, 3);
        buffer.insert(1, b"!").unwrap();
        let (saved, changed) = f.save(f.request(&buffer, false), plan(Encoding::UTF8, true));
        assert!(!changed);
        assert_eq!(unzip(&fs::read(f.path()).unwrap()), b"\xef\xbb\xbfA!\xc0\xaf\r\nB\xff");
        assert_eq!(logical(&saved.snapshot), logical(&buffer.snapshot()));
        let mut restored = Vec::new();
        kwil_core_text::encoding::write_snapshot(&saved.snapshot, plan(Encoding::UTF8, true), &mut restored).unwrap();
        assert_eq!(restored, b"\xef\xbb\xbfA!\xc0\xaf\r\nB\xff");
    }

    #[test]
    fn gzip_trim_and_endings_stream_one_bom_and_submitted_spool_coordinates() {
        let f = Fixture::new();
        let buffer = plain(b"A\r \nB \t\r\nC  \rD \nE \t");
        let (saved, changed) =
            f.save(f.request(&buffer, true), plan(Encoding::UTF16_BE, true).change_line_endings(LineEnding::Lf));
        assert!(changed);
        assert!(saved.normalized);
        assert_eq!(logical(&saved.snapshot), b"A\nB\nC\nD\nE");
        assert_eq!(unzip(&fs::read(f.path()).unwrap()), b"\xfe\xff\0A\0\n\0B\0\n\0C\0\n\0D\0\n\0E");
        let deletions =
            saved.trim_deletions.as_ref().unwrap().ranges().unwrap().collect::<io::Result<Vec<_>>>().unwrap();
        assert_eq!(deletions.first().cloned(), Some(2..3));
    }

    #[test]
    fn gzip_preflight_and_index_cancellation_leave_original_byte_identical() {
        let f = Fixture::new();
        fs::write(f.path(), gzip(b"old")).unwrap();
        let before = fs::read(f.path()).unwrap();
        let buffer = plain(b"new");
        let error = save_compressed_snapshot(
            f.request(&buffer, false),
            plan(Encoding::UTF8, false),
            &f.root,
            &AtomicBool::new(true),
        )
        .err()
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        f.only_destination();
        let reached_index = Cell::new(false);
        let error = save_with_operations(
            f.request(&buffer, false),
            plan(Encoding::UTF8, false),
            &f.root,
            &AtomicBool::new(false),
            |_, _, _| panic!("cancelled preparation must not replace"),
            sync_parent,
            || {
                reached_index.set(true);
                true
            },
        )
        .err()
        .unwrap();
        assert!(reached_index.get());
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(fs::read(f.path()).unwrap(), before);
        f.only_destination();
    }

    #[test]
    fn gzip_cancel_at_last_precommit_boundary_does_not_replace() {
        let f = Fixture::new();
        fs::write(f.path(), gzip(b"old")).unwrap();
        let before = fs::read(f.path()).unwrap();
        let buffer = plain(b"new");
        let cancel = AtomicBool::new(false);
        let error = save_with_operations(
            f.request(&buffer, false),
            plan(Encoding::UTF8, false),
            &f.root,
            &cancel,
            |_, _, _| panic!("final cancellation must stop replacement"),
            sync_parent,
            || {
                cancel.store(true, Ordering::Relaxed);
                false
            },
        )
        .err()
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(fs::read(f.path()).unwrap(), before);
        f.only_destination();
    }

    #[test]
    fn gzip_postreplace_cancellation_is_a_committed_success() {
        let f = Fixture::new();
        let buffer = plain(b"committed");
        let cancel = AtomicBool::new(false);
        let result = save_with_operations(
            f.request(&buffer, false),
            plan(Encoding::UTF8, false),
            &f.root,
            &cancel,
            |temp, destination, backup| {
                replace(temp, destination, backup)?;
                cancel.store(true, Ordering::Relaxed);
                Ok(())
            },
            sync_parent,
            || false,
        )
        .unwrap();
        assert_eq!(logical(&result.saved.snapshot), b"committed");
        assert_eq!(unzip(&fs::read(f.path()).unwrap()), b"committed");
        f.opened.borrow_mut().push(result.owners);
    }

    #[test]
    fn gzip_replace_failure_retires_both_prepared_owners_and_preserves_destination() {
        let f = Fixture::new();
        fs::write(f.path(), gzip(b"old")).unwrap();
        let before = fs::read(f.path()).unwrap();
        let buffer = plain(b"new");
        let error = save_with_operations(
            f.request(&buffer, false),
            plan(Encoding::UTF8, false),
            &f.root,
            &AtomicBool::new(false),
            |_, _, _| Err(io::Error::other("injected replace failure")),
            sync_parent,
            || false,
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("injected replace failure"));
        assert_eq!(fs::read(f.path()).unwrap(), before);
        f.only_destination();
    }

    #[test]
    fn gzip_corrupt_crc_or_missing_trailer_never_replaces_original() {
        let f = Fixture::new();
        fs::write(f.path(), gzip(b"old")).unwrap();
        let before = fs::read(f.path()).unwrap();
        let buffer = plain(b"new");
        let good = gzip(b"new");
        let mut corrupt = good.clone();
        let at = corrupt.len() - 8;
        corrupt[at] ^= 1;
        for payload in [corrupt, good[..good.len() - 1].to_vec()] {
            let submitted = buffer.snapshot();
            let mut prepared = PreparedCheckpoint::default();
            let cancel = AtomicBool::new(false);
            let error = commit_with_operations(
                f.request(&buffer, false),
                |_, file| {
                    file.write_all(&payload)?;
                    Ok(None)
                },
                |temp| {
                    prepare_checkpoint(
                        temp,
                        &f.path(),
                        &submitted,
                        None,
                        plan(Encoding::UTF8, false),
                        &f.root,
                        &cancel,
                        &mut || false,
                        &mut prepared,
                    )
                },
                |_, _, _| panic!("corrupt gzip must fail before replacement"),
                sync_parent,
            )
            .err()
            .unwrap();
            let error = discard(prepared, error);
            assert!(error.to_string().contains("corrupt or invalid gzip stream"));
            assert_eq!(fs::read(f.path()).unwrap(), before);
            f.only_destination();
        }
    }

    #[test]
    fn gzip_postcommit_directory_failure_keeps_saved_bytes_and_committed_context() {
        let f = Fixture::new();
        let buffer = plain(b"committed directory failure");
        let error = save_with_operations(
            f.request(&buffer, false),
            plan(Encoding::UTF8, false),
            &f.root,
            &AtomicBool::new(false),
            replace,
            |destination, _| {
                Err(committed_error(
                    destination,
                    "parent directory sync",
                    io::Error::other("injected durability failure"),
                ))
            },
            || false,
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("file committed at"));
        assert!(error.to_string().contains("do not retry as an uncommitted save"));
        assert_eq!(unzip(&fs::read(f.path()).unwrap()), b"committed directory failure");
        f.only_destination();
    }

    #[test]
    fn gzip_cleanup_error_token_keeps_both_resources_through_committed_context() {
        let f = Fixture::new();
        let buffer = plain(b"new");
        let moved = f.root.join("committed-copy");
        let error = save_with_operations(
            f.request(&buffer, false),
            plan(Encoding::UTF8, false),
            &f.root,
            &AtomicBool::new(false),
            |temp, destination, backup| {
                replace(temp, destination, backup)?;
                fs::rename(destination, &moved)?;
                for entry in fs::read_dir(&f.root)? {
                    let entry = entry?;
                    if entry.file_type()?.is_dir() {
                        fs::write(entry.path().join("blocker"), b"retained")?;
                    }
                }
                Ok(())
            },
            sync_parent,
            || false,
        )
        .err()
        .unwrap();
        assert_eq!(unzip(&fs::read(&moved).unwrap()), b"new");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("file committed at"));
        let pending = CompressedSaveRetirement::take_from(&error).expect("both resources travel together");
        assert!(CompressedSaveRetirement::take_from(&error).is_none());
        assert!(pending.body.is_some());
        assert!(pending.decoded.is_some());
        assert!(error.to_string().contains("do not retry as an uncommitted save"));
        for entry in fs::read_dir(&f.root).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                fs::remove_file(entry.path().join("blocker")).unwrap();
            }
        }
        assert!(pending.retry().is_ok());
        assert_eq!(fs::read_dir(&f.root).unwrap().count(), 1);
    }

    #[test]
    fn gzip_retirement_keeps_live_body_and_typed_decoded_snapshot_readable() {
        let f = Fixture::new();
        let buffer = plain(b"both owners pinned");
        let result = save_compressed_snapshot(
            f.request(&buffer, false),
            plan(Encoding::UTF8, false),
            &f.root,
            &AtomicBool::new(false),
        )
        .unwrap();
        let body_pin = result.owners.body.text_file().unwrap().clone();
        let logical_pin = result.saved.snapshot.clone();
        drop(result.saved);
        let pending = match result.owners.try_retire() {
            Err(pending) => pending,
            Ok(()) => panic!("live pins must block both owners"),
        };
        assert!(pending.body.is_some());
        assert!(pending.decoded.is_some());
        assert_eq!(body_pin.bytes().unwrap(), b"both owners pinned");
        assert_eq!(logical(&logical_pin), b"both owners pinned");
        drop(body_pin);
        drop(logical_pin);
        assert!(pending.retry().is_ok());
        f.only_destination();
    }

    #[test]
    fn gzip_discard_keeps_live_body_and_decoded_owners_in_one_error() {
        let f = Fixture::new();
        let buffer = plain(b"pinned discard");
        let result = save_compressed_snapshot(
            f.request(&buffer, false),
            plan(Encoding::UTF8, false),
            &f.root,
            &AtomicBool::new(false),
        )
        .unwrap();
        let body_pin = result.owners.body.text_file().unwrap().clone();
        let logical_pin = result.saved.snapshot.clone();
        let prepared = PreparedCheckpoint {
            snapshot: Some(result.saved.snapshot),
            body: Some(result.owners.body),
            decoded: Some(result.owners.decoded),
            ..PreparedCheckpoint::default()
        };
        let error = discard(prepared, io::Error::new(io::ErrorKind::Interrupted, "injected preparation failure"));
        let pending = CompressedSaveRetirement::take_from(&error).unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(pending.body.is_some());
        assert!(pending.decoded.is_some());
        assert_eq!(logical(&logical_pin), b"pinned discard");
        drop(body_pin);
        drop(logical_pin);
        assert!(pending.retry().is_ok());
    }
    include!("utf32_compressed_tests.rs");
}
