//! Admitted worker queries over the EXISTING piece tree and sparse line index.
//! ROOT registration: #[path = "worker_layout.rs"] pub mod worker_layout; in
//! piece_tree, followed by its public reexport from core-text. No legacy cursor,
//! index, SourceChunks or Snapshot synchronous query is modified here.
//!
//! Query headers are fixed-size, !Send and caller-admitted. No query builds an
//! index or copies a line. A read uses the existing WorkerSourceCursor, with
//! logical/raw windows supplied by the caller's admitted HEAP scratch owner.
use super::worker_source::{WorkerSourceChunk, WorkerSourceCursor, WorkerSourceError, WorkerSourceStep};
use super::{Kind, Node, Storage};
use crate::compare_input::{InputError, LineSpan, Rank, ReadWork, Step, Terminator};
use crate::decoded_backing::{AtomKind, BackingError, Boundary, ResumeAtom, ResumeBoundary};
use crate::{LineIndex, ReadError, Snapshot};
use std::{
    io,
    marker::PhantomData,
    mem::size_of,
    ops::Range,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

macro_rules! ready {
    ($step:expr) => {
        match $step {
            Step::Ready(value) => value,
            Step::Pending => return Step::Pending,
            Step::Cancelled => return Step::Cancelled,
            Step::Failed(error) => return Step::Failed(error),
        }
    };
}
fn check(work: &mut ReadWork, cancel: &AtomicBool) -> Step<()> {
    work.debit(0, 0, cancel)
}
fn read_error(error: ReadError) -> InputError {
    match error {
        ReadError::Changed(_) => InputError::SourceChanged,
        ReadError::Io(error) => InputError::Io(error),
    }
}
fn backing_error(error: BackingError) -> InputError {
    match error {
        BackingError::Read(error) => read_error(error),
        BackingError::Io(error) => InputError::Io(error),
        BackingError::Encoding(crate::encoding::EncodingError::Io(error)) => InputError::Io(error),
        BackingError::Cancelled => InputError::SourceContract,
        other => InputError::Provenance(other),
    }
}
fn cursor_error(error: WorkerSourceError) -> InputError {
    match error {
        WorkerSourceError::Io(error) => {
            if error.get_ref().is_some_and(|inner| inner.is::<ReadError>()) {
                read_error(*error.into_inner().unwrap().downcast::<ReadError>().unwrap())
            } else {
                InputError::Io(error)
            }
        }
        WorkerSourceError::Backing(error) => backing_error(error),
        WorkerSourceError::InvalidRange => InputError::Range,
        _ => InputError::SourceContract,
    }
}

#[derive(Default)]
struct Credit {
    required: Option<ReadWork>,
    paid: ReadWork,
}
impl Credit {
    fn poll(&mut self, bytes: u64, operations: u64, work: &mut ReadWork, cancel: &AtomicBool) -> Step<()> {
        let required = ReadWork::new(bytes, operations);
        if self.required.is_some_and(|old| old != required) {
            return Step::Failed(InputError::QueryChanged);
        }
        self.required = Some(required);
        let take = ReadWork::new(
            work.bytes.min(bytes - self.paid.bytes),
            work.operations.min(operations - self.paid.operations),
        );
        ready!(work.debit(take.bytes, take.operations, cancel));
        self.paid.bytes += take.bytes;
        self.paid.operations += take.operations;
        if self.paid == required {
            *self = Self::default();
            Step::Ready(())
        } else {
            Step::Pending
        }
    }
    fn admit(&mut self, bytes: usize, work: &mut ReadWork, cancel: &AtomicBool) -> io::Result<bool> {
        match self.poll(bytes as u64, 1, work, cancel) {
            Step::Ready(()) => Ok(true),
            Step::Pending | Step::Cancelled => Ok(false),
            Step::Failed(InputError::Io(error)) => Err(error),
            Step::Failed(error) => Err(io::Error::other(error)),
        }
    }
}
#[derive(Default)]
struct Binding {
    root: Option<usize>,
    _worker: PhantomData<Rc<()>>,
}
impl Binding {
    fn check(&mut self, snapshot: &Snapshot) -> Step<()> {
        let root = snapshot.root.as_ref().map_or(0, |root| Arc::as_ptr(root) as usize);
        match self.root {
            Some(old) if old != root => Step::Failed(InputError::QueryChanged),
            _ => {
                self.root = Some(root);
                Step::Ready(())
            }
        }
    }
}

#[derive(Clone, Copy)]
enum TreeTarget {
    Byte(u64),
    Break(u64),
}
struct TreeLookup {
    binding: Binding,
    node: Option<Arc<Node>>,
    base: u64,
    breaks: u64,
    target: TreeTarget,
    initialized: bool,
    done: bool,
    credit: Credit,
}
impl TreeLookup {
    fn new(target: TreeTarget) -> Self {
        Self {
            binding: Binding::default(),
            node: None,
            base: 0,
            breaks: 0,
            target,
            initialized: false,
            done: false,
            credit: Credit::default(),
        }
    }
    fn poll(&mut self, snapshot: &Snapshot, work: &mut ReadWork, cancel: &AtomicBool) -> Step<()> {
        ready!(check(work, cancel));
        ready!(self.binding.check(snapshot));
        if !self.initialized {
            ready!(self.credit.poll(size_of::<Arc<Node>>() as u64, 1, work, cancel));
            self.node = snapshot.root.clone();
            self.initialized = true;
        }
        while !self.done {
            let Some(node) = &self.node else {
                self.done = true;
                break;
            };
            ready!(self.credit.poll((size_of::<Node>() + 2 * size_of::<super::Metrics>()) as u64, 1, work, cancel));
            match &node.kind {
                Kind::Leaf(_) => self.done = true,
                Kind::Branch(left, right) => {
                    let rightward = match self.target {
                        TreeTarget::Byte(at) => at >= self.base + left.metrics.bytes,
                        TreeTarget::Break(nth) => nth > left.metrics.breaks,
                    };
                    if rightward {
                        self.base += left.metrics.bytes;
                        self.breaks += left.metrics.breaks;
                        if let TreeTarget::Break(nth) = &mut self.target {
                            *nth -= left.metrics.breaks;
                        }
                        self.node = Some(right.clone());
                    } else {
                        self.node = Some(left.clone());
                    }
                }
            }
        }
        Step::Ready(())
    }
}
fn storage_len(storage: &Storage) -> u64 {
    match storage {
        Storage::Original(file) => file.len(),
        Storage::Decoded(source) => source.origin.logical_bytes().len() as u64,
        Storage::Memory { bytes, .. } => bytes.len() as u64,
        Storage::Disk(block) => block.payload_len as u64,
        Storage::Opaque { logical, .. } => storage_len(logical),
    }
}
fn index(storage: &Storage) -> Option<&LineIndex> {
    match storage {
        Storage::Original(file) => file.index(),
        Storage::Decoded(source) => Some(&source.index),
        _ => None,
    }
}
fn worker_bytes(storage: &Storage) -> Result<&[u8], InputError> {
    match storage {
        Storage::Original(file) => file.worker_bytes().map_err(read_error),
        _ => storage.bytes().map_err(read_error),
    }
}
fn source_operations(storage: &Storage) -> u64 {
    if matches!(storage, Storage::Original(_)) { 8 } else { 1 }
}

#[derive(Clone, Copy)]
struct CheckpointSelector {
    ranks: bool,
    target: u64,
    by_offset: bool,
    strict: bool,
}

#[derive(Default)]
struct Search {
    initialized: bool,
    low: usize,
    high: usize,
    best: Option<(u64, u64)>,
    credit: Credit,
}
impl Search {
    fn poll(
        &mut self,
        index: &LineIndex,
        selector: CheckpointSelector,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<Option<(u64, u64)>> {
        let CheckpointSelector { ranks, target, by_offset, strict } = selector;
        if !self.initialized {
            ready!(self.credit.poll(size_of::<usize>() as u64, 1, work, cancel));
            self.high = index.worker_checkpoint_count(ranks);
            self.initialized = true;
        }
        while self.low < self.high {
            ready!(self.credit.poll(16, 1, work, cancel));
            let mid = self.low + (self.high - self.low) / 2;
            let Some(record) = index.worker_checkpoint(ranks, mid) else {
                return Step::Failed(InputError::CorruptStore);
            };
            let value = if by_offset { record.1 } else { record.0 };
            if if strict { value < target } else { value <= target } {
                self.best = Some(record);
                self.low = mid + 1;
            } else {
                self.high = mid;
            }
        }
        Step::Ready(self.best)
    }
}
#[derive(Clone, Copy)]
enum StorageTarget {
    Rank(u64),
    Start(u64),
}
enum StoragePhase {
    Lines,
    Ranks,
    Scan,
    Done,
}
struct StorageLookup {
    target: StorageTarget,
    lines: Search,
    ranks: Search,
    phase: StoragePhase,
    selected: (u64, u64),
    at: u64,
    count: u64,
    result: u64,
    credit: Credit,
}
impl StorageLookup {
    fn new(target: StorageTarget) -> Self {
        Self {
            target,
            lines: Search::default(),
            ranks: Search::default(),
            phase: StoragePhase::Lines,
            selected: (0, 0),
            at: 0,
            count: 0,
            result: 0,
            credit: Credit::default(),
        }
    }
    fn poll(&mut self, storage: &Storage, work: &mut ReadWork, cancel: &AtomicBool) -> Step<u64> {
        loop {
            ready!(check(work, cancel));
            match self.phase {
                StoragePhase::Lines => {
                    if matches!(storage, Storage::Original(_) | Storage::Decoded(_)) && index(storage).is_none() {
                        return Step::Failed(InputError::SourceContract);
                    }
                    if let Some(index) = index(storage) {
                        let (target, by_offset) = match self.target {
                            StorageTarget::Rank(at) => (at, true),
                            StorageTarget::Start(line) => (line, false),
                        };
                        self.selected = match ready!(self.lines.poll(
                            index,
                            CheckpointSelector { ranks: false, target, by_offset, strict: false },
                            work,
                            cancel
                        )) {
                            Some(record) => record,
                            None => return Step::Failed(InputError::CorruptStore),
                        };
                    }
                    if let StorageTarget::Start(line) = self.target
                        && self.selected.0 == line
                    {
                        self.result = self.selected.1;
                        self.phase = StoragePhase::Done;
                    } else {
                        self.phase = StoragePhase::Ranks;
                    }
                }
                StoragePhase::Ranks => {
                    if let Some(index) = index(storage) {
                        let (target, by_offset, strict) = match self.target {
                            StorageTarget::Rank(at) => (at, true, false),
                            StorageTarget::Start(line) => (line, false, true),
                        };
                        if let Some(record) = ready!(self.ranks.poll(
                            index,
                            CheckpointSelector { ranks: true, target, by_offset, strict },
                            work,
                            cancel
                        )) && record.1 > self.selected.1
                        {
                            self.selected = record;
                        }
                    }
                    self.count = self.selected.0;
                    self.at = self.selected.1;
                    self.phase = StoragePhase::Scan;
                }
                StoragePhase::Scan => {
                    let end = match self.target {
                        StorageTarget::Rank(at) => at,
                        StorageTarget::Start(_) => storage_len(storage),
                    };
                    if self.at > end || end > storage_len(storage) {
                        return Step::Failed(InputError::Range);
                    }
                    if self.at == end {
                        if matches!(self.target, StorageTarget::Start(_)) {
                            return Step::Failed(InputError::CorruptStore);
                        }
                        self.result = self.count;
                        self.phase = StoragePhase::Done;
                        continue;
                    }
                    let n = (end - self.at).min(super::ADD_BLOCK as u64);
                    // One complete bounded map/check/scan. Credits accumulate
                    // before access; no borrowed mapped slice survives Pending.
                    ready!(self.credit.poll(n, source_operations(storage) + 1, work, cancel));
                    let bytes = match worker_bytes(storage) {
                        Ok(bytes) => bytes,
                        Err(error) => return Step::Failed(error),
                    };
                    let stop = self.at + n;
                    while self.at < stop {
                        let byte = bytes[self.at as usize];
                        self.at += 1;
                        if byte == b'\n' {
                            self.count += 1;
                        }
                        if let StorageTarget::Start(line) = self.target
                            && self.count == line
                        {
                            self.result = self.at;
                            self.phase = StoragePhase::Done;
                            break;
                        }
                    }
                    if matches!(self.target, StorageTarget::Rank(_)) && self.at == end {
                        self.result = self.count;
                        self.phase = StoragePhase::Done;
                    }
                }
                StoragePhase::Done => return Step::Ready(self.result),
            }
        }
    }
}

/// Offset after a logical newline using tree metrics + existing sparse index.
pub struct LayoutStartQuery {
    line: u64,
    walk: TreeLookup,
    storage: Option<StorageLookup>,
}
impl LayoutStartQuery {
    pub fn new(line: u64) -> Self {
        Self { line, walk: TreeLookup::new(TreeTarget::Break(line)), storage: None }
    }
    pub fn poll(&mut self, snapshot: &Snapshot, work: &mut ReadWork, cancel: &AtomicBool) -> Step<u64> {
        ready!(check(work, cancel));
        ready!(self.walk.binding.check(snapshot));
        if self.line >= snapshot.line_count() {
            return Step::Failed(InputError::Range);
        }
        if self.line == 0 {
            return Step::Ready(0);
        }
        ready!(self.walk.poll(snapshot, work, cancel));
        let Some(node) = self.walk.node.as_ref() else { return Step::Failed(InputError::CorruptStore) };
        let Kind::Leaf(piece) = &node.kind else { unreachable!() };
        let TreeTarget::Break(nth) = self.walk.target else { unreachable!() };
        let query =
            self.storage.get_or_insert_with(|| StorageLookup::new(StorageTarget::Start(piece.first_line + nth)));
        let start = ready!(query.poll(&piece.storage, work, cancel));
        match start.checked_sub(piece.start).and_then(|at| at.checked_add(self.walk.base)) {
            Some(start) => Step::Ready(start),
            None => Step::Failed(InputError::CorruptStore),
        }
    }
}

struct ByteQuery {
    at: u64,
    walk: TreeLookup,
    credit: Credit,
}
impl ByteQuery {
    fn new(at: u64) -> Self {
        Self { at, walk: TreeLookup::new(TreeTarget::Byte(at)), credit: Credit::default() }
    }
    fn poll(&mut self, snapshot: &Snapshot, work: &mut ReadWork, cancel: &AtomicBool) -> Step<Option<u8>> {
        ready!(check(work, cancel));
        ready!(self.walk.binding.check(snapshot));
        if self.at >= snapshot.len() {
            return Step::Ready(None);
        }
        ready!(self.walk.poll(snapshot, work, cancel));
        let Kind::Leaf(piece) = &self.walk.node.as_ref().unwrap().kind else { unreachable!() };
        ready!(self.credit.poll(1, source_operations(&piece.storage) + 1, work, cancel));
        let bytes = match worker_bytes(&piece.storage) {
            Ok(bytes) => bytes,
            Err(error) => return Step::Failed(error),
        };
        Step::Ready(Some(bytes[(piece.start + self.at - self.walk.base) as usize]))
    }
}

pub struct LayoutLineQuery {
    line: u64,
    first: LayoutStartQuery,
    next: Option<LayoutStartQuery>,
    start: Option<u64>,
    end: Option<u64>,
    cr: Option<ByteQuery>,
    result: Option<LineSpan>,
}
impl LayoutLineQuery {
    pub fn new(line: u64) -> Self {
        Self { line, first: LayoutStartQuery::new(line), next: None, start: None, end: None, cr: None, result: None }
    }
    pub fn poll(&mut self, snapshot: &Snapshot, work: &mut ReadWork, cancel: &AtomicBool) -> Step<LineSpan> {
        ready!(check(work, cancel));
        ready!(self.first.walk.binding.check(snapshot));
        if let Some(result) = self.result {
            return Step::Ready(result);
        }
        if self.start.is_none() {
            self.start = Some(ready!(self.first.poll(snapshot, work, cancel)));
        }
        let start = self.start.unwrap();
        if self.line + 1 == snapshot.line_count() {
            let result =
                LineSpan { start, content_end: snapshot.len(), end: snapshot.len(), terminator: Terminator::None };
            self.result = Some(result);
            return Step::Ready(result);
        }
        if self.end.is_none() {
            let query = self.next.get_or_insert_with(|| LayoutStartQuery::new(self.line + 1));
            self.end = Some(ready!(query.poll(snapshot, work, cancel)));
        }
        let end = self.end.unwrap();
        if end <= start {
            return Step::Failed(InputError::CorruptStore);
        }
        let cr = if end > start + 1 {
            let query = self.cr.get_or_insert_with(|| ByteQuery::new(end - 2));
            ready!(query.poll(snapshot, work, cancel)) == Some(b'\r')
        } else {
            false
        };
        let result = LineSpan {
            start,
            content_end: end - if cr { 2 } else { 1 },
            end,
            terminator: if cr { Terminator::CrLf } else { Terminator::Lf },
        };
        self.result = Some(result);
        Step::Ready(result)
    }
}

pub struct LayoutRankQuery {
    at: u64,
    walk: TreeLookup,
    storage: Option<StorageLookup>,
    line: Option<u64>,
    start: Option<LayoutStartQuery>,
}
impl LayoutRankQuery {
    pub fn new(at: u64) -> Self {
        Self { at, walk: TreeLookup::new(TreeTarget::Byte(at)), storage: None, line: None, start: None }
    }
    pub fn poll(&mut self, snapshot: &Snapshot, work: &mut ReadWork, cancel: &AtomicBool) -> Step<Rank> {
        ready!(check(work, cancel));
        ready!(self.walk.binding.check(snapshot));
        if self.at > snapshot.len() {
            return Step::Failed(InputError::Range);
        }
        if self.line.is_none() {
            ready!(self.walk.poll(snapshot, work, cancel));
            self.line = if let Some(node) = &self.walk.node {
                let Kind::Leaf(piece) = &node.kind else { unreachable!() };
                let query = self.storage.get_or_insert_with(|| {
                    StorageLookup::new(StorageTarget::Rank(piece.start + self.at - self.walk.base))
                });
                let rank = ready!(query.poll(&piece.storage, work, cancel));
                Some(self.walk.breaks + rank - piece.first_line)
            } else {
                Some(0)
            };
        }
        let line = self.line.unwrap();
        let query = self.start.get_or_insert_with(|| LayoutStartQuery::new(line));
        let start = ready!(query.poll(snapshot, work, cancel));
        Step::Ready(Rank { line, column: self.at - start })
    }
}

/// Reuses source traversal, detached classification, decoding replay and the
/// eight-credit Original check unchanged. The owning adapter supplies both
/// admitted heap windows. Dropping this !Send query settles its extra cursor
/// pin on the same worker; the invocation's exact Snapshot owner is untouched.
pub struct LayoutReadQuery {
    at: u64,
    length: usize,
    binding: Binding,
    cursor: Option<WorkerSourceCursor>,
    credit: Credit,
}
impl LayoutReadQuery {
    pub fn new(at: u64, length: usize) -> Self {
        Self { at, length, binding: Binding::default(), cursor: None, credit: Credit::default() }
    }
    pub fn extra_resident_bytes() -> usize {
        WorkerSourceCursor::required_bytes()
    }
    pub fn temporary_handles() -> usize {
        WorkerSourceCursor::required_temporary_handles()
    }
    pub fn poll(
        &mut self,
        snapshot: &Snapshot,
        logical: &mut [u8],
        original: &mut [u8],
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<Option<WorkerSourceChunk>> {
        ready!(check(work, cancel));
        ready!(self.binding.check(snapshot));
        if logical.len() != self.length
            || self.length > super::ADD_BLOCK
            || self.at.checked_add(self.length as u64).is_none_or(|end| end > snapshot.len())
        {
            return Step::Failed(InputError::Range);
        }
        if self.cursor.is_none() {
            ready!(self.credit.poll(size_of::<Snapshot>() as u64, 1, work, cancel));
            self.cursor = match WorkerSourceCursor::try_new(
                snapshot.clone(),
                self.at..self.at + self.length as u64,
                |_| Ok(true),
            ) {
                Ok(cursor) => Some(cursor),
                Err(error) => return Step::Failed(cursor_error(error.error)),
            };
        }
        let credit = &mut self.credit;
        let result = self.cursor.as_mut().unwrap().fill(logical, original, cancel, |n| credit.admit(n, work, cancel));
        if cancel.load(Ordering::Relaxed) {
            return Step::Cancelled;
        }
        match result {
            Ok(WorkerSourceStep::Ready(value)) => Step::Ready(value),
            Ok(WorkerSourceStep::Pending) => Step::Pending,
            Err(WorkerSourceError::Cancelled) => Step::Cancelled,
            Err(error) => Step::Failed(cursor_error(error)),
        }
    }
}
impl Drop for LayoutReadQuery {
    fn drop(&mut self) {
        if let Some(cursor) = self.cursor.take() {
            drop(cursor.into_snapshot());
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutProvenance {
    pub source: Range<u64>,
    pub opaque: bool,
}
enum Decode {
    Locate(ResumeBoundary),
    Atom { at: u64, raw: u64, decoder: ResumeAtom },
}
pub struct LayoutProvenanceQuery {
    at: u64,
    walk: TreeLookup,
    decoded: Option<Decode>,
    backward: Option<u64>,
    forward: Option<u64>,
    credit: Credit,
    result: Option<LayoutProvenance>,
    opaque_phase: u8,
}
impl LayoutProvenanceQuery {
    pub fn new(at: u64) -> Self {
        Self {
            at,
            walk: TreeLookup::new(TreeTarget::Byte(at)),
            decoded: None,
            backward: None,
            forward: None,
            credit: Credit::default(),
            result: None,
            opaque_phase: 0,
        }
    }
    pub fn poll(&mut self, snapshot: &Snapshot, work: &mut ReadWork, cancel: &AtomicBool) -> Step<LayoutProvenance> {
        ready!(check(work, cancel));
        ready!(self.walk.binding.check(snapshot));
        if self.at >= snapshot.len() {
            return Step::Failed(InputError::Range);
        }
        if let Some(result) = &self.result {
            return Step::Ready(result.clone());
        }
        ready!(self.walk.poll(snapshot, work, cancel));
        let Kind::Leaf(piece) = &self.walk.node.as_ref().unwrap().kind else { unreachable!() };
        let local = piece.start + self.at - self.walk.base;
        let result = match piece.storage.as_ref() {
            Storage::Decoded(source) => {
                if self.decoded.is_none() {
                    self.decoded = Some(Decode::Locate(ResumeBoundary::new(&source.origin, local)));
                }
                let boundary = loop {
                    let credit = &mut self.credit;
                    let outcome = match self.decoded.as_mut().unwrap() {
                        Decode::Locate(query) => {
                            query.poll(&source.origin, cancel, &mut |n| credit.admit(n, work, cancel))
                        }
                        Decode::Atom { at, raw, decoder } => {
                            let end = source.origin.raw_bytes().len().min(*raw as usize + super::ADD_BLOCK);
                            decoder
                                .poll(&source.origin.raw_bytes()[*raw as usize..end], cancel, &mut |n| {
                                    credit.admit(n, work, cancel)
                                })
                                .map(|result| {
                                    result.map(|size| Boundary::Interior {
                                        logical: *at..*at + size.logical as u64,
                                        raw: *raw..*raw + size.raw as u64,
                                        kind: size.kind,
                                    })
                                })
                        }
                    };
                    if cancel.load(Ordering::Relaxed) {
                        return Step::Cancelled;
                    }
                    let boundary = match outcome {
                        Ok(Some(boundary)) => boundary,
                        Ok(None) => return Step::Pending,
                        Err(error) => return Step::Failed(backing_error(error)),
                    };
                    match boundary {
                        Boundary::Exact { logical, raw } => {
                            self.decoded = Some(Decode::Atom {
                                at: logical,
                                raw,
                                decoder: ResumeAtom::new(source.origin.interpretation().encoding),
                            })
                        }
                        other => break other,
                    }
                };
                let Boundary::Interior { logical, kind, .. } = boundary else { unreachable!() };
                // A source slice may contain only part of a preserved text
                // atom. It has no authority to invent an opaque raw endpoint.
                let start = logical.start.max(piece.start);
                let end = logical.end.min(piece.end);
                LayoutProvenance {
                    source: self.walk.base + start - piece.start..self.walk.base + end - piece.start,
                    opaque: kind == AtomKind::Opaque && start == logical.start && end == logical.end,
                }
            }
            Storage::Opaque { logical, offsets, .. } => {
                if self.opaque_phase == 0 {
                    let mut start = self.backward.unwrap_or(local);
                    loop {
                        ready!(self.credit.poll(4, 1, work, cancel));
                        let table = match offsets.bytes() {
                            Ok(bytes) => bytes,
                            Err(error) => return Step::Failed(read_error(error)),
                        };
                        if super::offset_entry(table, start) & super::ATOM_INTERIOR == 0 || start == piece.start {
                            break;
                        }
                        start -= 1;
                        self.backward = Some(start);
                    }
                    self.backward = Some(start);
                    self.opaque_phase = 1;
                }
                if self.opaque_phase == 1 {
                    let mut end = self.forward.unwrap_or(local + 1);
                    loop {
                        ready!(self.credit.poll(4, 1, work, cancel));
                        let table = match offsets.bytes() {
                            Ok(bytes) => bytes,
                            Err(error) => return Step::Failed(read_error(error)),
                        };
                        if super::offset_entry(table, end) & super::ATOM_INTERIOR == 0 || end == piece.end {
                            break;
                        }
                        end += 1;
                        self.forward = Some(end);
                    }
                    self.forward = Some(end);
                    self.opaque_phase = 2;
                }
                let start = self.backward.unwrap();
                let end = self.forward.unwrap();
                ready!(self.credit.poll(1, 1, work, cancel));
                let bytes = match logical.bytes() {
                    Ok(bytes) => bytes,
                    Err(error) => return Step::Failed(read_error(error)),
                };
                LayoutProvenance {
                    source: self.walk.base + start - piece.start..self.walk.base + end - piece.start,
                    opaque: bytes[start as usize] == 0xff,
                }
            }
            _ => LayoutProvenance { source: self.walk.base..self.walk.base + piece.end - piece.start, opaque: false },
        };
        self.result = Some(result.clone());
        Step::Ready(result)
    }
}

#[cfg(test)]
#[path = "worker_layout_tests.rs"]
mod tests;
