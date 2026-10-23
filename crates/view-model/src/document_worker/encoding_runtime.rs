//! Document-worker ownership for production decoded opens and encoded saves.
//! Owners remain here after logical snapshots cross to the frontend.
use super::provenance::journal::RecoveryPolicy;
use super::provenance::{OriginRegistry, journal::JournalRecovery};
use super::retirement::{BackingLimits, RetirementReport, WorkerBackings};
use crate::{Document, DocumentLineEnding};
use kwil_core_text::{
    SpillRetirement, SpillRetirementLimits, TextFile,
    decoded_backing::DecodedPreview,
    encoding::{Encoding, LineEnding, SavePlan},
};
use kwil_core_workspace::{SaveRequest, SavedSnapshot};
use kwil_core_workspace::{
    journal::{JournalBase, RecoveredJournal},
    session::Tab,
};
use std::{io, path::PathBuf, sync::atomic::AtomicBool};

pub(super) struct EncodingRuntime {
    backings: WorkerBackings,
    recovered: Vec<Option<JournalRecovery>>,
    spills: Vec<Option<SpillRetirement>>,
    spill_metadata_cap: usize,
    spill_metadata_reserved: usize,
    spill_path_cap: usize,
}

impl EncodingRuntime {
    pub(super) fn new(parent: PathBuf, limits: BackingLimits) -> io::Result<Self> {
        let slots = limits.max_backings;
        let metadata_cap = limits.max_metadata_bytes;
        let spill_path_cap = limits.max_private_path_bytes;
        let backings = WorkerBackings::new(parent, limits)?;
        let extra = slots
            .checked_mul(
                std::mem::size_of::<Option<JournalRecovery>>() + std::mem::size_of::<Option<SpillRetirement>>(),
            )
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Self>()))
            .ok_or_else(|| io::Error::other("recovery registry metadata overflow"))?;
        if backings.metadata_reserved().checked_add(extra).is_none_or(|bytes| bytes > metadata_cap) {
            return Err(io::Error::other("recovery registry metadata admission limit"));
        }
        let mut recovered = Vec::new();
        recovered.try_reserve_exact(slots).map_err(|_| io::Error::other("recovery owner allocation failed"))?;
        recovered.resize_with(slots, || None);
        let mut spills = Vec::new();
        spills.try_reserve_exact(slots).map_err(|_| io::Error::other("spill owner allocation failed"))?;
        spills.resize_with(slots, || None);
        let spill_metadata_cap = metadata_cap - backings.metadata_reserved() - extra;
        Ok(Self { backings, recovered, spills, spill_metadata_cap, spill_metadata_reserved: 0, spill_path_cap })
    }

    /// Pin every ordinary spill before a recovered Buffer reaches the UI.
    /// Origins alone do not own added logical/opaque spill blocks.
    pub(super) fn retain_recovered_spills(
        &mut self,
        snapshot: &kwil_core_text::Snapshot,
        cancel: &AtomicBool,
    ) -> io::Result<()> {
        if snapshot.storage_resources().spilled_blocks == 0 {
            return Ok(());
        }
        let slot = self
            .spills
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "recovered spill capacity retained"))?;
        let available = self.spill_metadata_cap - self.spill_metadata_reserved;
        let lease = snapshot.retain_spills(
            SpillRetirementLimits {
                max_block_references: available / std::mem::size_of::<usize>(),
                max_metadata_bytes: available,
                max_path_bytes: self.spill_path_cap,
            },
            || cancel.load(std::sync::atomic::Ordering::Relaxed),
        )?;
        self.spill_metadata_reserved += lease.metadata_reserved();
        *slot = Some(lease);
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn retained_spill_blocks(&self) -> usize {
        self.spills.iter().flatten().map(SpillRetirement::retained).sum()
    }

    pub(super) fn open(
        &mut self,
        path: PathBuf,
        file: TextFile,
        interpretation: Option<Encoding>,
        cancel: &AtomicBool,
        publish: impl FnOnce(DecodedPreview) -> io::Result<()>,
    ) -> io::Result<Document> {
        self.backings.open(path, file, interpretation, cancel, publish)
    }

    pub(super) fn save(
        &mut self,
        request: SaveRequest,
        plan: SavePlan,
        cancel: &AtomicBool,
    ) -> io::Result<SavedSnapshot> {
        // A conversion changes logical coordinates independently of trimming.
        // Admit it only after the model has a corresponding completion recipe.
        // Reject before the atomic writer can replace the destination.
        if plan.line_ending.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "line-ending conversion requires a logical completion transaction",
            ));
        }
        let mut completed = self.backings.save(request, plan, cancel)?;
        if completed.saved.normalized {
            // The model records a complete logical replacement in this case;
            // its trim-only spool is unnecessary and must retire on the worker.
            completed.saved.trim_deletions.take();
        }
        // Logical normalization is explicitly flagged in SavedSnapshot, even
        // when lengths agree. The model installs it as an undoable transaction.
        Ok(completed.saved)
    }

    pub(super) fn retire(&mut self) -> RetirementReport {
        let mut report = self.backings.retire();
        for slot in &mut self.spills {
            let Some(lease) = slot else { continue };
            let retired = lease.retire();
            report.retired += retired.retired;
            report.live += retired.live;
            report.cleanup_pending += retired.cleanup_pending;
            if report.first_error.is_none() {
                report.first_error = retired.first_error;
            }
            if lease.retained() == 0 {
                self.spill_metadata_reserved -= lease.metadata_reserved();
                *slot = None;
            }
        }
        for slot in &mut self.recovered {
            let Some(recovery) = slot else { continue };
            match recovery.retry_retirement() {
                Ok(status) => {
                    report.retired += status.origins.retired;
                    report.live += status.origins.live_owners;
                    report.cleanup_pending += status.origins.cleanup_paths + usize::from(status.payload_pending);
                    if recovery.retained_origin_resources() == 0 && !recovery.has_pending_payload() {
                        *slot = None;
                    }
                }
                Err(error) => {
                    report.cleanup_pending += 1;
                    if report.first_error.is_none() {
                        report.first_error = Some(error);
                    }
                }
            }
        }
        report
    }

    pub(super) fn recover(
        &mut self,
        recovery: JournalRecovery,
        path: PathBuf,
        tab: &Tab,
        max_origins: usize,
        registry_metadata_bytes: usize,
        cancel: &AtomicBool,
    ) -> io::Result<(Document, RecoveredJournal, JournalBase, OriginRegistry)> {
        let slot = self
            .recovered
            .iter()
            .position(|slot| slot.is_none())
            .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "recovery backing capacity retained"))?;
        // Keep ownership even if opening, replay, adoption or extraction fails.
        self.recovered[slot] = Some(recovery);
        let recovery = self.recovered[slot].as_mut().expect("worker recovery owner");
        let result = (|| {
            let journal = recovery.recover_owned(path, tab.id, cancel)?;
            let summary = recovery.summary().ok_or_else(|| io::Error::other("recovery summary missing"))?;
            let revision = summary.journal.revision;
            let base = summary.base.clone();
            let policy = summary.policy;
            if policy.preferred_ending == LineEnding::Cr {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "CR-only editing requires CR line-index support; recovery retained",
                ));
            }
            let count = recovery.accepted_origin_count();
            if count > max_origins {
                return Err(io::Error::other("recovery origin admission limit"));
            }
            let descriptor_bytes = count
                .checked_mul(std::mem::size_of::<(u64, kwil_core_text::decoded_backing::DecodedSnapshot)>())
                .and_then(|bytes| bytes.checked_mul(2))
                .ok_or_else(|| io::Error::other("origin descriptor admission overflow"))?;
            let directory_budget = registry_metadata_bytes
                .checked_sub(descriptor_bytes)
                .ok_or_else(|| io::Error::other("origin descriptor metadata admission limit"))?;
            let mut descriptors = Vec::new();
            descriptors
                .try_reserve_exact(count)
                .map_err(|_| io::Error::other("origin descriptor allocation failed"))?;
            recovery.for_each_origin(cancel, &mut |id, origin| {
                if descriptors.len() == count {
                    return Err(io::Error::other("recovery origin count changed"));
                }
                descriptors.push((id, origin));
                Ok(())
            })?;
            let origins = OriginRegistry::from_recovered(max_origins, directory_budget, descriptors, cancel)?;
            // Writer IDs are adopted before extraction; their owner remains in
            // this registry after the returned Buffer crosses to the UI.
            let buffer = recovery.take_buffer(cancel)?;
            let mut document = Document::new();
            document.buffer = buffer;
            document.encoding = if policy.encoding == Encoding::UTF8 {
                crate::DocumentEncoding::Utf8
            } else {
                crate::DocumentEncoding::Encoded(policy.encoding)
            };
            document.bom = policy.bom;
            document.line_ending = if policy.preferred_ending == LineEnding::CrLf {
                DocumentLineEnding::CrLf
            } else {
                DocumentLineEnding::Lf
            };
            document.revision = revision;
            document.saved_revision = tab.saved_revision;
            document.next_revision = revision
                .max(tab.saved_revision)
                .checked_add(2)
                .ok_or_else(|| io::Error::other("Recovery revision overflow"))?;
            // Only metadata naming this exact revision as saved can regain a
            // writable backing, and only after exact encoded-byte equality.
            // Logical FF atoms alone cannot establish raw-file equivalence.
            let clean = tab.revision == tab.saved_revision && tab.revision == revision;
            let verified = if clean {
                tab.path.as_ref().and_then(|path| {
                    kwil_core_workspace::open_for_editing(path)
                        .ok()
                        .filter(|file| {
                            matches_raw(&document.snapshot(), document.save_plan(), file, cancel).unwrap_or(false)
                        })
                        .map(|file| (path.clone(), file))
                })
            } else {
                None
            };
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            if let Some((path, file)) = verified {
                document.path = Some(path);
                document.original = Some(file);
                document.saved_revision = revision;
            } else if !document.is_dirty() {
                super::mark_recovered_unsaved(&mut document)?;
            }
            Ok((document, journal, base, origins))
        })();
        let result = result.and_then(|(document, journal, base, origins)| {
            self.retain_recovered_spills(&document.snapshot(), cancel)?;
            Ok((document, journal, base, origins))
        });
        if result.is_err() {
            // Drops replay roots before trying cleanup; pins from a successful
            // returned Buffer and writer directory keep their owners live.
            let _ = self.recovered[slot].as_mut().expect("worker recovery owner").abandon();
        }
        result
    }
}

fn matches_raw(
    snapshot: &kwil_core_text::Snapshot,
    plan: SavePlan,
    file: &TextFile,
    cancel: &AtomicBool,
) -> io::Result<bool> {
    use std::io::Write;
    if file.check()? != kwil_core_text::FileState::Unchanged {
        return Ok(false);
    }
    struct Compare<'a> {
        file: &'a TextFile,
        at: usize,
        equal: bool,
    }
    impl Write for Compare<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let raw = self.file.bytes().map_err(io::Error::other)?;
            let end = self.at.checked_add(bytes.len()).ok_or_else(|| io::Error::other("raw comparison overflow"))?;
            self.equal &= raw.get(self.at..end) == Some(bytes);
            self.at = end;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut compare = Compare { file, at: 0, equal: true };
    kwil_core_text::encoding::write_snapshot_cancellable(snapshot, plan, &mut compare, || {
        cancel.load(std::sync::atomic::Ordering::Relaxed)
    })
    .map_err(io::Error::other)?;
    Ok(compare.equal && compare.at as u64 == file.len() && file.check()? == kwil_core_text::FileState::Unchanged)
}

pub(crate) fn document_policy(document: &Document) -> RecoveryPolicy {
    RecoveryPolicy {
        encoding: document.encoding.encoding(),
        bom: document.bom,
        preferred_ending: match document.line_ending {
            DocumentLineEnding::Lf => LineEnding::Lf,
            DocumentLineEnding::CrLf => LineEnding::CrLf,
        },
    }
}
