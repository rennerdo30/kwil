//! Bounded worker ownership for decoded opens and encoded save checkpoints.
//! Construct and use on the document worker. Admission precedes preparation;
//! snapshots never become responsible for unlinking their backing files.
use super::prepare_decoded_document;
use crate::Document;
use kwil_core_text::{
    TextFile,
    decoded_backing::{BackingPaths, DecodedBacking, DecodedPreview, RetirementError},
    encoding::{Encoding, SavePlan},
};
use kwil_core_workspace::{
    CompressedSaveRetirement, SaveRequest, SavedSnapshot,
    compressed::{CompressionFormat, CompressionMode, DetectedFormat, PreparedFile, detect_magic, prepare_file},
};
use std::{
    io,
    marker::PhantomData,
    mem::size_of,
    path::{Path, PathBuf},
    rc::Rc,
    sync::atomic::AtomicBool,
};

/// Caller-owned resource policy; this module introduces no product defaults.
pub struct BackingLimits {
    pub max_backings: usize,
    /// Covers private paths and the source/temporary pathname retained by an
    /// origin. Saving reserves the generated same-directory suffix as well.
    pub max_private_path_bytes: usize,
    pub max_metadata_bytes: usize,
}

enum Resource {
    Raw(TextFile),
    Owner(DecodedBacking),
    Cleanup(Box<BackingPaths>),
    Compressed(Box<CompressedSaveRetirement>),
}

#[derive(Default)]
pub struct RetirementReport {
    pub retired: usize,
    pub live: usize,
    pub cleanup_pending: usize,
    pub first_error: Option<io::Error>,
}

/// The checkpoint's owner has already been retained on the worker. A changed
/// logical coordinate system needs a model transaction, not a plain checkpoint.
pub struct WorkerSavedSnapshot {
    pub saved: SavedSnapshot,
    pub logical_coordinates_changed: bool,
}

/// A bounded, non-transferable worker registry. No cleanup occurs in Drop.
/// Cleanup failure retains the exact paths; live pins retain the original owner.
pub struct WorkerBackings {
    parent: PathBuf,
    slots: Vec<Option<Resource>>,
    metadata_reserved: usize,
    path_cap: usize,
    // Prevent accidentally publishing this registry in a UI completion.
    _worker: PhantomData<Rc<()>>,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

impl WorkerBackings {
    /// The absolute app-private parent must already exist. Construction performs
    /// no filesystem access. Reserve registry/path metadata before any origin I/O.
    pub fn new(parent: PathBuf, limits: BackingLimits) -> io::Result<Self> {
        if !parent.is_absolute() {
            return Err(invalid("backing parent must be absolute and app-private"));
        }
        if parent
            .as_os_str()
            .as_encoded_bytes()
            .len()
            .checked_add(64)
            .is_none_or(|bytes| bytes > limits.max_private_path_bytes)
        {
            return Err(invalid("backing parent exceeds private path admission cap"));
        }
        // Core's generated suffix fits64 native bytes. Preparation/retirement
        // may coexist with up to20 path allocations and2x growth capacity
        // (the compressed body adds four to the decoded owner's sixteen).
        // Match replay owner accounting; mappings and fixed decode scratch are
        // separate from resident metadata. No file-sized heap reservation.
        let per_slot = limits
            .max_private_path_bytes
            // Compressed checkpoints retain both the decompressed encoded
            // body and the logical decoded owner, including failure paths.
            .checked_mul(40)
            .and_then(|bytes| bytes.checked_add(size_of::<BackingPaths>() * 2))
            .and_then(|bytes| bytes.checked_add(size_of::<CompressedSaveRetirement>()))
            .and_then(|bytes| bytes.checked_add(size_of::<Option<Resource>>()))
            .ok_or_else(|| invalid("backing metadata admission overflow"))?;
        let metadata_reserved = limits
            .max_backings
            .checked_mul(per_slot)
            .and_then(|bytes| bytes.checked_add(size_of::<Self>()))
            // Retain the caller's allocation, including unused capacity.
            .and_then(|bytes| bytes.checked_add(parent.capacity()))
            .ok_or_else(|| invalid("backing metadata admission overflow"))?;
        if metadata_reserved > limits.max_metadata_bytes {
            return Err(invalid("backing metadata admission limit"));
        }
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(limits.max_backings)
            .map_err(|_| io::Error::other("backing registry allocation failed"))?;
        slots.resize_with(limits.max_backings, || None);
        Ok(Self { parent, slots, metadata_reserved, path_cap: limits.max_private_path_bytes, _worker: PhantomData })
    }

    pub fn metadata_reserved(&self) -> usize {
        self.metadata_reserved
    }

    pub fn retained(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }

    pub(super) fn retain_raw(&mut self, file: &TextFile) -> io::Result<()> {
        self.check_input_path(file.path(), 0)?;
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "raw source backing capacity retained"))?;
        *slot = Some(Resource::Raw(file.clone()));
        Ok(())
    }

    pub(super) fn open_raw(
        &mut self,
        document: kwil_core_workspace::session::DocumentId,
        path: PathBuf,
        rows: usize,
        generation: u64,
        cancel: &AtomicBool,
        publish: impl FnOnce(crate::hex_session::Page),
    ) -> io::Result<Document> {
        let path = std::path::absolute(&path)?;
        self.check_input_path(&path, 0)?;
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "raw source backing capacity retained"))?;
        let document = super::hex_runtime::open(document, path, rows, generation, cancel, publish)?;
        *slot = Some(Resource::Raw(document.original.as_ref().expect("raw named original").clone()));
        Ok(document)
    }

    /// Same physical transport, using the exact owner held by a binary offer.
    pub(super) fn open_raw_file(
        &mut self,
        document: kwil_core_workspace::session::DocumentId,
        pending: super::binary_offer::Pending,
        rows: usize,
        generation: u64,
        cancel: &AtomicBool,
        publish: impl FnOnce(crate::hex_session::Page),
    ) -> io::Result<Document> {
        let super::binary_offer::Pending { path, file } = pending;
        self.check_input_path(&path, 0)?;
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "raw source backing capacity retained"))?;
        let document = super::hex_runtime::open_file(document, path, file, rows, generation, cancel, publish)?;
        *slot = Some(Resource::Raw(document.original.as_ref().expect("raw named original").clone()));
        Ok(document)
    }

    fn check_input_path(&self, path: &Path, suffix: usize) -> io::Result<()> {
        if path.as_os_str().as_encoded_bytes().len().checked_add(suffix).is_none_or(|bytes| bytes > self.path_cap) {
            return Err(invalid("source or save path exceeds backing metadata admission cap"));
        }
        Ok(())
    }
    fn prepare<T>(&mut self, make: impl FnOnce(&Path) -> io::Result<(T, DecodedBacking)>) -> io::Result<T> {
        // Poll explicitly between requests; never hide a cleanup retry or cold
        // filesystem operation inside capacity admission.
        let slot = self.slots.iter_mut().find(|slot| slot.is_none()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::WouldBlock, "decoded backing capacity retained by snapshots or cleanup")
        })?;
        match make(&self.parent) {
            Ok((value, owner)) => {
                *slot = Some(Resource::Owner(owner));
                Ok(value)
            }
            Err(error) => {
                *slot = RetirementError::take_from(&error).map(|retirement| match retirement {
                    RetirementError::LiveSnapshots(owner) => Resource::Owner(owner),
                    RetirementError::Cleanup { paths, .. } => Resource::Cleanup(paths),
                });
                Err(error)
            }
        }
    }

    pub fn open(
        &mut self,
        path: PathBuf,
        file: TextFile,
        interpretation: Option<Encoding>,
        cancel: &AtomicBool,
        publish: impl FnOnce(DecodedPreview) -> io::Result<()>,
    ) -> io::Result<Document> {
        // DecodedBacking also retains the external source name, independently
        // of its generated private paths. Check the file's actual source name.
        self.check_input_path(file.path(), 0)?;
        match source_format(&file)? {
            DetectedFormat::Raw => (),
            DetectedFormat::Unsupported(format) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("unsupported compressed format: {format}"),
                ));
            }
            DetectedFormat::Gzip => return self.open_compressed(path, file, interpretation, cancel, publish),
        }
        self.prepare(|parent| {
            let prepared = prepare_decoded_document(path, file, interpretation, parent, cancel, publish)?;
            Ok((prepared.document, prepared.backing))
        })
    }

    fn open_compressed(
        &mut self,
        path: PathBuf,
        original: TextFile,
        interpretation: Option<Encoding>,
        cancel: &AtomicBool,
        publish: impl FnOnce(DecodedPreview) -> io::Result<()>,
    ) -> io::Result<Document> {
        let slot = self.slots.iter_mut().find(|slot| slot.is_none()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::WouldBlock, "compressed backing capacity retained by snapshots or cleanup")
        })?;
        let mut owners = CompressedSaveRetirement { body: None, decoded: None };
        let result = (|| {
            owners.body = Some(
                prepare_file(original.path(), CompressionMode::Auto, &self.parent, cancel)
                    .map_err(kwil_core_workspace::compressed::CompressionError::into_io_error)?,
            );
            require_unchanged(&original)?;
            let file = owners.body.as_ref().unwrap().text_file().unwrap().clone();
            let prepared = prepare_decoded_document(path, file, interpretation, &self.parent, cancel, publish)?;
            owners.decoded = Some(RetirementError::LiveSnapshots(prepared.backing));
            let mut document = prepared.document;
            // Save/watch/journal identity always names the compressed source.
            document.original = Some(original.clone());
            document.compression = CompressionFormat::Gzip;
            require_unchanged(&original)?;
            Ok(document)
        })();
        if let Err(error) = &result {
            owners.body = owners.body.take().or_else(|| PreparedFile::take_from(error));
            owners.decoded = owners.decoded.take().or_else(|| RetirementError::take_from(error));
        }
        if owners.body.is_some() || owners.decoded.is_some() {
            *slot = Some(Resource::Compressed(Box::new(owners)));
        }
        result
    }

    /// Validate a clean recovered encoded payload against its original
    /// transport without ever comparing logical text to gzip container bytes.
    /// Any partial/failed body owner is retained before returning an error.
    pub(super) fn verify_source(
        &mut self,
        original: &TextFile,
        compression: CompressionFormat,
        cancel: &AtomicBool,
        compare: impl FnOnce(&TextFile) -> io::Result<bool>,
    ) -> io::Result<bool> {
        self.check_input_path(original.path(), 0)?;
        let detected = source_format(original)?;
        match (compression, detected) {
            (CompressionFormat::Raw, DetectedFormat::Raw) => return compare(original),
            (CompressionFormat::Gzip, DetectedFormat::Gzip) => (),
            _ => return Ok(false),
        }
        let slot =
            self.slots.iter_mut().find(|slot| slot.is_none()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::WouldBlock, "compressed verification capacity retained")
            })?;
        let mut owners = CompressedSaveRetirement { body: None, decoded: None };
        let result = (|| {
            owners.body = Some(
                prepare_file(original.path(), CompressionMode::Gzip, &self.parent, cancel)
                    .map_err(kwil_core_workspace::compressed::CompressionError::into_io_error)?,
            );
            require_unchanged(original)?;
            let equal = compare(owners.body.as_ref().unwrap().text_file().unwrap())?;
            require_unchanged(original)?;
            Ok(equal)
        })();
        if let Err(error) = &result {
            owners.body = owners.body.take().or_else(|| PreparedFile::take_from(error));
        }
        if owners.body.is_some() {
            *slot = Some(Resource::Compressed(Box::new(owners)));
        }
        result
    }

    pub fn save(
        &mut self,
        mut request: SaveRequest,
        plan: SavePlan,
        cancel: &AtomicBool,
    ) -> io::Result<WorkerSavedSnapshot> {
        // Admission and the atomic writer must use the same destination. A
        // relative spelling alone does not bound the retained temporary name.
        request.path = std::path::absolute(&request.path)?;
        self.check_input_path(&request.path, 0)?;
        // The checkpoint is prepared from .kwil-save-<u32 PID>-<u64 counter>,
        // not the final destination name. Charge its maximum generated suffix.
        self.check_input_path(request.path.parent().ok_or_else(|| invalid("save destination has no parent"))?, 64)?;
        if let Some(original) = &request.original {
            self.check_input_path(original.path(), 0)?;
            match source_format(original)? {
                DetectedFormat::Raw => (),
                DetectedFormat::Unsupported(format) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        format!("unsupported compressed save format: {format}"),
                    ));
                }
                DetectedFormat::Gzip if request.compression == CompressionFormat::Raw => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "gzip original requires explicit gzip output policy",
                    ));
                }
                DetectedFormat::Gzip => (),
            }
        }
        match request.compression {
            CompressionFormat::Gzip => {
                let slot = self.slots.iter_mut().find(|slot| slot.is_none()).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::WouldBlock, "compressed checkpoint capacity retained")
                })?;
                return match kwil_core_workspace::save_compressed_snapshot(request, plan, &self.parent, cancel) {
                    Ok(prepared) => {
                        *slot = Some(Resource::Compressed(Box::new(CompressedSaveRetirement {
                            body: Some(prepared.owners.body),
                            decoded: Some(RetirementError::LiveSnapshots(prepared.owners.decoded)),
                        })));
                        Ok(WorkerSavedSnapshot {
                            saved: prepared.saved,
                            logical_coordinates_changed: prepared.logical_coordinates_changed,
                        })
                    }
                    Err(error) => {
                        *slot = CompressedSaveRetirement::take_from(&error)
                            .map(|owners| Resource::Compressed(Box::new(owners)));
                        Err(error)
                    }
                };
            }
            CompressionFormat::Raw => (),
        }
        self.prepare(|parent| {
            let prepared = kwil_core_workspace::save_encoded_snapshot(request, plan, parent, cancel)?;
            Ok((
                WorkerSavedSnapshot {
                    saved: prepared.saved,
                    logical_coordinates_changed: prepared.logical_coordinates_changed,
                },
                prepared.backing,
            ))
        })
    }

    /// Explicit physical-byte output. In particular, a gzip magic prefix is
    /// data here and must not enter encoded/compressed checkpoint preparation.
    pub fn save_raw(&mut self, mut request: SaveRequest, cancel: &AtomicBool) -> io::Result<SavedSnapshot> {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        request.path = std::path::absolute(&request.path)?;
        self.check_input_path(&request.path, 0)?;
        self.check_input_path(request.path.parent().ok_or_else(|| invalid("save destination has no parent"))?, 64)?;
        if let Some(original) = &request.original {
            self.check_input_path(original.path(), 0)?;
        }
        request.trim_trailing_whitespace = false;
        request.compression = CompressionFormat::Raw;
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "raw save checkpoint capacity retained"))?;
        let saved = kwil_core_workspace::save_snapshot(request)?;
        *slot = Some(Resource::Raw(saved.file.clone()));
        Ok(saved)
    }

    /// One bounded pass, requested by the document worker after releasing old
    /// receipts/plans. Live owners do not cause a timer, spin, or idle wake.
    pub fn retire(&mut self) -> RetirementReport {
        let mut report = RetirementReport::default();
        for slot in &mut self.slots {
            let Some(resource) = slot.take() else { continue };
            let resource = match resource {
                Resource::Raw(file) => {
                    match file.try_close() {
                        Ok(()) => report.retired += 1,
                        Err(file) => {
                            report.live += 1;
                            *slot = Some(Resource::Raw(file));
                        }
                    }
                    continue;
                }
                Resource::Compressed(owners) => {
                    match owners.retry() {
                        Ok(()) => report.retired += 1,
                        Err(owners) => {
                            let cleanup = owners.body.as_ref().and_then(PreparedFile::cleanup_error).or({
                                match &owners.decoded {
                                    Some(RetirementError::Cleanup { error, .. }) => Some(error),
                                    _ => None,
                                }
                            });
                            if let Some(error) = cleanup {
                                report.cleanup_pending += 1;
                                if report.first_error.is_none() {
                                    report.first_error = Some(io::Error::new(error.kind(), error.to_string()));
                                }
                            } else {
                                report.live += 1;
                            }
                            *slot = Some(Resource::Compressed(Box::new(owners)));
                        }
                    }
                    continue;
                }
                other => other,
            };
            let outcome = match resource {
                Resource::Owner(owner) => owner.try_retire(),
                Resource::Cleanup(paths) => paths.cleanup().map_err(|error| RetirementError::Cleanup { paths, error }),
                Resource::Compressed(_) => unreachable!(),
                Resource::Raw(_) => unreachable!(),
            };
            match outcome {
                Ok(()) => report.retired += 1,
                Err(RetirementError::LiveSnapshots(owner)) => {
                    report.live += 1;
                    *slot = Some(Resource::Owner(owner));
                }
                Err(RetirementError::Cleanup { paths, error }) => {
                    report.cleanup_pending += 1;
                    if report.first_error.is_none() {
                        report.first_error = Some(error);
                    }
                    *slot = Some(Resource::Cleanup(paths));
                }
            }
        }
        report
    }
}

fn require_unchanged(file: &TextFile) -> io::Result<()> {
    if file.check()? != kwil_core_text::FileState::Unchanged {
        return Err(io::Error::other("compressed source changed on disk"));
    }
    Ok(())
}

pub(super) fn source_format(file: &TextFile) -> io::Result<DetectedFormat> {
    require_unchanged(file)?;
    let bytes = file.bytes().map_err(io::Error::other)?;
    Ok(detect_magic(&bytes[..bytes.len().min(8)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kwil_core_text::decoded_backing::Interpretation;
    use std::{
        cell::Cell,
        sync::atomic::{AtomicU64, Ordering},
    };

    #[test]
    fn pending_retirement_failed_preparation_occupies_slot_until_cleanup_retry() {
        let fixture = Fixture::new();
        let mut registry = fixture.registry(1);
        let cancel = AtomicBool::new(false);
        struct Cancels<'a> {
            parent: &'a Path,
            cancel: &'a AtomicBool,
        }
        impl std::io::Read for Cancels<'_> {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                let directory = std::fs::read_dir(self.parent)?
                    .map(Result::unwrap)
                    .find(|entry| entry.file_type().unwrap().is_dir())
                    .unwrap()
                    .path();
                std::fs::write(directory.join("blocker"), b"test blocker")?;
                self.cancel.store(true, Ordering::Relaxed);
                Err(io::Error::new(io::ErrorKind::Interrupted, "injected copy cancellation"))
            }
        }
        let error = registry
            .prepare(|parent| {
                let owner = DecodedBacking::prepare_from_reader(
                    &mut Cancels { parent, cancel: &cancel },
                    1,
                    Interpretation::new(Encoding::UTF8, 0).unwrap(),
                    parent,
                    &cancel,
                )
                .map_err(kwil_core_text::decoded_backing::BackingError::into_io_error)?;
                Ok(((), owner))
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(RetirementError::take_from(&error).is_none(), "registry must own the failed helper resource");
        assert_eq!(registry.retained(), 1);
        let called = Cell::new(false);
        let error = registry
            .prepare(|_| {
                called.set(true);
                Ok(((), fixture.owner()))
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(!called.get(), "pending cleanup must refuse before another private preparation");
        assert_eq!(registry.retire().cleanup_pending, 1);
        assert_eq!(registry.retained(), 1);
        let Some(Resource::Cleanup(paths)) = &registry.slots[0] else { panic!("retain exact cleanup paths") };
        let directory = paths.directory.clone();
        std::fs::remove_file(directory.join("blocker")).unwrap();
        assert_eq!(registry.retire().retired, 1);
        assert!(!directory.exists());
        assert_eq!(registry.retained(), 0);
        registry.prepare::<()>(|_| Err(io::Error::new(io::ErrorKind::NotFound, "ordinary failure"))).unwrap_err();
        assert_eq!(registry.retained(), 0, "ordinary failures without resources free the slot");
        registry.prepare(|_| Ok(((), fixture.owner()))).unwrap();
        assert_eq!(registry.retire().retired, 1);
    }

    #[test]
    fn pending_retirement_failed_index_callback_is_owned_before_error_returns() {
        let fixture = Fixture::new();
        let path = fixture.0.join("source");
        std::fs::write(&path, b"abc").unwrap();
        let file = TextFile::open(&path).unwrap();
        let mut registry = fixture.registry(1);
        let error = registry
            .prepare(|parent| {
                let prepared = crate::document_worker::decoded::prepare_with_index_check(
                    path,
                    file,
                    None,
                    parent,
                    &AtomicBool::new(false),
                    |_| {
                        assert_eq!(std::fs::read_dir(parent)?.count(), 1, "preview precedes private files");
                        Ok(())
                    },
                    || {
                        let directory = std::fs::read_dir(parent)
                            .unwrap()
                            .map(Result::unwrap)
                            .find(|entry| entry.file_type().unwrap().is_dir())
                            .unwrap()
                            .path();
                        std::fs::write(directory.join("blocker"), b"test blocker").unwrap();
                        true
                    },
                )?;
                Ok(((), prepared.backing))
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        let Some(Resource::Cleanup(paths)) = &registry.slots[0] else { panic!("failed callback must retain paths") };
        std::fs::remove_file(paths.directory.join("blocker")).unwrap();
        assert_eq!(registry.retire().retired, 1);
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 1);
    }

    #[test]
    fn pending_retirement_failed_callback_retains_actual_live_owner() {
        let fixture = Fixture::new();
        let mut registry = fixture.registry(1);
        let mut pin = None;
        let error = registry
            .prepare::<()>(|_| {
                let owner = fixture.owner();
                pin = Some(owner.snapshot());
                let retirement = owner.try_retire().err().unwrap();
                Err(retirement.with_cause(io::Error::new(io::ErrorKind::Interrupted, "failed callback")))
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(matches!(registry.slots[0], Some(Resource::Owner(_))), "retain the actual owner while pins exist");
        assert_eq!(registry.retained(), 1);
        assert_eq!(registry.retire().live, 1);
        assert_eq!(pin.as_ref().unwrap().logical_bytes(), b"abc");
        let directory = pin.as_ref().unwrap().paths().directory.clone();
        drop(pin);
        assert!(directory.exists());
        assert_eq!(registry.retire().retired, 1);
        assert!(!directory.exists());
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let parent = std::env::temp_dir().join(format!(
                "kwil-retirement-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&parent).unwrap();
            Self(parent)
        }
        fn registry(&self, slots: usize) -> WorkerBackings {
            WorkerBackings::new(
                self.0.clone(),
                BackingLimits { max_backings: slots, max_private_path_bytes: 1024, max_metadata_bytes: 1 << 20 },
            )
            .unwrap()
        }
        fn owner(&self) -> DecodedBacking {
            let source = self.0.join("source");
            std::fs::write(&source, b"abc").unwrap();
            DecodedBacking::prepare(
                &TextFile::open(&source).unwrap(),
                Interpretation::new(Encoding::UTF8, 0).unwrap(),
                &self.0,
                &AtomicBool::new(false),
            )
            .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // Test-only exact private fixture, never a user or production root.
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn backing_capacity_refuses_before_preparation_and_reuses_retired_slot() {
        let fixture = Fixture::new();
        let mut registry = fixture.registry(1);
        let snapshot = registry
            .prepare(|_| {
                let owner = fixture.owner();
                Ok((owner.snapshot(), owner))
            })
            .unwrap();
        let called = Cell::new(false);
        let result = registry.prepare(|_| {
            called.set(true);
            Ok(((), fixture.owner()))
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert!(!called.get(), "full admission must precede private preparation");
        assert_eq!(registry.retire().live, 1);
        assert_eq!(registry.retained(), 1);
        drop(snapshot);
        assert_eq!(registry.retire().retired, 1);
        registry.prepare(|_| Ok(((), fixture.owner()))).unwrap();
        assert_eq!(registry.retained(), 1);
        assert_eq!(registry.retire().retired, 1);
    }

    #[test]
    fn backing_cleanup_failure_retains_exact_paths_and_retries() {
        let fixture = Fixture::new();
        let mut registry = fixture.registry(1);
        let paths = registry
            .prepare(|_| {
                let owner = fixture.owner();
                let paths = owner.snapshot().paths().clone();
                Ok((paths, owner))
            })
            .unwrap();
        let blocker = paths.directory.join("blocker");
        std::fs::write(&blocker, b"owned test blocker").unwrap();
        let report = registry.retire();
        assert_eq!(report.cleanup_pending, 1);
        assert!(report.first_error.is_some());
        assert_eq!(registry.retained(), 1);
        assert!(paths.directory.exists());
        assert!(!paths.raw.exists(), "failed directory removal must not forget partial cleanup");
        std::fs::remove_file(blocker).unwrap();
        assert_eq!(registry.retire().retired, 1);
        assert_eq!(registry.retained(), 0);
        assert!(!paths.directory.exists());
    }

    #[test]
    fn backing_drop_never_unlinks_and_snapshot_pins_preserve_bytes() {
        let fixture = Fixture::new();
        let mut registry = fixture.registry(1);
        let snapshot = registry
            .prepare(|_| {
                let owner = fixture.owner();
                Ok((owner.snapshot(), owner))
            })
            .unwrap();
        let second = snapshot.clone();
        let paths = snapshot.paths().clone();
        drop(snapshot);
        assert_eq!(registry.retire().live, 1);
        assert_eq!(second.logical_bytes(), b"abc");
        drop(registry);
        assert!(paths.raw.exists(), "implicit registry drop must not unlink live backing");
        drop(second);
        paths.cleanup().unwrap();
        assert!(!paths.directory.exists());
    }

    #[test]
    fn backing_metadata_and_path_caps_are_checked_before_allocation() {
        let fixture = Fixture::new();
        let limits = |paths, metadata, count| BackingLimits {
            max_backings: count,
            max_private_path_bytes: paths,
            max_metadata_bytes: metadata,
        };
        assert!(WorkerBackings::new(PathBuf::from("relative"), limits(1024, usize::MAX, 1)).is_err());
        assert!(WorkerBackings::new(fixture.0.clone(), limits(1, usize::MAX, 1)).is_err());
        assert!(WorkerBackings::new(fixture.0.clone(), limits(1024, 1, 1)).is_err());
        assert!(WorkerBackings::new(fixture.0.clone(), limits(usize::MAX, usize::MAX, 1)).is_err());
        assert!(WorkerBackings::new(fixture.0.clone(), limits(1024, usize::MAX, usize::MAX)).is_err());
        let registry = fixture.registry(1);
        assert!(registry.metadata_reserved() >= 32 * 1024);
    }

    #[test]
    fn backing_open_and_save_keep_utf16_checkpoint_owned_through_undo() {
        let fixture = Fixture::new();
        let path = fixture.0.join("utf16.txt");
        std::fs::write(&path, b"\xff\xfeA\0").unwrap();
        let mut registry = fixture.registry(3);
        let cancel = AtomicBool::new(false);
        let document = registry
            .open(path.clone(), TextFile::open(&path).unwrap(), None, &cancel, |preview| {
                assert_eq!(preview.text, b"A");
                assert_eq!(std::fs::read_dir(&fixture.0)?.count(), 1, "first screen precedes backing files");
                Ok(())
            })
            .unwrap();
        let mut editor = crate::Editor::new(document);
        editor.execute("motion.lastLine", &crate::CommandArgs::default(), 0).unwrap();
        editor.execute("edit.insert", &crate::CommandArgs { text: Some("x".into()), ..Default::default() }, 1).unwrap();
        let plan = editor.document().save_plan();
        let request = editor.save_request(None).unwrap();
        let completion = registry.save(request, plan, &cancel).unwrap();
        assert!(!completion.logical_coordinates_changed);
        editor.finish_save(Ok(completion.saved)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"\xff\xfeA\0x\0");
        assert_eq!(registry.retire().live, 2, "old undo and new checkpoint origins stay worker-owned");
        editor.execute("edit.undo", &crate::CommandArgs::default(), 2).unwrap();
        let mut bytes = Vec::new();
        editor.document().snapshot().write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"A");
        drop(editor);
        assert_eq!(registry.retire().retired, 2);
        assert_eq!(registry.retained(), 0);
    }

    #[test]
    fn backing_full_save_refuses_before_touching_destination() {
        let fixture = Fixture::new();
        let path = fixture.0.join("utf16.txt");
        let raw = b"\xff\xfeA\0";
        std::fs::write(&path, raw).unwrap();
        let mut registry = fixture.registry(1);
        let cancel = AtomicBool::new(false);
        let document = registry.open(path.clone(), TextFile::open(&path).unwrap(), None, &cancel, |_| Ok(())).unwrap();
        let mut editor = crate::Editor::new(document);
        editor.execute("edit.insert", &crate::CommandArgs { text: Some("x".into()), ..Default::default() }, 1).unwrap();
        let request = editor.save_request(None).unwrap();
        let error = registry.save(request, editor.document().save_plan(), &cancel).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(std::fs::read(&path).unwrap(), raw);
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 2, "no save temporary or second backing");
        editor.finish_save(Err(error)).unwrap_err();
        drop(editor);
        assert_eq!(registry.retire().retired, 1);
    }

    #[test]
    fn backing_parent_spare_capacity_is_charged_even_without_slots() {
        let fixture = Fixture::new();
        let mut parent = fixture.0.clone();
        parent.reserve(4096);
        let bytes = parent.as_os_str().as_encoded_bytes().len();
        assert!(parent.capacity() > bytes + 64);
        let result = WorkerBackings::new(
            parent,
            BackingLimits {
                max_backings: 0,
                max_private_path_bytes: 8192,
                max_metadata_bytes: size_of::<WorkerBackings>() + bytes,
            },
        );
        assert!(result.is_err(), "retained parent capacity must fit the metadata cap");
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 0);
    }

    fn registry_with_short_path_cap(fixture: &Fixture) -> WorkerBackings {
        WorkerBackings::new(
            fixture.0.clone(),
            BackingLimits {
                max_backings: 1,
                max_private_path_bytes: fixture.0.as_os_str().as_encoded_bytes().len() + 64,
                max_metadata_bytes: 1 << 20,
            },
        )
        .unwrap()
    }

    #[test]
    fn backing_source_path_is_refused_before_preview_or_private_files() {
        let fixture = Fixture::new();
        let directory = fixture.0.join("s".repeat(100));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("input");
        std::fs::write(&path, b"abc").unwrap();
        let mut registry = registry_with_short_path_cap(&fixture);
        let error = registry
            .open(path.clone(), TextFile::open(&path).unwrap(), None, &AtomicBool::new(false), |_| {
                panic!("inadmissible source path must precede preview and preparation")
            })
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(registry.retained(), 0);
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 1);
    }

    fn assert_save_path_refused(component: usize) {
        let fixture = Fixture::new();
        let directory = fixture.0.join("d".repeat(component));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("f");
        std::fs::write(&path, b"keep").unwrap();
        let mut registry = registry_with_short_path_cap(&fixture);
        let mut editor = crate::Editor::new(Document::new());
        editor.execute("edit.insert", &crate::CommandArgs { text: Some("x".into()), ..Default::default() }, 1).unwrap();
        let request = editor.save_request(Some(path.clone())).unwrap();
        let result = registry.save(request, editor.document().save_plan(), &AtomicBool::new(false));
        assert!(result.is_err(), "over-limit save paths must refuse before preparation or replacement");
        let error = result.err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(registry.retained(), 0);
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1, "no same-directory temporary");
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 1, "no private checkpoint");
    }

    #[test]
    fn backing_long_save_destination_is_refused_before_replacement() {
        assert_save_path_refused(100);
    }

    #[test]
    fn backing_save_temporary_path_bound_is_checked_before_replacement() {
        assert_save_path_refused(40);
    }

    #[test]
    fn backing_relative_save_child() {
        let Some(parent) = std::env::var_os("KWIL_RETIREMENT_RELATIVE_CHILD") else { return };
        let parent = PathBuf::from(parent);
        assert!(parent.is_absolute());
        std::fs::write(parent.join("child-ran"), b"relative path admission").unwrap();
        let mut registry = WorkerBackings::new(
            parent.clone(),
            BackingLimits {
                max_backings: 1,
                max_private_path_bytes: parent.as_os_str().as_encoded_bytes().len() + 64,
                max_metadata_bytes: 1 << 20,
            },
        )
        .unwrap();
        let mut editor = crate::Editor::new(Document::new());
        editor.execute("edit.insert", &crate::CommandArgs { text: Some("x".into()), ..Default::default() }, 1).unwrap();
        let request = editor.save_request(Some(PathBuf::from("out.txt"))).unwrap();
        let result = registry.save(request, editor.document().save_plan(), &AtomicBool::new(false));
        assert!(result.is_err(), "expanded relative destination must be refused before replacement");
        assert_eq!(result.err().unwrap().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(std::fs::read("out.txt").unwrap(), b"keep");
        assert_eq!(registry.retained(), 0);
    }

    #[test]
    fn backing_relative_destination_uses_resolved_path_for_admission() {
        let fixture = Fixture::new();
        let directory = fixture.0.join("d".repeat(100));
        std::fs::create_dir(&directory).unwrap();
        let destination = directory.join("out.txt");
        std::fs::write(&destination, b"keep").unwrap();
        // Only the child process changes directory; no global environment or
        // cwd mutation races other tests or touches the owner's desktop.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "document_worker::retirement::tests::backing_relative_save_child", "--nocapture"])
            .current_dir(&directory)
            .env("KWIL_RETIREMENT_RELATIVE_CHILD", &fixture.0)
            .spawn()
            .unwrap();
        eprintln!("retirement relative fixture owned child PID={}", child.id());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                // The only child is ours. Bound reaping independently too.
                let reap = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while child.try_wait().unwrap().is_none() && std::time::Instant::now() < reap {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                panic!("relative fixture child exceeded its deadline");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert!(status.success(), "relative admission child failed: {status}");
        assert_eq!(std::fs::read(fixture.0.join("child-ran")).unwrap(), b"relative path admission");
        assert_eq!(std::fs::read(&destination).unwrap(), b"keep");
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 2);
    }
}
