//! Worker-only decoded opening. The first-screen callback precedes creation of
//! any full-file private backing or logical index.
use crate::Document;
use kwil_core_text::TextFile;
use kwil_core_text::decoded_backing::{BackingError, DecodedBacking, DecodedPreview, Interpretation, decode_preview};
use kwil_core_text::encoding::{EncodedFile, Encoding};
use std::{
    io,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

#[must_use = "retain the backing owner on the worker until document snapshots retire"]
pub struct PreparedDecodedDocument {
    pub document: Document,
    pub backing: DecodedBacking,
}

/// `publish` admits a bounded, read-only preview to the existing completion
/// queue. An admission error stops preparation; it never enables raw editing.
/// The caller owns the worker lane, cancellation flag and retirement registry.
pub fn prepare_decoded_document(
    path: PathBuf,
    file: TextFile,
    interpretation: Option<Encoding>,
    private_parent: &Path,
    cancel: &AtomicBool,
    publish: impl FnOnce(DecodedPreview) -> io::Result<()>,
) -> io::Result<PreparedDecodedDocument> {
    prepare_with_index_check(path, file, interpretation, private_parent, cancel, publish, || false)
}

pub(super) fn prepare_with_index_check(
    path: PathBuf,
    file: TextFile,
    interpretation: Option<Encoding>,
    private_parent: &Path,
    cancel: &AtomicBool,
    publish: impl FnOnce(DecodedPreview) -> io::Result<()>,
    mut index_cancelled: impl FnMut() -> bool,
) -> io::Result<PreparedDecodedDocument> {
    let encoded = match interpretation {
        Some(encoding) => EncodedFile::new_with_encoding(file.clone(), encoding),
        None => EncodedFile::new(file.clone()),
    }
    .map_err(io::Error::other)?;
    let detection = encoded.detection();
    let interpretation = Interpretation::new(detection.encoding, detection.bom_len).map_err(io::Error::other)?;
    let preview = decode_preview(&file, interpretation, cancel).map_err(BackingError::into_io_error)?;
    publish(preview)?;
    let backing =
        DecodedBacking::prepare(&file, interpretation, private_parent, cancel).map_err(BackingError::into_io_error)?;
    match Document::from_decoded_file_cancellable(path, file, backing.snapshot(), || {
        cancel.load(std::sync::atomic::Ordering::Relaxed) || index_cancelled()
    }) {
        Ok(document) => Ok(PreparedDecodedDocument { document, backing }),
        Err(error) => match backing.try_retire() {
            Ok(()) => Err(error),
            Err(retirement) => Err(retirement.with_cause(error)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn pending_retirement_decoded_index_cancellation_keeps_cleanup_resource() {
        let fixture = Fixture::new();
        let (path, file) = fixture.file(b"text");
        let error = prepare_with_index_check(
            path,
            file,
            None,
            &fixture.0,
            &AtomicBool::new(false),
            |_| Ok(()),
            || {
                let directory = std::fs::read_dir(&fixture.0)
                    .unwrap()
                    .map(Result::unwrap)
                    .find(|entry| entry.file_type().unwrap().is_dir())
                    .unwrap()
                    .path();
                std::fs::write(directory.join("blocker"), b"test blocker").unwrap();
                true
            },
        )
        .err()
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        let Some(kwil_core_text::decoded_backing::RetirementError::Cleanup { paths, .. }) =
            kwil_core_text::decoded_backing::RetirementError::take_from(&error)
        else {
            panic!("cancelled logical indexing must retain failed cleanup");
        };
        assert!(paths.cleanup().is_err());
        std::fs::remove_file(paths.directory.join("blocker")).unwrap();
        paths.cleanup().unwrap();
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 1);
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "kwil-decoded-open-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn file(&self, bytes: &[u8]) -> (PathBuf, TextFile) {
            let path = self.0.join("source");
            std::fs::write(&path, bytes).unwrap();
            let file = TextFile::open(&path).unwrap();
            (path, file)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn decoded_open_publishes_first_screen_before_private_preparation() {
        let fixture = Fixture::new();
        let published = std::cell::Cell::new(false);
        let raw = b"\xff\xfeA\0\0\xd8B\0";
        let (path, file) = fixture.file(raw);
        let prepared = prepare_decoded_document(path, file, None, &fixture.0, &AtomicBool::new(false), |preview| {
            published.set(true);
            assert_eq!(preview.text, b"A\\x00\\xD8B");
            assert_eq!(std::fs::read_dir(&fixture.0)?.count(), 1, "full private files must follow the first screen");
            Ok(())
        })
        .unwrap();
        assert!(published.get());
        assert_eq!(prepared.document.snapshot().len(), 3);
        let mut saved = Vec::new();
        kwil_core_text::encoding::write_snapshot(
            &prepared.document.snapshot(),
            prepared.document.save_plan(),
            &mut saved,
        )
        .unwrap();
        assert_eq!(saved, raw);
        drop(prepared.document);
        assert!(prepared.backing.try_retire().is_ok());
    }

    #[test]
    fn decoded_open_cancellation_and_rejected_preview_leave_no_full_backing() {
        let fixture = Fixture::new();
        let (path, file) = fixture.file(b"text");
        let cancel = AtomicBool::new(false);
        let result = prepare_decoded_document(path.clone(), file.clone(), None, &fixture.0, &cancel, |_| {
            cancel.store(true, Ordering::Relaxed);
            Ok(())
        });
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::Interrupted));
        let result = prepare_decoded_document(path, file, None, &fixture.0, &AtomicBool::new(false), |_| {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "preview queue full"))
        });
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::WouldBlock));
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 1);
    }

    #[test]
    fn decoded_open_explicit_interpretation_recovers_ambiguous_bom() {
        let fixture = Fixture::new();
        let (path, file) = fixture.file(b"\xff\xfe\0\0");
        assert!(
            prepare_decoded_document(
                path.clone(),
                file.clone(),
                None,
                &fixture.0,
                &AtomicBool::new(false),
                |_| panic!("unsupported auto encoding must not paint editable data")
            )
            .is_err()
        );
        let prepared = prepare_decoded_document(
            path,
            file,
            Some(Encoding::UTF16_LE),
            &fixture.0,
            &AtomicBool::new(false),
            |preview| {
                assert_eq!(preview.text, [0]);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(prepared.document.snapshot().len(), 1);
        drop(prepared.document);
        assert!(prepared.backing.try_retire().is_ok());
    }

    #[test]
    fn decoded_open_index_cancellation_retires_prepared_backing() {
        let fixture = Fixture::new();
        let (path, file) = fixture.file(&vec![b'a'; 128 << 10]);
        let checks = std::cell::Cell::new(0);
        let result = prepare_with_index_check(
            path,
            file,
            None,
            &fixture.0,
            &AtomicBool::new(false),
            |_| Ok(()),
            || {
                let next = checks.get() + 1;
                checks.set(next);
                assert!(
                    std::fs::read_dir(&fixture.0).unwrap().count() > 1,
                    "barrier must occur after backing preparation"
                );
                next >= 2
            },
        );
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::Interrupted));
        assert_eq!(checks.get(), 2, "cancelled index must stop at its first rejection");
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 1, "all private backing files must retire");
    }
}
