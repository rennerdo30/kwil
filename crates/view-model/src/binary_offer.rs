//! Scalar binary-open protocol. Sources and exact paths stay on the document
//! lane; neither an offer nor a choice lends a mapping to the UI.
use kwil_core_text::{FileKind, FileState, TextFile};
use kwil_core_workspace::{
    compressed::DetectedFormat,
    session::{self, DocumentId},
};
use std::{io, path::PathBuf};

/// The generation names one exact path in the workspace's open flight AND in
/// the worker's pending slot. It is never a pathname hash or a revision alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticket {
    pub(crate) document: DocumentId,
    pub(crate) revision: u64,
    pub(crate) generation: u64,
}
impl Ticket {
    pub fn document(self) -> DocumentId {
        self.document
    }
    pub fn revision(self) -> u64 {
        self.revision
    }
    pub fn generation(self) -> u64 {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Hex,
    Text,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offer {
    pub ticket: Ticket,
    pub len: u64,
}

pub(super) struct Pending {
    pub path: PathBuf,
    pub file: TextFile,
}

pub(super) enum Detection {
    Offer(Offer),
    Text(Pending),
}

/// Fixed slots admit the owner before opening/sniffing. There is at most one
/// pending physical source per tab; refusal never evicts an earlier ticket.
pub(super) struct Runtime {
    slots: [Option<(Ticket, Pending)>; session::MAX_TABS],
}
impl Runtime {
    pub fn new() -> Self {
        Self { slots: std::array::from_fn(|_| None) }
    }

    pub fn detect(&mut self, ticket: Ticket, path: PathBuf, file: Option<TextFile>) -> io::Result<Detection> {
        if ticket.generation == 0
            || ticket.revision != 0
            || !path.is_absolute()
            || path.capacity() > session::MAX_PATH_BYTES
            || path.as_os_str().as_encoded_bytes().len() > session::MAX_PATH_BYTES
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "binary open identity/path admission"));
        }
        if self.slots.iter().flatten().any(|(old, _)| old.document == ticket.document) {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "previous binary offer retained"));
        }
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "binary offer capacity retained"))?;
        let file = match file {
            Some(file) => file,
            None => kwil_core_workspace::open_for_editing(&path)?,
        };
        if file.path() != path || file.path().as_os_str().as_encoded_bytes().len() > session::MAX_PATH_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "binary source/path mismatch"));
        }
        // This prefix check is bounded and shares the production compression
        // detector. Gzip/unsupported containers always take the existing route.
        let format = super::retirement::source_format(&file)?;
        let binary = format == DetectedFormat::Raw && file.kind().map_err(io::Error::other)? == FileKind::Binary;
        let pending = Pending { path, file };
        if binary {
            let offer = Offer { ticket, len: pending.file.len() };
            *slot = Some((ticket, pending));
            Ok(Detection::Offer(offer))
        } else {
            Ok(Detection::Text(pending))
        }
    }

    pub fn take(&mut self, ticket: Ticket) -> io::Result<Pending> {
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|(old, _)| *old == ticket))
            .ok_or_else(|| io::Error::new(io::ErrorKind::Interrupted, "stale binary offer ticket"))?;
        Ok(slot.take().unwrap().1)
    }

    pub fn pending(&self, ticket: Ticket) -> io::Result<&Pending> {
        self.slots
            .iter()
            .flatten()
            .find(|(old, _)| *old == ticket)
            .map(|(_, pending)| pending)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Interrupted, "stale binary offer ticket"))
    }

    pub fn unchanged(pending: &Pending) -> io::Result<()> {
        if pending.file.path() != pending.path || pending.file.check()? != FileState::Unchanged {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "binary offer source changed; reopen required"));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "binary_offer_tests.rs"]
mod tests;
