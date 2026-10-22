//! Persistent AVL rope. Leaves reference immutable original/add storage;
//! branches cache extensible subtree metrics. No document bytes are copied.

use crate::{ReadError, TextFile};
use memchr::memchr_iter;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub(crate) const ADD_BLOCK: usize = 64 * 1024;
const ARC_HEADER: usize = 2 * std::mem::size_of::<usize>();
pub(crate) type Tree = Option<Arc<Node>>;
pub(crate) type OriginalUses = BTreeMap<usize, (crate::file::OriginalRef, u64)>;

/// Shared across all versions, so keeping worker snapshots cannot bypass
/// the resident add-byte cap. The OS page cache of spilled blocks is separate.
pub(crate) struct AddBudget {
    pub limit: usize,
    pub used: AtomicUsize,
}

impl AddBudget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self { limit, used: AtomicUsize::new(0) })
    }
}

struct DiskBlock {
    map: Option<memmap2::Mmap>,
    file: Option<File>,
    path: PathBuf,
}

impl DiskBlock {
    fn new(bytes: &[u8]) -> io::Result<Self> {
        Self::new_with_write(bytes, |file, _, bytes| file.write_all(bytes))
    }

    fn new_with_write(
        bytes: &[u8],
        write: impl FnOnce(&mut File, &std::path::Path, &[u8]) -> io::Result<()>,
    ) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // create_new prevents collisions between processes, tests and stale files.
        let mut block = loop {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            #[cfg(unix)]
            let path = {
                use std::os::unix::fs::DirBuilderExt;
                let dir = std::env::temp_dir().join(format!("kwil-add-{}-{id}", std::process::id()));
                match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                    Ok(()) => dir.join("block"),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                }
            };
            // Windows temp_dir is per-user; retain its inherited ACLs.
            #[cfg(not(unix))]
            let path = std::env::temp_dir().join(format!("kwil-add-{}-{id}.tmp", std::process::id()));
            // Own the private directory before any fallible file operation.
            let mut block = Self { map: None, file: None, path };
            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&block.path) {
                Ok(file) => {
                    block.file = Some(file);
                    break block;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        };
        // Own cleanup even when writing or mapping fails.
        write(block.file.as_mut().unwrap(), &block.path, bytes)?;
        // SAFETY: this private immutable block is never changed or truncated;
        // its handle and mapping live until the last referencing piece drops.
        block.map = Some(unsafe { memmap2::Mmap::map(block.file.as_ref().unwrap())? });
        Ok(block)
    }
}

impl Drop for DiskBlock {
    fn drop(&mut self) {
        self.map.take();
        let owns_file = self.file.take().is_some();
        // A failed create_new must never delete somebody else's file.
        // Only a successfully opened file belongs to this block.
        // file.take() above closes the handle before removal on Windows.
        if owns_file {
            let _ = std::fs::remove_file(&self.path);
        }
        #[cfg(unix)]
        let _ = std::fs::remove_dir(self.path.parent().unwrap());
    }
}

enum Storage {
    Original(TextFile),
    Memory { bytes: Box<[u8]>, budget: Arc<AddBudget> },
    Disk(DiskBlock),
}

impl Drop for Storage {
    fn drop(&mut self) {
        if let Self::Memory { bytes, budget } = self {
            budget.used.fetch_sub(bytes.len(), Ordering::Relaxed);
        }
    }
}

impl Storage {
    fn bytes(&self) -> Result<&[u8], ReadError> {
        Ok(match self {
            Self::Original(file) => file.bytes()?,
            Self::Memory { bytes, .. } => bytes,
            Self::Disk(block) => block.map.as_ref().expect("mapped add block"),
        })
    }

    fn rank(&self, offset: u64) -> Result<u64, ReadError> {
        Ok(match self {
            Self::Original(file) => {
                file.index().expect("indexed original").line_of_offset(file.bytes()?, offset).unwrap()
            }
            // Added pieces never scan more than ADD_BLOCK bytes.
            _ => memchr_iter(b'\n', &self.bytes()?[..offset as usize]).count() as u64,
        })
    }

    fn line_start(&self, line: u64) -> Result<u64, ReadError> {
        Ok(match self {
            Self::Original(file) => file.index().unwrap().line_start(file.bytes()?, line).unwrap(),
            _ if line == 0 => 0,
            _ => memchr_iter(b'\n', self.bytes()?).nth((line - 1) as usize).unwrap() as u64 + 1,
        })
    }

    fn retained_bytes(&self) -> usize {
        let bytes = match self {
            Self::Memory { bytes, .. } => bytes.len(),
            Self::Disk(block) => block.path.capacity(),
            _ => 0, // Original storage is baseline; mapped add bytes are page cache.
        };
        bytes + std::mem::size_of::<Self>() + ARC_HEADER
    }
}

#[derive(Clone)]
pub(crate) struct Piece {
    storage: Arc<Storage>,
    owner: Option<Arc<()>>,
    start: u64,
    end: u64,
    first_line: u64,
    breaks: u64,
}

impl Piece {
    fn split(&self, at: u64) -> Result<(Self, Self), ReadError> {
        let middle = self.start + at;
        let line = self.storage.rank(middle)?;
        Ok((
            Self { end: middle, breaks: line - self.first_line, ..self.clone() },
            Self { start: middle, first_line: line, breaks: self.first_line + self.breaks - line, ..self.clone() },
        ))
    }
}

/// Char/UTF-16 counts can be added here without changing the tree algorithms.
#[derive(Clone, Copy)]
pub(crate) struct Metrics {
    pub bytes: u64,
    pub breaks: u64,
    pub retained: usize,
}

pub(crate) struct Node {
    pub metrics: Metrics,
    height: u32,
    kind: Kind,
}

enum Kind {
    Leaf(Piece),
    Branch(Arc<Node>, Arc<Node>),
}

fn leaf(piece: Piece) -> Arc<Node> {
    Arc::new(Node {
        metrics: Metrics {
            bytes: piece.end - piece.start,
            breaks: piece.breaks,
            retained: std::mem::size_of::<Node>() + ARC_HEADER + piece.storage.retained_bytes(),
        },
        height: 1,
        kind: Kind::Leaf(piece),
    })
}

fn branch(left: Arc<Node>, right: Arc<Node>) -> Arc<Node> {
    Arc::new(Node {
        metrics: Metrics {
            bytes: left.metrics.bytes + right.metrics.bytes,
            breaks: left.metrics.breaks + right.metrics.breaks,
            retained: left
                .metrics
                .retained
                .saturating_add(right.metrics.retained)
                .saturating_add(std::mem::size_of::<Node>() + ARC_HEADER),
        },
        height: 1 + left.height.max(right.height),
        kind: Kind::Branch(left, right),
    })
}

fn children(node: &Node) -> (&Arc<Node>, &Arc<Node>) {
    match &node.kind {
        Kind::Branch(left, right) => (left, right),
        Kind::Leaf(_) => unreachable!("rotation of leaf"),
    }
}

fn balance(left: Arc<Node>, right: Arc<Node>) -> Arc<Node> {
    if left.height > right.height + 1 {
        let (a, b) = children(&left);
        if a.height >= b.height {
            branch(a.clone(), branch(b.clone(), right))
        } else {
            let (c, d) = children(b);
            branch(branch(a.clone(), c.clone()), branch(d.clone(), right))
        }
    } else if right.height > left.height + 1 {
        let (a, b) = children(&right);
        if b.height >= a.height {
            branch(branch(left, a.clone()), b.clone())
        } else {
            let (c, d) = children(a);
            branch(branch(left, c.clone()), branch(d.clone(), b.clone()))
        }
    } else {
        branch(left, right)
    }
}

/// AVL join path-copies only the taller spine.
pub(crate) fn join(left: Tree, right: Tree) -> Tree {
    let (left, right) = match (left, right) {
        (None, right) => return right,
        (left, None) => return left,
        (Some(left), Some(right)) => (left, right),
    };
    Some(if left.height > right.height + 1 {
        let (a, b) = children(&left);
        balance(a.clone(), join(Some(b.clone()), Some(right)).unwrap())
    } else if right.height > left.height + 1 {
        let (a, b) = children(&right);
        balance(join(Some(left), Some(a.clone())).unwrap(), b.clone())
    } else {
        branch(left, right)
    })
}

pub(crate) fn split(tree: Tree, at: u64) -> Result<(Tree, Tree), ReadError> {
    let Some(node) = tree else { return Ok((None, None)) };
    if at == 0 {
        return Ok((None, Some(node)));
    }
    if at == node.metrics.bytes {
        return Ok((Some(node), None));
    }
    Ok(match &node.kind {
        Kind::Leaf(piece) => {
            let (left, right) = piece.split(at)?;
            (Some(leaf(left)), Some(leaf(right)))
        }
        Kind::Branch(left, right) if at < left.metrics.bytes => {
            let (a, b) = split(Some(left.clone()), at)?;
            (a, join(b, Some(right.clone())))
        }
        Kind::Branch(left, right) => {
            let (a, b) = split(Some(right.clone()), at - left.metrics.bytes)?;
            (join(Some(left.clone()), a), b)
        }
    })
}

pub(crate) fn original(file: TextFile, owner: Arc<()>) -> Result<Tree, ReadError> {
    let breaks = file.build_index(4)?.line_count() - 1;
    if file.is_empty() {
        return Ok(None);
    }
    let len = file.len();
    Ok(Some(leaf(Piece {
        storage: Arc::new(Storage::Original(file)),
        owner: Some(owner),
        start: 0,
        end: len,
        first_line: 0,
        breaks,
    })))
}

pub(crate) fn added(bytes: &[u8], budget: &Arc<AddBudget>) -> io::Result<Tree> {
    let mut tree = None;
    for chunk in bytes.chunks(ADD_BLOCK) {
        let reserved = budget
            .used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(chunk.len()).filter(|&next| next <= budget.limit)
            })
            .is_ok();
        let storage = if reserved {
            Storage::Memory { bytes: chunk.into(), budget: budget.clone() }
        } else {
            Storage::Disk(DiskBlock::new(chunk)?)
        };
        let breaks = memchr_iter(b'\n', chunk).count() as u64;
        tree = join(
            tree,
            Some(leaf(Piece {
                storage: Arc::new(storage),
                owner: None,
                start: 0,
                end: chunk.len() as u64,
                first_line: 0,
                breaks,
            })),
        );
    }
    Ok(tree)
}

/// Import foreign original ranges and resident add blocks into the destination
/// cap. Its own originals/add blocks and immutable spill blocks remain shared.
pub(crate) fn import_added(tree: Tree, budget: &Arc<AddBudget>, owner: &Arc<()>) -> io::Result<Tree> {
    let Some(node) = tree else { return Ok(None) };
    match &node.kind {
        Kind::Leaf(piece) => {
            let foreign = match piece.storage.as_ref() {
                Storage::Memory { budget: source, .. } => !Arc::ptr_eq(source, budget),
                Storage::Original(_) => !piece.owner.as_ref().is_some_and(|source| Arc::ptr_eq(source, owner)),
                Storage::Disk(_) => false,
            };
            if foreign {
                let bytes = piece.storage.bytes().map_err(io::Error::other)?;
                added(&bytes[piece.start as usize..piece.end as usize], budget)
            } else {
                Ok(Some(node))
            }
        }
        Kind::Branch(left, right) => {
            let a = import_added(Some(left.clone()), budget, owner)?;
            let b = import_added(Some(right.clone()), budget, owner)?;
            if a.as_ref().is_some_and(|a| Arc::ptr_eq(a, left)) && b.as_ref().is_some_and(|b| Arc::ptr_eq(b, right)) {
                Ok(Some(node))
            } else {
                Ok(join(a, b))
            }
        }
    }
}

/// Exact source byte ownership from cached metrics, with no file reads.
pub(crate) fn original_uses(tree: &Tree, range: Range<u64>) -> OriginalUses {
    let mut result = OriginalUses::new();
    let mut stack = tree.as_ref().map(|node| vec![(node.as_ref(), 0_u64)]).unwrap_or_default();
    while let Some((node, offset)) = stack.pop() {
        if offset >= range.end || offset + node.metrics.bytes <= range.start {
            continue;
        }
        match &node.kind {
            Kind::Branch(left, right) => {
                stack.push((right, offset + left.metrics.bytes));
                stack.push((left, offset));
            }
            Kind::Leaf(piece) => {
                if let Storage::Original(file) = &*piece.storage {
                    let source = file.history_backing_ref();
                    let bytes = (range.end - offset).min(node.metrics.bytes) - range.start.saturating_sub(offset);
                    result.entry(source.id()).or_insert((source, 0)).1 += bytes;
                }
            }
        }
    }
    result
}

/// Only a complete mapped original (or empty file) may become a saved baseline.
/// Arbitrary slices/composites use capped insertion rather than this shortcut.
pub(crate) fn saved_original(tree: &Tree) -> bool {
    let Some(node) = tree else { return true };
    match &node.kind {
        Kind::Leaf(piece) => match &*piece.storage {
            Storage::Original(file) => {
                piece.start == 0 && piece.end == file.len() && file.resident_original_bytes() == 0
            }
            _ => false,
        },
        _ => false,
    }
}

/// A saved baseline has one original leaf, so bind it to the document lineage
/// in O(1). Prior snapshots keep the same stable token across checkpoints.
pub(crate) fn own_saved_original(tree: Tree, owner: &Arc<()>) -> Tree {
    let node = tree?;
    match &node.kind {
        Kind::Leaf(piece) => match &*piece.storage {
            Storage::Original(_) => Some(leaf(Piece { owner: Some(owner.clone()), ..piece.clone() })),
            _ => Some(node),
        },
        _ => Some(node),
    }
}

/// Offset immediately after the `nth` newline (one-based).
pub(crate) fn after_break(mut node: &Node, mut nth: u64) -> Result<u64, ReadError> {
    let mut offset = 0;
    loop {
        match &node.kind {
            Kind::Leaf(piece) => return Ok(offset + piece.storage.line_start(piece.first_line + nth)? - piece.start),
            Kind::Branch(left, right) if nth <= left.metrics.breaks => node = left,
            Kind::Branch(left, right) => {
                nth -= left.metrics.breaks;
                offset += left.metrics.bytes;
                node = right;
            }
        }
    }
}

pub(crate) fn rank(mut node: &Node, mut at: u64) -> Result<u64, ReadError> {
    let mut breaks = 0;
    loop {
        match &node.kind {
            Kind::Leaf(piece) => return Ok(breaks + piece.storage.rank(piece.start + at)? - piece.first_line),
            Kind::Branch(left, _) if at < left.metrics.bytes => node = left,
            Kind::Branch(left, right) => {
                at -= left.metrics.bytes;
                breaks += left.metrics.breaks;
                node = right;
            }
        }
    }
}

/// Borrowed byte slices, clipped to the requested range. Scratch space is
/// O(tree height), independent of the document's length or number of lines.
/// Each item is fallible; the first original-file read error ends iteration.
pub struct Chunks<'a> {
    stack: Vec<(&'a Node, u64)>,
    range: Range<u64>,
}

/// A borrowed piece with its original-file provenance retained for encoding saves.
pub enum SourceChunk<'a> {
    Original {
        file: &'a TextFile,
        range: Range<u64>,
        bytes: &'a [u8],
    },
    /// Added bytes are UTF-8 for an encoding-aware editor.
    Added(&'a [u8]),
}

/// Read-only traversal with the same O(tree height) scratch bound as `Chunks`.
pub struct SourceChunks<'a>(Chunks<'a>);

impl<'a> SourceChunks<'a> {
    pub(crate) fn new(tree: &'a Tree, range: Range<u64>) -> Self {
        Self(Chunks::new(tree, range))
    }
}

impl<'a> Iterator for SourceChunks<'a> {
    type Item = Result<SourceChunk<'a>, ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some((node, offset)) = self.0.stack.pop() {
            if offset >= self.0.range.end || offset + node.metrics.bytes <= self.0.range.start {
                continue;
            }
            match &node.kind {
                Kind::Branch(left, right) => {
                    self.0.stack.push((right, offset + left.metrics.bytes));
                    self.0.stack.push((left, offset));
                }
                Kind::Leaf(piece) => {
                    let start = piece.start + self.0.range.start.saturating_sub(offset);
                    let end = piece.start + (self.0.range.end - offset).min(node.metrics.bytes);
                    let result = piece.storage.bytes().map(|bytes| {
                        let bytes = &bytes[start as usize..end as usize];
                        match piece.storage.as_ref() {
                            Storage::Original(file) => SourceChunk::Original { file, range: start..end, bytes },
                            _ => SourceChunk::Added(bytes),
                        }
                    });
                    if result.is_err() {
                        self.0.stack.clear();
                    }
                    return Some(result);
                }
            }
        }
        None
    }
}

impl<'a> Chunks<'a> {
    pub(crate) fn new(tree: &'a Tree, range: Range<u64>) -> Self {
        let stack = if range.is_empty() {
            Vec::new()
        } else {
            tree.as_ref().map(|node| vec![(node.as_ref(), 0)]).unwrap_or_default()
        };
        Self { stack, range }
    }
}

impl<'a> Iterator for Chunks<'a> {
    type Item = Result<&'a [u8], ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some((node, offset)) = self.stack.pop() {
            if offset >= self.range.end || offset + node.metrics.bytes <= self.range.start {
                continue;
            }
            match &node.kind {
                Kind::Branch(left, right) => {
                    self.stack.push((right, offset + left.metrics.bytes));
                    self.stack.push((left, offset));
                }
                Kind::Leaf(piece) => {
                    let start = self.range.start.saturating_sub(offset);
                    let end = (self.range.end - offset).min(node.metrics.bytes);
                    let bytes = piece.storage.bytes();
                    if bytes.is_err() {
                        self.stack.clear();
                    }
                    return Some(
                        bytes.map(|bytes| &bytes[(piece.start + start) as usize..(piece.start + end) as usize]),
                    );
                }
            }
        }
        None
    }
}

#[cfg(test)]
pub(crate) fn spill_paths(tree: &Tree) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(node) = tree {
        match &node.kind {
            Kind::Leaf(piece) => {
                if let Storage::Disk(block) = piece.storage.as_ref() {
                    paths.push(block.path.clone());
                }
            }
            Kind::Branch(left, right) => {
                paths.extend(spill_paths(&Some(left.clone())));
                paths.extend(spill_paths(&Some(right.clone())));
            }
        }
    }
    paths
}

#[cfg(test)]
pub(crate) fn assert_valid(tree: &Tree) {
    if let Some(node) = tree {
        match &node.kind {
            Kind::Leaf(piece) => {
                assert_eq!(node.height, 1);
                assert_eq!(node.metrics.bytes, piece.end - piece.start);
                assert_eq!(
                    node.metrics.breaks,
                    piece.storage.rank(piece.end).unwrap() - piece.storage.rank(piece.start).unwrap()
                );
            }
            Kind::Branch(left, right) => {
                assert!(left.height.abs_diff(right.height) <= 1);
                assert_eq!(node.height, 1 + left.height.max(right.height));
                assert_eq!(node.metrics.bytes, left.metrics.bytes + right.metrics.bytes);
                assert_eq!(node.metrics.breaks, left.metrics.breaks + right.metrics.breaks);
                assert_valid(&Some(left.clone()));
                assert_valid(&Some(right.clone()));
            }
        }
    }
}

#[cfg(test)]
mod spill_tests {
    use super::*;

    #[test]
    fn spill_write_failure_removes_partial_file() {
        let mut path = None;
        let result = DiskBlock::new_with_write(b"private added text", |file, location, bytes| {
            path = Some(location.to_path_buf());
            file.write_all(&bytes[..7])?;
            assert!(location.exists());
            Err(io::Error::other("simulated spill write failure"))
        });
        assert_eq!(result.err().unwrap().to_string(), "simulated spill write failure");
        let path = path.unwrap();
        assert!(!path.exists(), "partial spill remains at {}", path.display());
        #[cfg(unix)]
        assert!(!path.parent().unwrap().exists(), "private spill directory remains");
    }

    #[cfg(unix)]
    #[test]
    fn unix_spill_file_and_directory_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let block = DiskBlock::new(b"private added text").unwrap();
        let mode = std::fs::metadata(&block.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let directory = block.path.parent().unwrap();
        let mode = std::fs::metadata(directory).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let path = block.path.clone();
        drop(block);
        assert!(!path.exists());
        assert!(!path.parent().unwrap().exists());
    }
}
