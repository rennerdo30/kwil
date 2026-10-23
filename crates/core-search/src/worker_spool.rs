//! Private replacement spool. Creation/writes/iteration/retirement are worker
//! operations. Published recipes are cheap immutable pins; Drop performs no I/O.
use crate::{Edit, FileEvent, FileHit};
use std::{
    fs::{File, OpenOptions},
    io::{self, BufWriter, Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_PAYLOAD: u64 = 8 << 20;
const RECORD_OVERHEAD: u64 = 32;
const MAX_LIVE: usize = 8;
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Data {
    path: PathBuf,
    count: u64,
    bytes: u64,
    wake: Arc<dyn Fn() + Send + Sync>,
    lease: Arc<()>,
}
#[derive(Clone)]
pub struct SpoolRecipe {
    data: Option<Arc<Data>>,
}
impl std::fmt::Debug for SpoolRecipe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpoolRecipe").field("count", &self.data().count).field("bytes", &self.data().bytes).finish()
    }
}
impl SpoolRecipe {
    fn data(&self) -> &Arc<Data> {
        self.data.as_ref().unwrap()
    }
    pub fn len(&self) -> u64 {
        self.data().count
    }
    pub fn is_empty(&self) -> bool {
        self.data().count == 0
    }
    pub fn bytes(&self) -> u64 {
        self.data().bytes
    }
    /// Worker-only iterator, increasing original-document coordinates. Reverse
    /// it for descending atomic application. Clones share only an immutable pin.
    pub fn edits(&self) -> SpoolEdits {
        SpoolEdits {
            recipe: self.clone(),
            front: 0,
            back: self.data().bytes,
            remaining: self.data().count,
            failed: false,
        }
    }
}
impl Drop for SpoolRecipe {
    fn drop(&mut self) {
        let data = self.data.take().unwrap();
        let wake = data.wake.clone();
        drop(data); // release pin BEFORE waking retirement; Drop never unlinks.
        wake();
    }
}
#[derive(Clone)]
pub struct SpoolEdits {
    recipe: SpoolRecipe,
    front: u64,
    back: u64,
    remaining: u64,
    failed: bool,
}
impl SpoolEdits {
    fn read_at(&self, at: u64, expected_end: Option<u64>) -> io::Result<(Edit, u64)> {
        let mut file = File::open(&self.recipe.data().path)?;
        file.seek(SeekFrom::Start(at))?;
        let offset = number(&mut file)?;
        let len = number(&mut file)?;
        let payload = number(&mut file)?;
        if payload > MAX_PAYLOAD || offset.checked_add(len).is_none() {
            return Err(invalid("invalid replacement spool record"));
        }
        let bytes = RECORD_OVERHEAD.checked_add(payload).ok_or_else(|| invalid("replacement spool length overflow"))?;
        let end = at
            .checked_add(bytes)
            .filter(|end| *end <= self.back)
            .ok_or_else(|| invalid("replacement spool record exceeds cursor"))?;
        if expected_end.is_some_and(|expected| end != expected) {
            return Err(invalid("replacement spool trailer mismatch"));
        }
        let mut new_bytes = vec![0; payload as usize];
        file.read_exact(&mut new_bytes)?;
        if number(&mut file)? != bytes {
            return Err(invalid("replacement spool record length mismatch"));
        }
        Ok((Edit { offset, len, new_bytes }, end))
    }
    fn fail(&mut self, error: io::Error) -> Option<io::Result<Edit>> {
        self.failed = true;
        Some(Err(error))
    }
}
impl Iterator for SpoolEdits {
    type Item = io::Result<Edit>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.remaining == 0 {
            return None;
        }
        match self.read_at(self.front, None) {
            Ok((edit, end)) => {
                self.front = end;
                self.remaining -= 1;
                if self.remaining == 0 && self.front != self.back {
                    return self.fail(invalid("replacement spool count mismatch"));
                }
                Some(Ok(edit))
            }
            Err(error) => self.fail(error),
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        // An I/O error yields one item then terminates, so zero is the only
        // sound lower bound. Counts can exceed usize on 32-bit platforms.
        (0, usize::try_from(self.remaining).ok())
    }
}
impl DoubleEndedIterator for SpoolEdits {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.failed || self.remaining == 0 {
            return None;
        }
        let result = (|| {
            let trailer = self
                .back
                .checked_sub(8)
                .filter(|p| *p >= self.front)
                .ok_or_else(|| invalid("replacement spool reverse cursor underflow"))?;
            let mut file = File::open(&self.recipe.data().path)?;
            file.seek(SeekFrom::Start(trailer))?;
            let bytes = number(&mut file)?;
            if !(RECORD_OVERHEAD..=RECORD_OVERHEAD + MAX_PAYLOAD).contains(&bytes) {
                return Err(invalid("invalid replacement spool reverse length"));
            }
            let at = self
                .back
                .checked_sub(bytes)
                .filter(|p| *p >= self.front)
                .ok_or_else(|| invalid("replacement spool reverse record underflow"))?;
            let (edit, _) = self.read_at(at, Some(self.back))?;
            Ok((edit, at))
        })();
        match result {
            Ok((edit, at)) => {
                self.back = at;
                self.remaining -= 1;
                if self.remaining == 0 && self.front != self.back {
                    return self.fail(invalid("replacement spool reverse count mismatch"));
                }
                Some(Ok(edit))
            }
            Err(error) => self.fail(error),
        }
    }
}

enum Slot {
    Writing {
        path: PathBuf,
        file: BufWriter<File>,
        bytes: u64,
        count: u64,
        previous: Option<(u64, u64)>,
        failed: bool,
        lease: Arc<()>,
    },
    Published(Arc<Data>),
    Cleanup {
        path: PathBuf,
        bytes: u64,
        lease: Arc<()>,
    },
}

/// Explicit worker owner; never transfer this to a UI completion. Failed
/// unlink retains its native path for the next retirement attempt.
#[derive(Clone, Debug)]
pub struct SpoolDiagnostic {
    pub path: PathBuf,
    pub kind: io::ErrorKind,
    pub message: String,
}
pub struct SpoolOwner {
    slots: Vec<Option<Slot>>,
    max_bytes: u64,
    wake: Arc<dyn Fn() + Send + Sync>,
    diagnostics: Vec<SpoolDiagnostic>,
    _worker: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl SpoolOwner {
    /// The aggregate live disk cap is caller policy. No I/O during construction.
    pub fn new(max_bytes: u64) -> io::Result<Self> {
        Self::with_wake(max_bytes, || {})
    }
    pub fn with_wake(max_bytes: u64, wake: impl Fn() + Send + Sync + 'static) -> io::Result<Self> {
        if max_bytes < RECORD_OVERHEAD {
            return Err(invalid("replacement spool cap too small"));
        }
        Ok(Self {
            slots: (0..MAX_LIVE).map(|_| None).collect(),
            max_bytes,
            wake: Arc::new(wake),
            diagnostics: Vec::with_capacity(MAX_LIVE),
            _worker: std::marker::PhantomData,
        })
    }
    pub fn begin(&mut self) -> io::Result<usize> {
        self.retire()?;
        let slot = self
            .slots
            .iter()
            .position(Option::is_none)
            .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "replacement spool owners are pinned"))?;
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        // A create_new flat private file avoids a directory whose cleanup could
        // accidentally target somebody else's output. Its exact path is retained.
        let path = std::env::temp_dir().join(format!("kwil-search-{}-{stamp}-{sequence}.spool", std::process::id()));
        if path.as_os_str().as_encoded_bytes().len() > 32768 {
            return Err(invalid("replacement spool path exceeds cap"));
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        self.slots[slot] = Some(Slot::Writing {
            path,
            file: BufWriter::with_capacity(64 << 10, file),
            bytes: 0,
            count: 0,
            previous: None,
            failed: false,
            lease: Arc::new(()),
        });
        Ok(slot)
    }
    pub fn append(&mut self, slot: usize, edit: Edit) -> io::Result<()> {
        let used = self.bytes();
        let bytes = RECORD_OVERHEAD
            .checked_add(edit.new_bytes.len() as u64)
            .ok_or_else(|| invalid("replacement spool record overflow"))?;
        if edit.new_bytes.len() as u64 > MAX_PAYLOAD || bytes > self.max_bytes.saturating_sub(used) {
            return Err(invalid("replacement spool disk/payload cap exceeded"));
        }
        let Some(Some(Slot::Writing { file, bytes: written, count, previous, failed, .. })) = self.slots.get_mut(slot)
        else {
            return Err(invalid("replacement spool is not writable"));
        };
        if *failed {
            return Err(invalid("replacement spool retains a failed write"));
        }
        let end = edit.offset.checked_add(edit.len).ok_or_else(|| invalid("replacement range overflow"))?;
        if previous.is_some_and(|(offset, end)| edit.offset <= offset || edit.offset < end) {
            return Err(invalid("replacement spool edits overlap or are unordered"));
        }
        let next_count = count.checked_add(1).ok_or_else(|| invalid("replacement count overflow"))?;
        let next_bytes = written.checked_add(bytes).ok_or_else(|| invalid("replacement spool byte overflow"))?;
        // Charge the entire write before I/O; partial failures still own disk.
        *written = next_bytes;
        *failed = true;
        file.write_all(&edit.offset.to_le_bytes())?;
        file.write_all(&edit.len.to_le_bytes())?;
        file.write_all(&(edit.new_bytes.len() as u64).to_le_bytes())?;
        file.write_all(&edit.new_bytes)?;
        file.write_all(&bytes.to_le_bytes())?;
        *previous = Some((edit.offset, end));
        *failed = false;
        *count = next_count;
        Ok(())
    }
    pub fn finish(&mut self, slot: usize) -> io::Result<SpoolRecipe> {
        let Some(Some(Slot::Writing { file, failed, .. })) = self.slots.get_mut(slot) else {
            return Err(invalid("replacement spool is not writable"));
        };
        if *failed {
            return Err(invalid("replacement spool retains a failed write"));
        }
        file.flush()?;
        let Slot::Writing { path, file, bytes, count, lease, .. } = self.slots[slot].take().unwrap() else {
            unreachable!()
        };
        drop(file);
        let data = Arc::new(Data { path, count, bytes, wake: self.wake.clone(), lease });
        self.slots[slot] = Some(Slot::Published(data.clone()));
        Ok(SpoolRecipe { data: Some(data) })
    }
    /// A flushed immutable prefix, sharing the writer's lifetime lease.
    pub fn checkpoint(&mut self, slot: usize) -> io::Result<SpoolRecipe> {
        let Some(Some(Slot::Writing { file, failed, path, count, bytes, lease, .. })) = self.slots.get_mut(slot) else {
            return Err(invalid("replacement spool is not writable"));
        };
        if *failed {
            return Err(invalid("replacement spool retains a failed write"));
        }
        file.flush()?;
        Ok(SpoolRecipe {
            data: Some(Arc::new(Data {
                path: path.clone(),
                count: *count,
                bytes: *bytes,
                wake: self.wake.clone(),
                lease: lease.clone(),
            })),
        })
    }
    pub fn abandon(&mut self, slot: usize) {
        if let Some(Slot::Writing { path, file, bytes, lease, .. }) = self.slots[slot].take() {
            drop(file);
            self.slots[slot] = Some(Slot::Cleanup { path, bytes, lease });
        }
    }
    pub fn bytes(&self) -> u64 {
        self.slots
            .iter()
            .flatten()
            .map(|slot| match slot {
                Slot::Writing { bytes, .. } | Slot::Cleanup { bytes, .. } => *bytes,
                Slot::Published(data) => data.bytes,
            })
            .sum()
    }
    pub fn live(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }
    pub fn diagnostics(&self) -> &[SpoolDiagnostic] {
        &self.diagnostics
    }
    pub fn retire(&mut self) -> io::Result<()> {
        self.diagnostics.clear();
        let mut first_error = None;
        for slot in &mut self.slots {
            let path = match slot {
                Some(Slot::Published(data)) if Arc::strong_count(data) == 1 && Arc::strong_count(&data.lease) == 1 => {
                    &data.path
                }
                Some(Slot::Cleanup { path, lease, .. }) if Arc::strong_count(lease) == 1 => path,
                _ => continue,
            };
            match std::fs::remove_file(path) {
                Ok(()) => *slot = None,
                Err(error) if error.kind() == io::ErrorKind::NotFound => *slot = None,
                Err(error) => {
                    self.diagnostics.push(SpoolDiagnostic {
                        path: path.clone(),
                        kind: error.kind(),
                        message: error.to_string(),
                    });
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}
fn number(file: &mut File) -> io::Result<u64> {
    let mut bytes = [0; 8];
    file.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

const MAX_FILE_RECORD: usize = 128 << 10;
/// Native path encoding is internal to this machine-private ephemeral spool.
/// No lossy strings participate in storage, diagnostics or file navigation.
fn path_bytes(path: &std::path::Path) -> io::Result<Vec<u8>> {
    if path.as_os_str().as_encoded_bytes().len() > 32768 {
        return Err(invalid("result path exceeds native path cap"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        Ok(path.as_os_str().encode_wide().flat_map(u16::to_le_bytes).collect())
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(path.as_os_str().as_bytes().to_vec())
    }
}
fn native_path(bytes: &[u8]) -> io::Result<PathBuf> {
    if bytes.len() > 65536 {
        return Err(invalid("result path record exceeds cap"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        let (pairs, remainder) = bytes.as_chunks::<2>();
        if !remainder.is_empty() {
            return Err(invalid("invalid native wide path record"));
        }
        let units: Vec<_> = pairs.iter().map(|bytes| u16::from_le_bytes(*bytes)).collect();
        Ok(PathBuf::from(std::ffi::OsString::from_wide(&units)))
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec())))
    }
}
pub(crate) fn encode_file_event(row: &FileEvent) -> io::Result<Vec<u8>> {
    let path = path_bytes(match row {
        FileEvent::Hit(hit) => &hit.path,
        FileEvent::Error { path, .. } => path,
    })?;
    let payload = match row {
        FileEvent::Hit(hit) => {
            if hit.line.len() > 4096 || hit.match_ranges.len() > 256 {
                return Err(invalid("file result row exceeds cap"));
            }
            8 + 8 + 1 + 4 + hit.line.len() + 4 + hit.match_ranges.len() * 16
        }
        FileEvent::Error { message, .. } => {
            if message.len() > 64 << 10 {
                return Err(invalid("file result diagnostic exceeds cap"));
            }
            4 + message.len()
        }
    };
    let bytes = 1 + 4 + path.len() + payload;
    if bytes > MAX_FILE_RECORD {
        return Err(invalid("file result record exceeds cap"));
    }
    let mut output = Vec::with_capacity(bytes);
    output.push(u8::from(matches!(row, FileEvent::Error { .. })));
    output.extend_from_slice(&(path.len() as u32).to_le_bytes());
    output.extend_from_slice(&path);
    match row {
        FileEvent::Hit(hit) => {
            output.extend_from_slice(&hit.line_number.to_le_bytes());
            output.extend_from_slice(&hit.line_offset.to_le_bytes());
            output.push(u8::from(hit.line_truncated) | u8::from(hit.ranges_truncated) << 1);
            output.extend_from_slice(&(hit.line.len() as u32).to_le_bytes());
            output.extend_from_slice(&hit.line);
            output.extend_from_slice(&(hit.match_ranges.len() as u32).to_le_bytes());
            for range in &hit.match_ranges {
                output.extend_from_slice(&(range.start as u64).to_le_bytes());
                output.extend_from_slice(&(range.end as u64).to_le_bytes());
            }
        }
        FileEvent::Error { message, .. } => {
            output.extend_from_slice(&(message.len() as u32).to_le_bytes());
            output.extend_from_slice(message.as_bytes());
        }
    }
    Ok(output)
}
pub(crate) fn decode_file_event(bytes: &[u8]) -> io::Result<FileEvent> {
    if bytes.len() > MAX_FILE_RECORD {
        return Err(invalid("file result record exceeds cap"));
    }
    let mut input = RowReader { bytes, at: 0 };
    let tag = input.take(1)?[0];
    let path_len = input.u32()? as usize;
    let path = native_path(input.take(path_len)?)?;
    let row = match tag {
        0 => {
            let line_number = input.u64()?;
            let line_offset = input.u64()?;
            let flags = input.take(1)?[0];
            let preview_len = input.u32()? as usize;
            if preview_len > 4096 {
                return Err(invalid("file result preview exceeds cap"));
            }
            let line = input.take(preview_len)?.to_vec();
            let count = input.u32()? as usize;
            if count > 256 {
                return Err(invalid("file result ranges exceed cap"));
            }
            let mut match_ranges = Vec::with_capacity(count);
            for _ in 0..count {
                let start =
                    usize::try_from(input.u64()?).map_err(|_| invalid("file result offset exceeds platform"))?;
                let end = usize::try_from(input.u64()?).map_err(|_| invalid("file result offset exceeds platform"))?;
                if end < start {
                    return Err(invalid("file result reversed range"));
                }
                match_ranges.push(start..end);
            }
            FileEvent::Hit(FileHit {
                path,
                line_number,
                line_offset,
                line,
                line_truncated: flags & 1 != 0,
                match_ranges,
                ranges_truncated: flags & 2 != 0,
            })
        }
        1 => {
            let len = input.u32()? as usize;
            if len > 64 << 10 {
                return Err(invalid("file diagnostic exceeds cap"));
            }
            let message =
                std::str::from_utf8(input.take(len)?).map_err(|_| invalid("invalid file diagnostic"))?.to_owned();
            FileEvent::Error { path, message }
        }
        _ => return Err(invalid("invalid file result tag")),
    };
    if input.at != bytes.len() {
        return Err(invalid("trailing file result bytes"));
    }
    Ok(row)
}
struct RowReader<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> RowReader<'a> {
    fn take(&mut self, count: usize) -> io::Result<&'a [u8]> {
        let end = self.at.checked_add(count).ok_or_else(|| invalid("file result length overflow"))?;
        let bytes = self.bytes.get(self.at..end).ok_or_else(|| invalid("truncated file result record"))?;
        self.at = end;
        Ok(bytes)
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn edit(offset: u64, text: &[u8]) -> Edit {
        Edit { offset, len: 1, new_bytes: text.to_vec() }
    }
    #[test]
    fn file_result_native_path_codec_round_trips_unpaired_units_and_raw_offsets() {
        #[cfg(windows)]
        let path = {
            use std::os::windows::ffi::OsStringExt;
            PathBuf::from(std::ffi::OsString::from_wide(&[0x78, 0xd800, 0x79]))
        };
        #[cfg(unix)]
        let path = {
            use std::os::unix::ffi::OsStringExt;
            PathBuf::from(std::ffi::OsString::from_vec(vec![b'x', 0xff, b'y']))
        };
        let event = FileEvent::Hit(FileHit {
            path: path.clone(),
            line_number: 9,
            line_offset: 200,
            line: b"preview".to_vec(),
            line_truncated: true,
            ranges_truncated: true,
            match_ranges: std::iter::once(80..84).collect(),
        });
        let FileEvent::Hit(decoded) = decode_file_event(&encode_file_event(&event).unwrap()).unwrap() else {
            panic!("hit record")
        };
        assert_eq!(decoded.path.as_os_str(), path.as_os_str());
        assert_eq!((decoded.line_number, decoded.line_offset), (9, 200));
        assert_eq!(decoded.match_ranges.len(), 1);
        assert_eq!(decoded.match_ranges[0], 80..84);
        assert!(decoded.ranges_truncated && decoded.line_truncated);
    }
    #[test]
    fn cancelled_writing_spool_keeps_published_prefix_until_pin_release() {
        let mut owner = SpoolOwner::new(4096).unwrap();
        let slot = owner.begin().unwrap();
        owner.append(slot, edit(0, b"a")).unwrap();
        let prefix = owner.checkpoint(slot).unwrap();
        let path = prefix.data().path.clone();
        owner.append(slot, edit(2, b"b")).unwrap();
        assert_eq!(prefix.len(), 1);
        owner.abandon(slot);
        owner.retire().unwrap();
        assert!(path.exists());
        assert_eq!(prefix.edits().count(), 1);
        drop(prefix);
        owner.retire().unwrap();
        assert!(!path.exists());
    }
    #[test]
    fn spool_forward_reverse_clone_and_pin_retirement() {
        fn send_sync_clone<T: Send + Sync + Clone>() {}
        send_sync_clone::<SpoolEdits>();
        let mut owner = SpoolOwner::new(4096).unwrap();
        let slot = owner.begin().unwrap();
        owner.append(slot, edit(2, b"AA")).unwrap();
        owner.append(slot, edit(7, b"B")).unwrap();
        let recipe = owner.finish(slot).unwrap();
        let path = recipe.data().path.clone();
        let mut forward = recipe.edits();
        let mut reverse = forward.clone().rev();
        assert_eq!(forward.next().unwrap().unwrap(), edit(2, b"AA"));
        assert_eq!(reverse.next().unwrap().unwrap(), edit(7, b"B"));
        assert_eq!(forward.next_back().unwrap().unwrap(), edit(7, b"B"));
        assert!(forward.next().is_none());
        owner.retire().unwrap();
        assert_eq!(owner.live(), 1);
        drop((forward, reverse, recipe));
        assert!(path.exists(), "lease Drop does not unlink");
        owner.retire().unwrap();
        assert_eq!(owner.live(), 0);
        assert!(!path.exists());
    }
    #[test]
    fn spool_budget_and_count_overflow_refuse_before_growth() {
        let mut owner = SpoolOwner::new(35).unwrap();
        let slot = owner.begin().unwrap();
        owner.append(slot, edit(0, b"abc")).unwrap();
        assert!(owner.append(slot, edit(2, b"x")).is_err());
        assert_eq!(owner.bytes(), 35);
        owner.abandon(slot);
        owner.retire().unwrap();
        let slot = owner.begin().unwrap();
        if let Some(Slot::Writing { count, .. }) = &mut owner.slots[slot] {
            *count = u64::MAX;
        }
        assert!(owner.append(slot, edit(0, b"x")).is_err());
        assert_eq!(owner.bytes(), 0);
        owner.abandon(slot);
        owner.retire().unwrap();
    }
    #[test]
    fn spool_cleanup_failure_keeps_exact_native_path_until_retry() {
        let mut owner = SpoolOwner::new(100).unwrap();
        let slot = owner.begin().unwrap();
        owner.append(slot, edit(0, b"a")).unwrap();
        let recipe = owner.finish(slot).unwrap();
        let path = recipe.data().path.clone();
        drop(recipe);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap(); // injected unlink failure on our exact private path.
        assert!(owner.retire().is_err());
        assert_eq!(owner.live(), 1);
        assert_eq!(owner.diagnostics()[0].path.as_os_str(), path.as_os_str());
        std::fs::remove_dir(&path).unwrap();
        owner.retire().unwrap();
        assert_eq!(owner.live(), 0);
    }
    #[test]
    fn spool_corrupt_reverse_length_is_a_single_error_not_allocation() {
        let mut owner = SpoolOwner::new(100).unwrap();
        let slot = owner.begin().unwrap();
        owner.append(slot, edit(0, b"a")).unwrap();
        let recipe = owner.finish(slot).unwrap();
        let mut file = OpenOptions::new().write(true).open(&recipe.data().path).unwrap();
        file.seek(SeekFrom::Start(recipe.bytes() - 8)).unwrap();
        file.write_all(&u64::MAX.to_le_bytes()).unwrap();
        drop(file);
        let mut edits = recipe.edits();
        assert!(edits.next_back().unwrap().is_err());
        assert!(edits.next().is_none());
        drop((edits, recipe));
        owner.retire().unwrap();
    }
}
