//! Worker-only folder replacement. The manifest and edit recipes are disk-backed;
//! frontends retain one page, scalar identities and explicit exclusion indices.
use super::*;
use kwil_core_text::{
    Buffer, TextFile,
    decoded_backing::{DecodedBacking, Interpretation},
    encoding::{EncodedFile, SavePlan},
};
use std::{
    hash::Hasher,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct OpenDocument {
    pub path: PathBuf,
    pub key: SearchKey,
    pub epoch: u64,
    /// None reserves loading, raw or busy tabs against closed-file writes.
    pub snapshot: Option<Snapshot>,
}
#[derive(Clone, Debug)]
pub struct Plan {
    manifest: SpoolRecipe,
    search: FileSearch,
    template: Vec<u8>,
    pub matches: u64,
    page_index: u64,
    page_offset: u64,
}
impl Plan {
    pub fn len(&self) -> u64 {
        self.manifest.len()
    }
    pub fn is_empty(&self) -> bool {
        self.manifest.is_empty()
    }
}
pub enum Task {
    Preview { search: FileSearch, template: Vec<u8>, open: Vec<OpenDocument> },
    Page { plan: Plan, page: Page },
    Apply { plan: Plan, index: u64, cursor: u64, open: Vec<OpenDocument> },
}
#[derive(Debug)]
pub enum Update {
    /// Admit on the existing document/save lane, refreshing every reservation.
    NeedsClosedApply {
        plan: Plan,
        index: u64,
        offset: u64,
        next_offset: u64,
    },
    Preview {
        plan: Plan,
        first: u64,
        rows: Vec<FileEvent>,
    },
    Applied {
        source_epoch: u64,
        next_offset: u64,
        index: u64,
        path: PathBuf,
        result: Result<Option<ReplaceRecipe>, String>,
    },
}

pub(super) fn validate(task: &Task) -> Result<(), &'static str> {
    let plan = match task {
        Task::Preview { search, template, open } => {
            if template.len() > INPUT_BYTES
                || open.len() > REPLACE_EDITS
                || !(1 << 16..=8 << 20).contains(&search.heap_limit)
                || open.iter().any(|o| o.path.as_os_str().as_encoded_bytes().len() > 32768)
            {
                return Err("folder replacement input exceeds cap");
            }
            super::validate(&SearchRequest {
                key: SearchKey { document_id: 0, revision: 0, generation: 0 },
                task: SearchTask::Files(search.clone()),
            })?;
            return Ok(());
        }
        Task::Page { plan, page } => {
            if page.len == 0 || page.len > FILE_PAGE {
                return Err("folder preview page exceeds cap");
            }
            plan
        }
        Task::Apply { plan, index, open, .. } => {
            if *index >= plan.len() || open.len() > REPLACE_EDITS {
                return Err("folder apply exceeds cap");
            }
            plan
        }
    };
    if plan.template.len() > INPUT_BYTES {
        return Err("folder template exceeds cap");
    }
    Ok(())
}
fn cancelled(cancel: &Cancellation) -> io::Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(io::Error::new(io::ErrorKind::Interrupted, "Folder replacement cancelled"))
    } else {
        Ok(())
    }
}
fn fingerprint(path: &Path, cancel: &Cancellation) -> io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let before = file.metadata()?;
    let mut hash = std::hash::DefaultHasher::new();
    let mut bytes = [0; 64 << 10];
    loop {
        cancelled(cancel)?;
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        hash.write(&bytes[..n]);
    }
    let after = file.metadata()?;
    if before.len() != after.len() || before.modified()? != after.modified()? {
        return Err(io::Error::other("File changed during search"));
    }
    let modified = after.modified()?.duration_since(std::time::UNIX_EPOCH).map_err(io::Error::other)?.as_nanos();
    let mut result = after.len().to_le_bytes().to_vec();
    result.extend(modified.to_le_bytes());
    result.extend(hash.finish().to_le_bytes());
    Ok(result)
}
fn resolve_open(open: &[OpenDocument]) -> Vec<Option<kwil_core_workspace::SaveDestination>> {
    open.iter().map(|o| kwil_core_workspace::resolve_save_destination(&o.path).ok()).collect()
}
fn opened<'a>(
    open: &'a [OpenDocument],
    resolved: &[Option<kwil_core_workspace::SaveDestination>],
    path: &Path,
) -> Option<&'a OpenDocument> {
    if open.is_empty() {
        return None;
    }
    if let Some(document) = open.iter().find(|o| o.path == path) {
        return Some(document);
    }
    let candidate = kwil_core_workspace::resolve_save_destination(path).ok()?;
    open.iter().zip(resolved).find_map(|(o, destination)| {
        destination.as_ref().is_some_and(|destination| destination.same_existing_file(&candidate)).then_some(o)
    })
}

fn identity(open: Option<&OpenDocument>) -> Vec<u8> {
    let mut bytes = Vec::new();
    for value in open.map_or([0; 3], |o| [o.key.document_id, o.key.revision, o.epoch]) {
        bytes.extend(value.to_le_bytes());
    }
    bytes
}
// Preserve malformed bytes and legacy encoding aliases through the existing
// decoded backing and encoded save; never create a whole-file decoded Vec.
fn with_disk<T>(
    path: &Path,
    cancel: &Cancellation,
    f: impl FnOnce(Buffer, TextFile, SavePlan) -> io::Result<T>,
) -> io::Result<T> {
    cancelled(cancel)?;
    let file = TextFile::open(path).map_err(io::Error::other)?;
    let detection = EncodedFile::new(file.clone()).map_err(io::Error::other)?.detection();
    if file.kind().map_err(io::Error::other)? == kwil_core_text::FileKind::Binary {
        return Err(io::Error::other("Binary file skipped"));
    }
    let interpretation = Interpretation::new(detection.encoding, detection.bom_len).map_err(io::Error::other)?;
    let backing =
        DecodedBacking::prepare(&file, interpretation, &std::env::temp_dir(), cancel).map_err(|e| e.into_io_error())?;
    let result = (|| {
        let buffer = Buffer::from_decoded_cancellable(backing.snapshot(), || cancel.load(Ordering::Acquire))?;
        f(buffer, file, SavePlan { encoding: detection.encoding, bom: detection.bom_len != 0, line_ending: None })
    })();
    backing.try_retire().map_err(|e| e.with_cause(io::Error::other("Folder decoded backing retirement failed")))?;
    result
}
fn recipe(
    snapshot: &Snapshot,
    search: &FileSearch,
    template: &[u8],
    key: SearchKey,
    cancel: &Cancellation,
    spools: &mut SpoolOwner,
) -> io::Result<ReplaceRecipe> {
    let expression = if search.literal { regex::escape(&search.expression) } else { search.expression.clone() };
    let expression = if search.options.whole_word { format!("\\b(?:{expression})\\b") } else { expression };
    let regex = regex::bytes::RegexBuilder::new(&expression)
        .case_insensitive(search.options.case_insensitive)
        .multi_line(true)
        .crlf(true)
        .build()
        .map_err(io::Error::other)?;
    let slot = spools.begin()?;
    let result = (|| {
        let mut line = Vec::new();
        let mut offset = 0_u64;
        let mut scan = |line: &[u8], offset: u64| -> io::Result<()> {
            for capture in regex.captures_iter(line) {
                cancelled(cancel)?;
                let hit = capture.get(0).unwrap();
                let end = line.len() - usize::from(line.last() == Some(&b'\n'));
                if hit.start() > end {
                    continue;
                }
                if hit.end() > end {
                    return Err(io::Error::other("Folder replacement cannot consume a line separator"));
                }
                let mut new_bytes = Vec::new();
                if search.literal {
                    new_bytes.extend(template);
                } else {
                    new_bytes = crate::search::expand_captures(&capture, template)?;
                }
                if new_bytes.len() > REPLACE_BYTES {
                    return Err(io::Error::other("Replacement expansion exceeds 8 MiB"));
                }
                spools.append(slot, Edit { offset: offset + hit.start() as u64, len: hit.len() as u64, new_bytes })?;
            }
            Ok(())
        };
        for chunk in snapshot.chunks(0..snapshot.len())? {
            cancelled(cancel)?;
            for part in chunk.map_err(io::Error::other)?.split_inclusive(|b| *b == b'\n') {
                if line.len().saturating_add(part.len()) > search.heap_limit {
                    return Err(io::Error::other("Line exceeds folder search memory limit; file skipped"));
                }
                line.extend(part);
                if line.last() == Some(&b'\n') {
                    scan(&line, offset)?;
                    offset += line.len() as u64;
                    line.clear();
                }
            }
        }
        if !line.is_empty() || offset == 0 {
            scan(&line, offset)?;
        }
        cancelled(cancel)?;
        Ok(ReplaceRecipe { key, edits: ReplacementEdits::Spool(spools.finish(slot)?) })
    })();
    if result.is_err() {
        spools.abandon(slot);
    }
    result
}
fn manifest_record(plan: &Plan, index: u64, mut cursor: u64, cancel: &Cancellation) -> io::Result<(Edit, u64, u64)> {
    loop {
        cancelled(cancel)?;
        let offset = cursor;
        let (record, next) = plan.manifest.record_at(cursor)?;
        cursor = next;
        if record.offset == index {
            return Ok((record, offset, next));
        }
        if record.offset > index {
            return Err(io::Error::other("Manifest cursor passed its requested file"));
        }
    }
}
fn page(plan: &Plan, page: Page, cancel: &Cancellation) -> io::Result<(Plan, Vec<FileEvent>)> {
    let mut plan = plan.clone();
    let mut index = plan.page_index;
    let mut cursor = plan.page_offset;
    while index < page.first {
        cancelled(cancel)?;
        cursor = plan.manifest.record_at(cursor)?.1;
        index += 1;
    }
    while index > page.first {
        cancelled(cancel)?;
        cursor = plan.manifest.record_before(cursor)?.1;
        index -= 1;
    }
    plan.page_index = index;
    plan.page_offset = cursor;
    let mut rows = Vec::new();
    for _ in 0..page.len.min(plan.len().saturating_sub(index) as usize) {
        cancelled(cancel)?;
        let (record, next) = plan.manifest.record_at(cursor)?;
        cursor = next;
        rows.push(decode_file_event(&record.new_bytes)?);
    }
    Ok((plan, rows))
}

pub(super) fn run(
    key: SearchKey,
    task: &Task,
    cancel: &Cancellation,
    emit: &impl Fn(SearchUpdate) -> bool,
    spools: &mut SpoolOwner,
) -> io::Result<()> {
    match task {
        Task::Preview { search, template, open } => {
            let resolved = resolve_open(open);
            let include = crate::files::globs(&search.include)?;
            let exclude = crate::files::globs(&search.exclude)?;
            let slot = spools.begin()?;
            let mut count = 0;
            let mut matches = 0;
            let mut walk = ignore::WalkBuilder::new(&search.root);
            walk.require_git(false).follow_links(false);
            let result = (|| {
                for entry in walk.build() {
                    cancelled(cancel)?;
                    let entry = entry.map_err(io::Error::other)?;
                    if !entry.file_type().is_some_and(|t| t.is_file()) {
                        continue;
                    }
                    let path = entry.path();
                    let relative = path.strip_prefix(&search.root).unwrap_or(path);
                    if (!search.include.is_empty() && !include.is_match(relative)) || exclude.is_match(relative) {
                        continue;
                    }
                    let row = (|| -> io::Result<Option<FileEvent>> {
                        let mut stamp = fingerprint(path, cancel)?;
                        let opened = opened(open, &resolved, path);
                        let plan = if let Some(opened) = opened {
                            recipe(
                                opened
                                    .snapshot
                                    .as_ref()
                                    .ok_or_else(|| io::Error::other("Open document is busy or not text"))?,
                                search,
                                template,
                                opened.key,
                                cancel,
                                spools,
                            )?
                        } else {
                            with_disk(path, cancel, |buffer, _, _| {
                                recipe(&buffer.snapshot(), search, template, key, cancel, spools)
                            })?
                        };
                        let n = plan.edits.len();
                        drop(plan);
                        spools.retire()?;
                        if stamp != fingerprint(path, cancel)? {
                            return Err(io::Error::other("File changed during search"));
                        }
                        if n == 0 {
                            return Ok(None);
                        }
                        stamp.extend(identity(opened));
                        matches += n;
                        Ok(Some(FileEvent::Hit(crate::FileHit {
                            path: path.to_owned(),
                            line_number: n,
                            line_offset: count,
                            line: stamp,
                            line_truncated: false,
                            match_ranges: vec![],
                            ranges_truncated: false,
                        })))
                    })();
                    let row = match row {
                        Ok(Some(row)) => row,
                        Ok(None) => continue,
                        Err(error) => {
                            cancelled(cancel)?;
                            FileEvent::Error { path: path.to_owned(), message: error.to_string() }
                        }
                    };
                    spools.append(slot, Edit { offset: count, len: 0, new_bytes: encode_file_event(&row)? })?;
                    count += 1;
                    if count % FILE_PAGE as u64 == 0 {
                        emit(SearchUpdate::Progress(Progress { scanned: count, total: 0, count: matches }));
                    }
                }
                let plan = Plan {
                    manifest: spools.finish(slot)?,
                    search: search.clone(),
                    template: template.clone(),
                    matches,
                    page_index: 0,
                    page_offset: 0,
                };
                let (plan, rows) = page(&plan, Page { first: 0, len: FILE_PAGE }, cancel)?;
                emit(SearchUpdate::Folder(Update::Preview { plan, first: 0, rows }));
                Ok(())
            })();
            if result.is_err() {
                spools.abandon(slot);
            }
            result
        }
        Task::Page { plan, page: p } => {
            let (plan, rows) = page(plan, *p, cancel)?;
            emit(SearchUpdate::Folder(Update::Preview { plan, first: p.first, rows }));
            Ok(())
        }
        Task::Apply { plan, index, cursor, open } => {
            let (record, offset, next_offset) = manifest_record(plan, *index, *cursor, cancel)?;
            let row = decode_file_event(&record.new_bytes)?;
            let closed = std::cell::Cell::new(false);
            let source_epoch = std::cell::Cell::new(0);
            let FileEvent::Hit(hit) = row else {
                emit(SearchUpdate::Folder(Update::Applied {
                    source_epoch: 0,
                    next_offset,
                    index: *index,
                    path: PathBuf::new(),
                    result: Err("Preview error; file skipped".into()),
                }));
                return Ok(());
            };
            let result = (|| {
                let opened = opened(open, &resolve_open(open), &hit.path);
                if hit.line[..32] != fingerprint(&hit.path, cancel)? || hit.line[32..] != identity(opened) {
                    return Err(io::Error::other("File or open document changed since preview; skipped"));
                }
                if let Some(opened) = opened {
                    source_epoch.set(opened.epoch);
                    let mut k = opened.key;
                    k.generation = key.generation;
                    return recipe(
                        opened.snapshot.as_ref().ok_or_else(|| io::Error::other("Open document is busy; skipped"))?,
                        &plan.search,
                        &plan.template,
                        k,
                        cancel,
                        spools,
                    )
                    .map(Some);
                }
                closed.set(true);
                emit(SearchUpdate::Folder(Update::NeedsClosedApply {
                    plan: plan.clone(),
                    index: *index,
                    offset,
                    next_offset,
                }));
                Ok(None)
            })();
            cancelled(cancel)?;
            if closed.get() {
                return Ok(());
            }
            emit(SearchUpdate::Folder(Update::Applied {
                source_epoch: source_epoch.get(),
                next_offset,
                index: *index,
                path: hit.path.clone(),
                result: result.map_err(|e| format!("{}: {e}", hit.path.display())),
            }));
            Ok(())
        }
    }
}

/// Execute only on the serialized document/save lane. The caller must reserve
/// pending opens/saves in that lane until this returns, and supply every current
/// open/pending native destination. This is deliberately not a SearchTask write.
pub fn apply_closed(plan: &Plan, index: u64, reserved: &[PathBuf], cancel: &Cancellation) -> io::Result<()> {
    apply_closed_at(plan, index, 0, reserved, cancel)
}
/// Resume from the exact manifest offset delivered to the document worker.
pub fn apply_closed_at(
    plan: &Plan,
    index: u64,
    cursor: u64,
    reserved: &[PathBuf],
    cancel: &Cancellation,
) -> io::Result<()> {
    let (record, _, _) = manifest_record(plan, index, cursor, cancel)?;
    let FileEvent::Hit(hit) = decode_file_event(&record.new_bytes)? else {
        return Err(io::Error::other("Preview error; skipped"));
    };
    if hit.line.len() != 56 || hit.line[32..] != [0; 24] || hit.line[..32] != fingerprint(&hit.path, cancel)? {
        return Err(io::Error::other(format!(
            "{}: file changed since preview or was open; skipped",
            hit.path.display()
        )));
    }
    validate_closed_destination(&hit.path, reserved)?;
    let mut spools = SpoolOwner::new(512 << 20)?;
    let key = SearchKey { document_id: 0, revision: 0, generation: 0 };
    let result = with_disk(&hit.path, cancel, |mut buffer, file, save_plan| {
        let edits = recipe(&buffer.snapshot(), &plan.search, &plan.template, key, cancel, &mut spools)?;
        buffer.set_undo_memory_limit(0);
        buffer.begin();
        for edit in edits.edits.iter().rev() {
            cancelled(cancel)?;
            let edit = edit?;
            buffer.delete(edit.offset..edit.offset + edit.len)?;
            buffer.insert(edit.offset, &edit.new_bytes)?;
        }
        buffer.commit();
        drop(edits);
        spools.retire()?;
        validate_closed_destination(&hit.path, reserved)?;
        if hit.line[..32] != fingerprint(&hit.path, cancel)? {
            return Err(io::Error::other("File changed since preview; skipped"));
        }
        cancelled(cancel)?;
        let saved = kwil_core_workspace::save_encoded_snapshot(
            kwil_core_workspace::SaveRequest {
                backup: false,
                compression: kwil_core_workspace::compressed::CompressionFormat::Raw,
                snapshot: buffer.snapshot(),
                path: hit.path.clone(),
                original: Some(file),
                revision: 1,
                trim_trailing_whitespace: false,
            },
            save_plan,
            &std::env::temp_dir(),
            cancel,
        )?;
        let warnings = saved.saved.warnings.join("; ");
        drop(saved.saved);
        saved.backing.try_retire().map_err(|e| e.with_cause(io::Error::other("Saved backing retirement failed")))?;
        if !warnings.is_empty() {
            return Err(io::Error::other(format!("Saved with warning: {warnings}")));
        }
        Ok(None)
    });
    spools.retire()?;
    result.map(|_: Option<ReplaceRecipe>| ())
}
fn validate_closed_destination(path: &Path, reserved: &[PathBuf]) -> io::Result<()> {
    let destination = kwil_core_workspace::resolve_save_destination(path)?;
    for path in reserved {
        if destination.equivalent(&kwil_core_workspace::resolve_save_destination(path)?) {
            return Err(io::Error::other(format!(
                "{}: file is open or reserved by a pending save; skipped",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, sync::atomic::AtomicU64};
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "kwil-folder-replace-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn file(&self, bytes: &[u8]) -> PathBuf {
            let path = self.0.join("input.txt");
            std::fs::write(&path, bytes).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn key() -> SearchKey {
        SearchKey { document_id: 0, revision: 0, generation: 1 }
    }
    fn token() -> Cancellation {
        Arc::new(AtomicBool::new(false))
    }
    fn preview(
        fixture: &Fixture,
        expression: &str,
        template: &str,
        spools: &mut SpoolOwner,
        open: Vec<OpenDocument>,
    ) -> Plan {
        let result = RefCell::new(None);
        run(
            key(),
            &Task::Preview {
                search: FileSearch::new(fixture.0.clone(), expression.into()),
                template: template.as_bytes().to_vec(),
                open,
            },
            &token(),
            &|event| {
                if let SearchUpdate::Folder(Update::Preview { plan, .. }) = event {
                    *result.borrow_mut() = Some(plan);
                }
                true
            },
            spools,
        )
        .unwrap();
        result.into_inner().unwrap()
    }
    #[test]
    fn folder_replace_regex_groups_and_mixed_endings() {
        let f = Fixture::new();
        let path = f.file(b"x12\r\nx34\nx56\r\n");
        let mut owner = SpoolOwner::new(8 << 20).unwrap();
        let plan = preview(&f, r"x(?P<number>[0-9]+)", "y${number}$$", &mut owner, vec![]);
        assert_eq!((plan.len(), plan.matches), (1, 3));
        apply_closed(&plan, 0, &[], &token()).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"y12$\r\ny34$\ny56$\r\n");
        drop(plan);
        owner.retire().unwrap();
    }
    #[test]
    fn folder_replace_regex_anchors_respect_crlf_and_eof() {
        let f = Fixture::new();
        let path = f.file(b"x1\r\nx2");
        let mut owner = SpoolOwner::new(8 << 20).unwrap();
        let plan = preview(&f, r"x([0-9]+)$", "y$1", &mut owner, vec![]);
        assert_eq!(plan.matches, 2);
        apply_closed(&plan, 0, &[], &token()).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"y1\r\ny2");
        drop(plan);
        owner.retire().unwrap();
    }

    #[test]
    fn folder_replace_binary_detected_as_legacy_text_is_never_replaced() {
        let f = Fixture::new();
        let raw = b"\x89PNG\r\n\x1a\n\0needle";
        let path = f.file(raw);
        let file = TextFile::open(&path).unwrap();
        assert_eq!(file.kind().unwrap(), kwil_core_text::FileKind::Binary);
        assert_ne!(EncodedFile::new(file).unwrap().detection().encoding, kwil_core_text::encoding::Encoding::UTF8);
        let mut owner = SpoolOwner::new(8 << 20).unwrap();
        let plan = preview(&f, "needle", "changed", &mut owner, vec![]);
        assert_eq!(plan.matches, 0, "legacy encoding heuristics cannot authorize editing binary bytes");
        assert_eq!(std::fs::read(path).unwrap(), raw);
        drop(plan);
        owner.retire().unwrap();
    }

    #[test]
    fn folder_replace_manifest_pages_resume_forward_and_backward() {
        let f = Fixture::new();
        f.file(b"x\n");
        std::fs::write(f.0.join("a.txt"), b"x\n").unwrap();
        std::fs::write(f.0.join("b.txt"), b"x\n").unwrap();
        let mut owner = SpoolOwner::new(8 << 20).unwrap();
        let plan = preview(&f, "x", "y", &mut owner, vec![]);
        let (plan, rows) = page(&plan, Page { first: 2, len: 1 }, &token()).unwrap();
        assert!(matches!(&rows[0],FileEvent::Hit(hit) if hit.line_offset==2));
        assert_eq!(rows.len(), 1);
        let (plan, rows) = page(&plan, Page { first: 0, len: 2 }, &token()).unwrap();
        assert!(matches!(&rows[0],FileEvent::Hit(hit) if hit.line_offset==0));
        assert!(matches!(&rows[1],FileEvent::Hit(hit) if hit.line_offset==1));
        drop(plan);
        owner.retire().unwrap();
    }

    #[test]
    fn folder_replace_skips_changed_file_even_with_same_length() {
        let f = Fixture::new();
        let path = f.file(b"x12\n");
        let mut owner = SpoolOwner::new(8 << 20).unwrap();
        let plan = preview(&f, "x", "y", &mut owner, vec![]);
        std::fs::write(&path, b"z12\n").unwrap();
        assert!(apply_closed(&plan, 0, &[], &token()).unwrap_err().to_string().contains("changed"));
        assert_eq!(std::fs::read(path).unwrap(), b"z12\n");
        drop(plan);
        owner.retire().unwrap();
    }
    #[test]
    fn folder_replace_preserves_utf16_bom_and_line_endings() {
        for little in [true, false] {
            let f = Fixture::new();
            let encode = |text: &str| {
                let mut b = if little { vec![0xff, 0xfe] } else { vec![0xfe, 0xff] };
                for u in text.encode_utf16() {
                    b.extend(if little { u.to_le_bytes() } else { u.to_be_bytes() });
                }
                b
            };
            let path = f.file(&encode("x界\r\nx猫\n"));
            let mut owner = SpoolOwner::new(8 << 20).unwrap();
            let plan = preview(&f, "x", "y", &mut owner, vec![]);
            assert_eq!(plan.matches, 2);
            apply_closed(&plan, 0, &[], &token()).unwrap();
            assert_eq!(std::fs::read(path).unwrap(), encode("y界\r\ny猫\n"));
            drop(plan);
            owner.retire().unwrap();
        }
    }
    #[test]
    fn folder_replace_preserves_legacy_bytes_and_cancels_during_preview() {
        let f = Fixture::new();
        let path = f.file(b"caf\xe9 x\r\n");
        let mut owner = SpoolOwner::new(8 << 20).unwrap();
        let plan = preview(&f, "x", "y", &mut owner, vec![]);
        apply_closed(&plan, 0, &[], &token()).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"caf\xe9 y\r\n");
        drop(plan);
        owner.retire().unwrap();
        for i in 0..32 {
            std::fs::write(f.0.join(format!("{i}.txt")), b"x\n").unwrap();
        }
        let cancel = token();
        let published = std::cell::Cell::new(false);
        let error = run(
            key(),
            &Task::Preview { search: FileSearch::new(f.0.clone(), "x".into()), template: b"y".to_vec(), open: vec![] },
            &cancel,
            &|event| {
                if matches!(event, SearchUpdate::Progress(_)) {
                    cancel.store(true, Ordering::Release);
                }
                if matches!(event, SearchUpdate::Folder(Update::Preview { .. })) {
                    published.set(true);
                }
                true
            },
            &mut owner,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(!published.get());
        owner.retire().unwrap();
    }

    #[test]
    fn folder_replace_cancel_and_reserved_paths_never_write() {
        let f = Fixture::new();
        let path = f.file(b"x\n");
        let mut owner = SpoolOwner::new(8 << 20).unwrap();
        let plan = preview(&f, "x", "y", &mut owner, vec![]);
        let cancel = token();
        cancel.store(true, Ordering::Release);
        assert_eq!(apply_closed(&plan, 0, &[], &cancel).unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert!(
            apply_closed(&plan, 0, std::slice::from_ref(&path), &token()).unwrap_err().to_string().contains("reserved")
        );
        assert_eq!(std::fs::read(path).unwrap(), b"x\n");
        drop(plan);
        owner.retire().unwrap();
    }
    #[test]
    fn folder_replace_hard_link_alias_uses_open_dirty_source() {
        let f = Fixture::new();
        let path = f.file(b"disk\n");
        std::fs::hard_link(&path, f.0.join("alias.txt")).unwrap();
        let mut buffer = Buffer::empty();
        buffer.insert(0, b"x12 x34\n").unwrap();
        let opened = OpenDocument {
            path,
            key: SearchKey { document_id: 7, revision: 1, generation: 0 },
            epoch: 9,
            snapshot: Some(buffer.snapshot()),
        };
        let mut owner = SpoolOwner::new(8 << 20).unwrap();
        let plan = preview(&f, "x([0-9]+)", "y$1", &mut owner, vec![opened]);
        assert_eq!((plan.len(), plan.matches), (2, 4));
        drop(plan);
        owner.retire().unwrap();
    }

    #[test]
    fn folder_replace_open_dirty_snapshot_returns_editor_recipe_without_writing() {
        let f = Fixture::new();
        let path = f.file(b"disk\n");
        let mut buffer = Buffer::empty();
        buffer.insert(0, b"x12 x34\n").unwrap();
        let opened = OpenDocument {
            path: path.clone(),
            key: SearchKey { document_id: 7, revision: 12, generation: 0 },
            epoch: 88,
            snapshot: Some(buffer.snapshot()),
        };
        let mut owner = SpoolOwner::new(8 << 20).unwrap();
        let plan = preview(&f, "x([0-9]+)", "y$1", &mut owner, vec![opened.clone()]);
        assert_eq!(plan.matches, 2);
        let result = RefCell::new(None);
        // The real callback below retains only the recipe, as a frontend would.
        let opened = OpenDocument {
            path: path.clone(),
            key: SearchKey { document_id: 7, revision: 12, generation: 0 },
            epoch: 88,
            snapshot: Some(buffer.snapshot()),
        };
        run(
            key(),
            &Task::Apply { plan: plan.clone(), index: 0, cursor: 0, open: vec![opened] },
            &token(),
            &|event| {
                if let SearchUpdate::Folder(Update::Applied { result: Ok(recipe), .. }) = event {
                    *result.borrow_mut() = recipe;
                }
                true
            },
            &mut owner,
        )
        .unwrap();
        let recipe = result.into_inner().unwrap();
        assert_eq!(recipe.key.document_id, 7);
        assert_eq!(recipe.edits.len(), 2);
        assert_eq!(recipe.edits.iter().next().unwrap().unwrap().new_bytes, b"y12");
        assert_eq!(std::fs::read(path).unwrap(), b"disk\n");
        drop((recipe, plan));
        owner.retire().unwrap();
    }
}
