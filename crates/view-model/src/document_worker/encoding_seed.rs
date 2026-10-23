//! Transfer a completed seed's origin token back to the sole document worker.
use super::provenance::{AdmissionToken, CapsuleReader};
use std::{
    io::{self, Read},
    sync::mpsc::{self, Receiver, SyncSender},
};

pub(super) struct SeedSource {
    reader: Option<CapsuleReader>,
    completion: Option<SyncSender<io::Result<AdmissionToken>>>,
}

impl SeedSource {
    pub(super) fn new(reader: CapsuleReader) -> (Self, Receiver<io::Result<AdmissionToken>>) {
        let (tx, rx) = mpsc::sync_channel(1);
        (Self { reader: Some(reader), completion: Some(tx) }, rx)
    }

    fn report(&mut self, result: io::Result<AdmissionToken>) {
        if let Some(tx) = self.completion.take() {
            // Never wait on the document worker, which may itself be waiting
            // for journal admission. There is exactly one terminal message.
            let _ = tx.try_send(result);
        }
    }
}

impl Read for SeedSource {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let Some(reader) = &mut self.reader else {
            return Ok(0);
        };
        match reader.read(output) {
            Ok(0) => {
                let result = self.reader.take().expect("seed reader owned").finish();
                match result {
                    Ok(token) => {
                        self.report(Ok(token));
                        Ok(0)
                    }
                    Err(error) => {
                        let kind = error.kind();
                        let message = error.to_string();
                        self.report(Err(error));
                        Err(io::Error::new(kind, message))
                    }
                }
            }
            Ok(count) => Ok(count),
            Err(error) => {
                let kind = error.kind();
                let message = error.to_string();
                self.reader.take();
                self.report(Err(error));
                Err(io::Error::new(kind, message))
            }
        }
    }
}

impl Drop for SeedSource {
    fn drop(&mut self) {
        self.report(Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "journal seed stopped before complete origin serialization",
        )));
    }
}
