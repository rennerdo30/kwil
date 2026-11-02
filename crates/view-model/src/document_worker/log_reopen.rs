//! Follow refresh preparation stays on the existing document lane.
use kwil_core_text::{
    FileState, TextFile,
    encoding::{EncodedFile, Encoding},
};
use std::{io, path::Path};

pub(super) fn follow_source(path: &Path, old: Option<TextFile>) -> io::Result<TextFile> {
    if let Some(old) = old {
        if old.path() != path {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "follow source/path mismatch"));
        }
        match old.check()? {
            FileState::Unchanged if old.index().is_some() => return Ok(old),
            FileState::Grown { .. } => {
                if let Ok(grown) = old.reopen_grown() {
                    return Ok(grown);
                }
            }
            _ => (),
        }
    }
    // No resident copy of the old prefix, including for small files. Source
    // validation still checks identity/length/mtime around every bounded read.
    TextFile::open_saved_version(path).map_err(io::Error::other)
}

pub(super) fn plain_utf8_follow(file: &TextFile) -> io::Result<bool> {
    if super::retirement::source_format(file)? != kwil_core_workspace::compressed::DetectedFormat::Raw {
        return Ok(false);
    }
    let encoded = EncodedFile::new(file.clone()).map_err(io::Error::other)?;
    let detection = encoded.detection();
    Ok(detection.encoding == Encoding::UTF8 && detection.bom_len == 0)
}

#[cfg(test)]
mod tests {
    use super::super::{
        DocumentRequest, DocumentResult, DocumentWorker,
        binary_offer::Ticket,
        tests::{Scratch, prepare_test_worker, wait_stopped},
    };
    use super::*;
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    #[test]
    fn kw41_follow_reuses_unchanged_index() {
        let root = Scratch::new();
        let path = root.0.join("app.log");
        std::fs::write(&path, b"INFO row\n").unwrap();
        let file = TextFile::open_saved_version(&path).unwrap();
        file.build_index(1).unwrap();
        let next = follow_source(&path, Some(file.clone())).unwrap();
        assert!(next.same_source(&file));
    }

    #[test]
    fn kw41_follow_keeps_bom_and_legacy_decoded_backings() {
        for raw in [b"\xef\xbb\xbfINFO row\n".as_slice(), b"\xff\xfeI\0N\0F\0O\0 \0r\0o\0w\0\n\0"] {
            let root = Scratch::new();
            let path = root.0.join("app.log");
            std::fs::write(&path, raw).unwrap();
            let (tx, rx) = mpsc::channel();
            let worker = DocumentWorker::spawn(move |result| {
                let _ = tx.send(result);
            })
            .unwrap();
            prepare_test_worker(&worker, &rx, &root.0);
            worker
                .try_submit(DocumentRequest::OpenChecked {
                    ticket: Ticket {
                        document: kwil_core_workspace::session::DocumentId(1),
                        revision: 0,
                        generation: 1,
                        follow: true,
                    },
                    path,
                    file: None,
                })
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let document = loop {
                if let DocumentResult::CheckedOpened { result, .. } =
                    rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap()
                {
                    break result.unwrap().unwrap();
                }
            };
            let mut bytes = Vec::new();
            document.snapshot().write_to(&mut bytes).unwrap();
            assert_eq!(bytes, b"INFO row\n");
            assert!(document.save_plan().bom);
            assert!(document.original.as_ref().unwrap().index().is_none(), "fallback uses decoded logical indexing");
            drop(document);
            drop(worker);
            wait_stopped(&rx);
        }
    }
}
