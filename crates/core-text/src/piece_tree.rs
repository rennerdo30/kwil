//! Persistent AVL rope. Leaves reference immutable original/add storage;
//! branches cache extensible subtree metrics. No document bytes are copied.

use crate::TextFile;
use memchr::memchr_iter;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub(crate) const ADD_BLOCK: usize = 64 * 1024;
const ARC_HEADER: usize = 2 * std::mem::size_of::<usize>();
pub(crate) type Tree = Option<Arc<Node>>;

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
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // create_new prevents collisions between processes, tests and stale files.
        let (path, file) = loop {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("kwil-add-{}-{id}.tmp", std::process::id()));
            match OpenOptions::new().read(true).write(true).create_new(true).open(&path) {
                Ok(file) => break (path, file),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        };
        // Own cleanup even when writing or mapping fails.
        let mut block = Self { map: None, file: Some(file), path };
        block.file.as_mut().unwrap().write_all(bytes)?;
        // SAFETY: this private immutable block is never changed or truncated;
        // its handle and mapping live until the last referencing piece drops.
        block.map = Some(unsafe { memmap2::Mmap::map(block.file.as_ref().unwrap())? });
        Ok(block)
    }
}

impl Drop for DiskBlock {
    fn drop(&mut self) {
        self.map.take();
        self.file.take();
        let _ = std::fs::remove_file(&self.path);
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
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Original(file) => file.bytes(),
            Self::Memory { bytes, .. } => bytes,
            Self::Disk(block) => block.map.as_ref().expect("mapped add block"),
        }
    }

    fn rank(&self, offset: u64) -> u64 {
        match self {
            Self::Original(file) => {
                file.index().expect("indexed original").line_of_offset(file.bytes(), offset).unwrap()
            }
            // Added pieces never scan more than ADD_BLOCK bytes.
            _ => memchr_iter(b'\n', &self.bytes()[..offset as usize]).count() as u64,
        }
    }

    fn line_start(&self, line: u64) -> u64 {
        match self {
            Self::Original(file) => file.index().unwrap().line_start(file.bytes(), line).unwrap(),
            _ if line == 0 => 0,
            _ => memchr_iter(b'\n', self.bytes()).nth((line - 1) as usize).unwrap() as u64 + 1,
        }
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
    start: u64,
    end: u64,
    first_line: u64,
    breaks: u64,
}

impl Piece {
    fn split(&self, at: u64) -> (Self, Self) {
        let middle = self.start + at;
        let line = self.storage.rank(middle);
        (
            Self { end: middle, breaks: line - self.first_line, ..self.clone() },
            Self { start: middle, first_line: line, breaks: self.first_line + self.breaks - line, ..self.clone() },
        )
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

pub(crate) fn split(tree: Tree, at: u64) -> (Tree, Tree) {
    let Some(node) = tree else { return (None, None) };
    if at == 0 {
        return (None, Some(node));
    }
    if at == node.metrics.bytes {
        return (Some(node), None);
    }
    match &node.kind {
        Kind::Leaf(piece) => {
            let (left, right) = piece.split(at);
            (Some(leaf(left)), Some(leaf(right)))
        }
        Kind::Branch(left, right) if at < left.metrics.bytes => {
            let (a, b) = split(Some(left.clone()), at);
            (a, join(b, Some(right.clone())))
        }
        Kind::Branch(left, right) => {
            let (a, b) = split(Some(right.clone()), at - left.metrics.bytes);
            (join(Some(left.clone()), a), b)
        }
    }
}

pub(crate) fn original(file: TextFile) -> Tree {
    if file.is_empty() {
        return None;
    }
    let len = file.len();
    let breaks = file.build_index(4).line_count() - 1;
    Some(leaf(Piece { storage: Arc::new(Storage::Original(file)), start: 0, end: len, first_line: 0, breaks }))
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
            Some(leaf(Piece { storage: Arc::new(storage), start: 0, end: chunk.len() as u64, first_line: 0, breaks })),
        );
    }
    Ok(tree)
}

/// Offset immediately after the `nth` newline (one-based).
pub(crate) fn after_break(mut node: &Node, mut nth: u64) -> u64 {
    let mut offset = 0;
    loop {
        match &node.kind {
            Kind::Leaf(piece) => return offset + piece.storage.line_start(piece.first_line + nth) - piece.start,
            Kind::Branch(left, right) if nth <= left.metrics.breaks => node = left,
            Kind::Branch(left, right) => {
                nth -= left.metrics.breaks;
                offset += left.metrics.bytes;
                node = right;
            }
        }
    }
}

pub(crate) fn rank(mut node: &Node, mut at: u64) -> u64 {
    let mut breaks = 0;
    loop {
        match &node.kind {
            Kind::Leaf(piece) => return breaks + piece.storage.rank(piece.start + at) - piece.first_line,
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
pub struct Chunks<'a> {
    stack: Vec<(&'a Node, u64)>,
    range: Range<u64>,
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
    type Item = &'a [u8];

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
                    return Some(&piece.storage.bytes()[(piece.start + start) as usize..(piece.start + end) as usize]);
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
                assert_eq!(node.metrics.breaks, piece.storage.rank(piece.end) - piece.storage.rank(piece.start));
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
