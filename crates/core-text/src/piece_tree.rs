//! Persistent AVL rope. Leaves reference immutable original/add storage;
//! branches cache extensible subtree metrics. No document bytes are copied.

use crate::decoded_backing::DecodedSnapshot;
use crate::decoded_piece::IndexedDecoded;
use crate::encoding::Encoding;
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
/// Worker disk admission charges EACH physical file, including an empty file,
/// by whole64KiB units. Never round a logical sum across distinct allocations.
pub(crate) fn worker_disk_charge(bytes: u64) -> io::Result<u64> {
    bytes
        .max(1)
        .checked_add(ADD_BLOCK as u64 - 1)
        .map(|n| n / ADD_BLOCK as u64 * ADD_BLOCK as u64)
        .ok_or_else(crate::worker_edit::overflow)
}
pub(crate) type Tree = Option<Arc<Node>>;
pub(crate) type OriginalUses = BTreeMap<(u8, usize), (SourceRef, u64)>;

/// Persistent directory of physical storage allocations used by one snapshot.
/// A split changes logical use counts, never the full allocation charge. The
/// directory does not pin storage independently: its snapshot's root owns it.
#[derive(Clone, Default)]
pub(crate) struct RetainedStorage(Option<Arc<RetainedNode>>);

struct RetainedNode {
    key: usize,
    uses: u128,
    cost: usize,
    left: RetainedStorage,
    right: RetainedStorage,
    height: u32,
    bytes: usize,
}

impl RetainedStorage {
    fn height(&self) -> u32 {
        self.0.as_ref().map_or(0, |node| node.height)
    }
    pub(crate) fn bytes(&self) -> usize {
        self.0.as_ref().map_or(0, |node| node.bytes)
    }
    fn node(key: usize, uses: u128, cost: usize, left: Self, right: Self) -> Self {
        let height = 1 + left.height().max(right.height());
        let bytes = left
            .bytes()
            .saturating_add(right.bytes())
            .saturating_add(cost)
            .saturating_add(std::mem::size_of::<RetainedNode>() + ARC_HEADER);
        Self(Some(Arc::new(RetainedNode { key, uses, cost, left, right, height, bytes })))
    }
    fn balanced(key: usize, uses: u128, cost: usize, left: Self, right: Self) -> Self {
        if left.height() > right.height() + 1 {
            let a = left.0.as_ref().unwrap();
            if a.left.height() >= a.right.height() {
                return Self::node(
                    a.key,
                    a.uses,
                    a.cost,
                    a.left.clone(),
                    Self::node(key, uses, cost, a.right.clone(), right),
                );
            }
            let b = a.right.0.as_ref().unwrap();
            return Self::node(
                b.key,
                b.uses,
                b.cost,
                Self::node(a.key, a.uses, a.cost, a.left.clone(), b.left.clone()),
                Self::node(key, uses, cost, b.right.clone(), right),
            );
        }
        if right.height() > left.height() + 1 {
            let a = right.0.as_ref().unwrap();
            if a.right.height() >= a.left.height() {
                return Self::node(
                    a.key,
                    a.uses,
                    a.cost,
                    Self::node(key, uses, cost, left, a.left.clone()),
                    a.right.clone(),
                );
            }
            let b = a.left.0.as_ref().unwrap();
            return Self::node(
                b.key,
                b.uses,
                b.cost,
                Self::node(key, uses, cost, left, b.left.clone()),
                Self::node(a.key, a.uses, a.cost, b.right.clone(), a.right.clone()),
            );
        }
        Self::node(key, uses, cost, left, right)
    }
    fn without_first(&self) -> ((usize, u128, usize), Self) {
        let node = self.0.as_ref().unwrap();
        if node.left.0.is_none() {
            return ((node.key, node.uses, node.cost), node.right.clone());
        }
        let (first, left) = node.left.without_first();
        (first, Self::balanced(node.key, node.uses, node.cost, left, node.right.clone()))
    }
    fn changed(&self, key: usize, uses: u128, cost: usize, remove: bool) -> Self {
        let Some(node) = &self.0 else {
            assert!(!remove, "retained storage must be present before removal");
            return Self::node(key, uses, cost, Self::default(), Self::default());
        };
        match key.cmp(&node.key) {
            std::cmp::Ordering::Less => Self::balanced(
                node.key,
                node.uses,
                node.cost,
                node.left.changed(key, uses, cost, remove),
                node.right.clone(),
            ),
            std::cmp::Ordering::Greater => Self::balanced(
                node.key,
                node.uses,
                node.cost,
                node.left.clone(),
                node.right.changed(key, uses, cost, remove),
            ),
            std::cmp::Ordering::Equal => {
                let remaining = if remove {
                    node.uses.checked_sub(uses).expect("retained use count")
                } else {
                    node.uses.checked_add(uses).expect("retained use count overflow")
                };
                if remaining != 0 {
                    return Self::node(key, remaining, cost, node.left.clone(), node.right.clone());
                }
                if node.left.0.is_none() {
                    return node.right.clone();
                }
                if node.right.0.is_none() {
                    return node.left.clone();
                }
                let ((key, uses, cost), right) = node.right.without_first();
                Self::balanced(key, uses, cost, node.left.clone(), right)
            }
        }
    }
    pub(crate) fn from_tree(tree: &Tree) -> Self {
        let mut result = Self::default();
        let length = tree.as_ref().map_or(0, |node| node.metrics.bytes);
        result.change_range(tree, 0..length, false);
        result
    }
    /// Traversal is clipped to the edited range. Point edits touch only the
    /// affected leaf and O(log distinct allocations) directory nodes.
    pub(crate) fn change_range(&mut self, tree: &Tree, range: Range<u64>, remove: bool) {
        fn storage(directory: &mut RetainedStorage, source: &Arc<Storage>, uses: u128, remove: bool) {
            let key = Arc::as_ptr(source) as usize;
            *directory = directory.changed(key, uses, source.allocation_bytes(), remove);
            if let Storage::Opaque { logical, raw, offsets, .. } = source.as_ref() {
                for child in [logical, raw, offsets] {
                    storage(directory, child, uses, remove);
                }
            }
        }
        fn visit(directory: &mut RetainedStorage, node: &Node, range: Range<u64>, remove: bool) {
            if range.is_empty() {
                return;
            }
            match &node.kind {
                Kind::Leaf(piece) => storage(directory, &piece.storage, u128::from(range.end - range.start), remove),
                Kind::Branch(left, right) => {
                    let split = left.metrics.bytes;
                    if range.start < split {
                        visit(directory, left, range.start..range.end.min(split), remove);
                    }
                    if range.end > split {
                        visit(directory, right, range.start.saturating_sub(split)..range.end - split, remove);
                    }
                }
            }
        }
        if let Some(root) = tree {
            visit(self, root, range, remove);
        }
    }
    pub(crate) fn extend(&mut self, other: &Self) {
        if self.0.is_none() {
            *self = other.clone();
            return;
        }
        fn visit(directory: &mut RetainedStorage, node: &RetainedNode) {
            if let Some(left) = &node.left.0 {
                visit(directory, left);
            }
            *directory = directory.changed(node.key, node.uses, node.cost, false);
            if let Some(right) = &node.right.0 {
                visit(directory, right);
            }
        }
        if let Some(root) = &other.0 {
            visit(self, root);
        }
    }

    #[cfg(test)]
    pub(crate) fn assert_matches_tree(&self, tree: &Tree) {
        // Independent mutable-map oracle for the persistent directory. It also
        // checks balance/cache invariants after removals, rotations and sharing.
        fn storage(source: &Arc<Storage>, uses: u128, expected: &mut BTreeMap<usize, (u128, usize)>) {
            expected.entry(Arc::as_ptr(source) as usize).or_insert((0, source.allocation_bytes())).0 += uses;
            if let Storage::Opaque { logical, raw, offsets, .. } = source.as_ref() {
                for child in [logical, raw, offsets] {
                    storage(child, uses, expected);
                }
            }
        }
        fn tree_uses(node: &Node, expected: &mut BTreeMap<usize, (u128, usize)>) {
            match &node.kind {
                Kind::Leaf(piece) => storage(&piece.storage, u128::from(piece.end - piece.start), expected),
                Kind::Branch(left, right) => {
                    tree_uses(left, expected);
                    tree_uses(right, expected);
                }
            }
        }
        fn directory(node: &RetainedNode, actual: &mut BTreeMap<usize, (u128, usize)>) {
            assert!(node.uses > 0);
            assert!(node.left.height().abs_diff(node.right.height()) <= 1);
            assert_eq!(node.height, 1 + node.left.height().max(node.right.height()));
            assert_eq!(
                node.bytes,
                node.left.bytes() + node.right.bytes() + node.cost + std::mem::size_of::<RetainedNode>() + ARC_HEADER
            );
            if let Some(left) = &node.left.0 {
                assert!(left.key < node.key);
                directory(left, actual);
            }
            assert!(actual.insert(node.key, (node.uses, node.cost)).is_none());
            if let Some(right) = &node.right.0 {
                assert!(right.key > node.key);
                directory(right, actual);
            }
        }
        let mut expected = BTreeMap::new();
        if let Some(root) = tree {
            tree_uses(root, &mut expected);
        }
        let mut actual = BTreeMap::new();
        if let Some(root) = &self.0 {
            directory(root, &mut actual);
        }
        assert_eq!(actual, expected);
    }
}

#[path = "spill_retirement.rs"]
mod spill_retirement;
pub use spill_retirement::{SpillRetirement, SpillRetirementLimits, SpillRetirementReport};

#[path = "legacy_raw.rs"]
mod legacy_raw;

#[derive(Clone)]
pub(crate) enum SourceRef {
    Raw(crate::file::OriginalRef),
    Decoded { source: std::sync::Weak<IndexedDecoded>, cost: usize },
}

impl SourceRef {
    fn id(&self) -> (u8, usize) {
        match self {
            Self::Raw(source) => (0, source.id()),
            Self::Decoded { source, .. } => (1, source.as_ptr() as usize),
        }
    }
    pub fn retained_bytes(&self) -> usize {
        match self {
            Self::Raw(source) => source.retained_bytes(),
            Self::Decoded { source, cost } => {
                if source.strong_count() == 0 {
                    0
                } else {
                    *cost
                }
            }
        }
    }
}

/// Shared across all versions, so keeping worker snapshots cannot bypass
/// the resident add-byte cap. The OS page cache of spilled blocks is separate.
pub(crate) struct AddBudget {
    pub limit: AtomicUsize,
    pub used: AtomicUsize,
    owner: Arc<()>,
    #[cfg(test)]
    import_streams: AtomicUsize,
    #[cfg(test)]
    import_atoms: AtomicUsize,
    #[cfg(test)]
    import_raw_bytes: AtomicU64,
    #[cfg(test)]
    import_raw_end: AtomicU64,
    #[cfg(test)]
    import_raw_repeated: AtomicU64,
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct ImportWork {
    pub streams: usize,
    pub atoms: usize,
    pub raw_bytes: u64,
    pub repeated_raw_bytes: u64,
}

impl AddBudget {
    pub fn new(limit: usize, owner: Arc<()>) -> Arc<Self> {
        Arc::new(Self {
            limit: AtomicUsize::new(limit),
            used: AtomicUsize::new(0),
            owner,
            #[cfg(test)]
            import_streams: AtomicUsize::new(0),
            #[cfg(test)]
            import_atoms: AtomicUsize::new(0),
            #[cfg(test)]
            import_raw_bytes: AtomicU64::new(0),
            #[cfg(test)]
            import_raw_end: AtomicU64::new(0),
            #[cfg(test)]
            import_raw_repeated: AtomicU64::new(0),
        })
    }

    /// One-way activation after spill-only replay. Keep the original shared
    /// accounting object: snapshots/importers cannot acquire independent caps.
    pub fn enable_resident_additions(&self, limit: usize) -> io::Result<()> {
        if limit == 0 || self.used.load(Ordering::Acquire) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "resident additions require zero-cap unused budget",
            ));
        }
        self.limit
            .compare_exchange(0, limit, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "resident add budget was already configured"))
    }

    fn observe_import_atom(&self, _raw: &Range<u64>) {
        #[cfg(test)]
        {
            self.import_atoms.fetch_add(1, Ordering::Relaxed);
            self.import_raw_bytes.fetch_add(_raw.end - _raw.start, Ordering::Relaxed);
            let previous = self.import_raw_end.fetch_max(_raw.end, Ordering::Relaxed);
            self.import_raw_repeated
                .fetch_add(previous.saturating_sub(_raw.start).min(_raw.end - _raw.start), Ordering::Relaxed);
        }
    }

    #[cfg(test)]
    pub fn import_work(&self) -> ImportWork {
        ImportWork {
            streams: self.import_streams.load(Ordering::Relaxed),
            atoms: self.import_atoms.load(Ordering::Relaxed),
            raw_bytes: self.import_raw_bytes.load(Ordering::Relaxed),
            repeated_raw_bytes: self.import_raw_repeated.load(Ordering::Relaxed),
        }
    }
}

pub(crate) struct DiskBlock {
    map: Option<memmap2::Mmap>,
    file: Option<File>,
    path: PathBuf,
    worker_file_owned: bool,
    worker_directory_owned: bool,
    payload_len: usize,
    // Once leased, only an explicit worker retirement may unlink this block.
    // Worker shutdown must not hand deletion back to a surviving UI snapshot.
    explicit_retirement: std::sync::atomic::AtomicBool,
    retirement_leased: std::sync::atomic::AtomicBool,
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
            let mut block = Self {
                map: None,
                file: None,
                path,
                worker_file_owned: false,
                worker_directory_owned: cfg!(unix),
                payload_len: bytes.len(),
                explicit_retirement: std::sync::atomic::AtomicBool::new(false),
                retirement_leased: std::sync::atomic::AtomicBool::new(false),
            };
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
                    block.worker_file_owned = true;
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
        if self.explicit_retirement.load(Ordering::Relaxed) {
            return;
        }
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

pub(crate) enum Storage {
    Original(TextFile),
    Decoded(Arc<IndexedDecoded>),
    Memory {
        bytes: Box<[u8]>,
        budget: Arc<AddBudget>,
    },
    Disk(DiskBlock),
    /// Bounded detached text/opaque/preserved-valid block. Each payload/table
    /// is <= ADD_BLOCK and uses the destination budget or immutable spill path.
    Opaque {
        encoding: Encoding,
        logical: Arc<Storage>,
        raw: Arc<Storage>,
        offsets: Arc<Storage>,
    },
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
            Self::Decoded(source) => source.origin.logical_bytes(),
            Self::Memory { bytes, .. } => bytes,
            Self::Disk(block) => block.map.as_ref().expect("mapped add block"),
            Self::Opaque { logical, .. } => logical.bytes()?,
        })
    }

    fn rank(&self, offset: u64) -> Result<u64, ReadError> {
        Ok(match self {
            Self::Original(file) => {
                file.index().expect("indexed original").line_of_offset(file.bytes()?, offset).unwrap()
            }
            Self::Decoded(source) => source.index.line_of_offset(source.origin.logical_bytes(), offset).unwrap(),
            // Added pieces never scan more than ADD_BLOCK bytes.
            _ => memchr_iter(b'\n', &self.bytes()?[..offset as usize]).count() as u64,
        })
    }

    fn line_start(&self, line: u64) -> Result<u64, ReadError> {
        Ok(match self {
            Self::Original(file) => file.index().unwrap().line_start(file.bytes()?, line).unwrap(),
            Self::Decoded(source) => source.index.line_start(source.origin.logical_bytes(), line).unwrap(),
            _ if line == 0 => 0,
            _ => memchr_iter(b'\n', self.bytes()?).nth((line - 1) as usize).unwrap() as u64 + 1,
        })
    }

    fn retained_bytes(&self) -> usize {
        let bytes = match self {
            Self::Memory { bytes, .. } => bytes.len(),
            Self::Disk(block) => block.path.capacity(),
            Self::Opaque { logical, raw, offsets, .. } => {
                logical.retained_bytes() + raw.retained_bytes() + offsets.retained_bytes()
            }
            _ => 0, // Original storage is baseline; mapped add bytes are page cache.
        };
        bytes + std::mem::size_of::<Self>() + ARC_HEADER
    }

    // Opaque children have their own identities and charges in the directory.
    fn allocation_bytes(&self) -> usize {
        let bytes = match self {
            Self::Memory { bytes, .. } => bytes.len(),
            Self::Disk(block) => block.path.capacity(),
            _ => 0,
        };
        bytes.saturating_add(std::mem::size_of::<Self>() + ARC_HEADER)
    }

    fn resources(&self) -> StorageResources {
        match self {
            Self::Memory { .. } => StorageResources { blocks: 1, ..StorageResources::default() },
            Self::Disk(_) => StorageResources { blocks: 1, spilled_blocks: 1, ..StorageResources::default() },
            Self::Opaque { logical, raw, offsets, .. } => {
                logical.resources().plus(raw.resources()).plus(offsets.resources())
            }
            _ => StorageResources::default(),
        }
    }
}

/// Conservative add-storage resource accounting. Shared pieces can charge a
/// block more than once; original-file baseline maps are excluded. Counts are
/// cached, so inspecting them never walks a file or opens a handle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StorageResources {
    pub tree_nodes: usize,
    pub blocks: usize,
    pub spilled_blocks: usize,
}
impl StorageResources {
    fn plus(self, other: Self) -> Self {
        Self {
            tree_nodes: self.tree_nodes.saturating_add(other.tree_nodes),
            blocks: self.blocks.saturating_add(other.blocks),
            spilled_blocks: self.spilled_blocks.saturating_add(other.spilled_blocks),
        }
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
    pub resources: StorageResources,
}

pub(crate) struct Node {
    pub metrics: Metrics,
    height: u32,
    kind: Kind,
    affinity: Affinity,
}

enum Affinity {
    Free,
    Owned(Arc<()>),
    Mixed,
}
impl Affinity {
    fn accepts(&self, owner: &Arc<()>) -> bool {
        match self {
            Self::Free => true,
            Self::Owned(source) => Arc::ptr_eq(source, owner),
            Self::Mixed => false,
        }
    }
    fn joined(a: &Self, b: &Self) -> Self {
        match (a, b) {
            (Self::Free, Self::Free) => Self::Free,
            (Self::Owned(a), Self::Free) | (Self::Free, Self::Owned(a)) => Self::Owned(a.clone()),
            (Self::Owned(a), Self::Owned(b)) if Arc::ptr_eq(a, b) => Self::Owned(a.clone()),
            _ => Self::Mixed,
        }
    }
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
            retained: std::mem::size_of::<Node>() + ARC_HEADER,
            resources: piece
                .storage
                .resources()
                .plus(StorageResources { tree_nodes: 1, ..StorageResources::default() }),
        },
        height: 1,
        affinity: piece.owner.as_ref().map_or(Affinity::Free, |owner| Affinity::Owned(owner.clone())),
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
            resources: left
                .metrics
                .resources
                .plus(right.metrics.resources)
                .plus(StorageResources { tree_nodes: 1, ..StorageResources::default() }),
        },
        height: 1 + left.height.max(right.height),
        affinity: Affinity::joined(&left.affinity, &right.affinity),
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
        let storage = capped_block(chunk, budget)?;
        let owner = matches!(&*storage, Storage::Memory { .. }).then(|| budget.owner.clone());
        let breaks = memchr_iter(b'\n', chunk).count() as u64;
        tree =
            join(tree, Some(leaf(Piece { storage, owner, start: 0, end: chunk.len() as u64, first_line: 0, breaks })));
    }
    Ok(tree)
}

fn capped_block(bytes: &[u8], budget: &Arc<AddBudget>) -> io::Result<Arc<Storage>> {
    assert!(bytes.len() <= ADD_BLOCK);
    let reserved = budget
        .used
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
            used.checked_add(bytes.len()).filter(|&next| next <= budget.limit.load(Ordering::Acquire))
        })
        .is_ok();
    Ok(Arc::new(if reserved {
        Storage::Memory { bytes: bytes.into(), budget: budget.clone() }
    } else {
        Storage::Disk(DiskBlock::new(bytes)?)
    }))
}

fn opaque_run(encoding: Encoding, raw: &[u8], offsets: &[u8], budget: &Arc<AddBudget>) -> io::Result<Tree> {
    // The offset table includes both endpoints. It is itself budgeted payload,
    // never an uncharged Vec retained by a snapshot.
    assert!(offsets.len().is_multiple_of(4) && offsets.len() >= 8 && offsets.len() <= ADD_BLOCK);
    let count = offsets.len() / 4 - 1;
    detached_block(encoding, &vec![0xff; count], raw, offsets, budget)
}

/// Validate external detached data before entering the internal trusted block
/// constructors. One FF byte denotes one opaque atom, never replacement text.
pub(crate) fn validate_recovery_opaque(raw: &[u8], offsets: &[u8], count: u64) -> io::Result<()> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid recovery opaque block");
    let count = usize::try_from(count).map_err(|_| invalid())?;
    if count == 0
        || count >= ADD_BLOCK / 4
        || raw.is_empty()
        || raw.len() > ADD_BLOCK
        || offsets.len() != (count + 1) * 4
        || offsets.len() > ADD_BLOCK
    {
        return Err(invalid());
    }
    let mut previous = None;
    for bytes in offsets.as_chunks::<4>().0 {
        let at = u32::from_le_bytes(*bytes) as usize;
        if at > raw.len() || previous.is_none() && at != 0 || previous.is_some_and(|before| at <= before) {
            return Err(invalid());
        }
        previous = Some(at);
    }
    if previous != Some(raw.len()) {
        return Err(invalid());
    }
    Ok(())
}

pub(crate) fn recovery_opaque(
    encoding: Encoding,
    raw: &[u8],
    offsets: &[u8],
    count: u64,
    budget: &Arc<AddBudget>,
) -> io::Result<Tree> {
    validate_recovery_opaque(raw, offsets, count)?;
    opaque_run(encoding, raw, offsets, budget)
}

/// Validate bounded complete valid atoms before scratch growth or file creation.
/// Byte-at-a-time decode prevents forged atom-interior flags across scalars.
pub(crate) fn validate_recovery_preserved(
    encoding: Encoding,
    bytes: &[u8],
    raw: &[u8],
    offsets: &[u8],
    check: &mut impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid preserved valid atoms");
    check()?;
    if bytes.is_empty()
        || bytes.len() >= ADD_BLOCK / 4
        || raw.is_empty()
        || raw.len() > ADD_BLOCK
        || offsets.len() != (bytes.len() + 1) * 4
        || offsets.len() > ADD_BLOCK
        || std::str::from_utf8(bytes).is_err()
        || !atom_boundary(offsets, 0)
        || !atom_boundary(offsets, bytes.len())
    {
        return Err(invalid());
    }
    let base = offset_at(offsets, 0);
    if offset_at(offsets, bytes.len() as u64).checked_sub(base) != Some(raw.len())
        || offset_at(offsets, bytes.len() as u64) > ADD_BLOCK
    {
        return Err(invalid());
    }
    let codec = encoding_rs::Encoding::for_label_no_replacement(encoding.name().as_bytes()).ok_or_else(invalid)?;
    if codec == encoding_rs::ISO_2022_JP || codec == encoding_rs::REPLACEMENT {
        return Err(invalid());
    }
    let mut first = 0;
    while first < bytes.len() {
        check()?;
        let start = offset_at(offsets, first as u64);
        if start < base || start - base >= raw.len() || !atom_boundary(offsets, first) {
            return Err(invalid());
        }
        let mut last = first + 1;
        while last < bytes.len() && !atom_boundary(offsets, last) {
            if offset_at(offsets, last as u64) != start || last - first >= 8 {
                return Err(invalid());
            }
            last += 1;
        }
        let end = offset_at(offsets, last as u64);
        if !atom_boundary(offsets, last)
            || end <= start
            || end - start > 4
            || end > base + raw.len()
            || last - first > 8
        {
            return Err(invalid());
        }
        let atom_raw = &raw[start - base..end - base];
        let mut decoder = codec.new_decoder_without_bom_handling();
        let mut scratch = [0u8; 16];
        for (i, byte) in atom_raw.iter().enumerate() {
            check()?;
            let final_byte = i + 1 == atom_raw.len();
            let (result, read, written) =
                decoder.decode_to_utf8_without_replacement(std::slice::from_ref(byte), &mut scratch, final_byte);
            if !matches!(result, encoding_rs::DecoderResult::InputEmpty)
                || read != 1
                || !final_byte && written != 0
                || final_byte && scratch[..written] != bytes[first..last]
            {
                return Err(invalid());
            }
        }
        first = last;
    }
    check()
}

pub(crate) fn recovery_preserved(
    encoding: Encoding,
    bytes: &[u8],
    raw: &[u8],
    offsets: &[u8],
    budget: &Arc<AddBudget>,
    check: &mut impl FnMut() -> io::Result<()>,
) -> io::Result<Tree> {
    validate_recovery_preserved(encoding, bytes, raw, offsets, check)?;
    let base = offset_at(offsets, 0) as u32;
    // Caller admits this transient table (<= ADD_BLOCK) with recovery work.
    let mut rebased = Vec::with_capacity(offsets.len());
    for entry in offsets.as_chunks::<4>().0 {
        let entry = u32::from_le_bytes(*entry);
        rebased.extend_from_slice(&(((entry & !ATOM_INTERIOR) - base) | (entry & ATOM_INTERIOR)).to_le_bytes());
    }
    check()?;
    let logical = capped_block(bytes, budget)?;
    check()?;
    let raw = capped_block(raw, budget)?;
    check()?;
    let offsets = capped_block(&rebased, budget)?;
    check()?;
    let resident = [&logical, &raw, &offsets].iter().any(|storage| matches!(&***storage, Storage::Memory { .. }));
    Ok(Some(leaf(Piece {
        storage: Arc::new(Storage::Opaque { encoding, logical, raw, offsets }),
        owner: resident.then(|| budget.owner.clone()),
        start: 0,
        end: bytes.len() as u64,
        first_line: 0,
        breaks: memchr_iter(b'\n', bytes).count() as u64,
    })))
}

/// Conservative transient AVL allocation reservation for two splits and joins.
/// Every recursive join allocates at most a constant number of branch nodes;
/// split's joins visit at most height squared nodes. Original storage/index
/// allocations are charged separately, once when their registry entry is built.
pub(crate) fn recovery_tree_work(tree: &Tree) -> Option<usize> {
    let height = tree.as_ref().map_or(0, |node| node.height as usize).checked_add(1)?;
    height.checked_mul(height)?.checked_mul(64)?.checked_mul(std::mem::size_of::<Node>() + ARC_HEADER)
}

pub(crate) fn recovery_block_work(blocks: usize) -> Option<usize> {
    // PathBuf growth, platform temp prefix and maximum decimal PID/counter
    // suffix are reserved before any spill path/storage allocation.
    let paths = std::env::temp_dir().as_os_str().len().checked_add(128)?.checked_mul(4)?;
    blocks.checked_mul(paths.checked_add(4 * (std::mem::size_of::<Storage>() + ARC_HEADER))?)
}

pub(crate) fn recovery_validate_range(
    origin: &DecodedSnapshot,
    range: Range<u64>,
    check: &mut impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    use crate::decoded_backing::{AtomKind, Boundary};
    if range.start > range.end || range.end > origin.logical_bytes().len() as u64 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "range outside recovery origin"));
    }
    for at in [range.start, range.end] {
        check()?;
        let boundary = origin.resolve_logical(at).map_err(io::Error::other)?;
        check()?;
        if let Boundary::Interior { logical, kind, .. } = boundary
            && (kind != AtomKind::Text
                || !std::str::from_utf8(&origin.logical_bytes()[logical.start as usize..logical.end as usize])
                    .is_ok_and(|text| text.is_char_boundary((at - logical.start) as usize)))
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "recovery range splits a decoded scalar"));
        }
    }
    Ok(())
}

pub(crate) fn recovery_cut(tree: &Tree, at: u64, check: &mut impl FnMut() -> io::Result<()>) -> io::Result<()> {
    let mut node = tree.as_deref();
    let mut at = at;
    while let Some(current) = node {
        if at == 0 || at == current.metrics.bytes {
            return Ok(());
        }
        match &current.kind {
            Kind::Branch(left, right) => {
                if at < left.metrics.bytes {
                    node = Some(left);
                } else {
                    at -= left.metrics.bytes;
                    node = Some(right);
                }
            }
            Kind::Leaf(piece) => {
                if let Storage::Decoded(source) = &*piece.storage {
                    recovery_validate_range(&source.origin, piece.start + at..piece.start + at, check)?;
                } else if let Storage::Opaque { logical, offsets, .. } = &*piece.storage {
                    check()?;
                    validate_detached_endpoint(
                        logical.bytes().map_err(io::Error::other)?,
                        offsets.bytes().map_err(io::Error::other)?,
                        (piece.start + at) as usize,
                    )?;
                    check()?;
                }
                return Ok(());
            }
        }
    }
    Ok(())
}

fn detached_block(
    encoding: Encoding,
    bytes: &[u8],
    raw: &[u8],
    offsets: &[u8],
    budget: &Arc<AddBudget>,
) -> io::Result<Tree> {
    if raw.is_empty() {
        return added(bytes, budget);
    }
    assert!(!bytes.is_empty() && bytes.len() < ADD_BLOCK / 4);
    assert_eq!(offsets.len(), (bytes.len() + 1) * 4);
    assert_eq!(offset_at(offsets, 0), 0);
    assert_eq!(offset_at(offsets, bytes.len() as u64), raw.len());
    let logical = capped_block(bytes, budget)?;
    let raw = capped_block(raw, budget)?;
    let offsets = capped_block(offsets, budget)?;
    let resident = [&logical, &raw, &offsets].iter().any(|storage| matches!(&***storage, Storage::Memory { .. }));
    Ok(Some(leaf(Piece {
        storage: Arc::new(Storage::Opaque { encoding, logical, raw, offsets }),
        owner: resident.then(|| budget.owner.clone()),
        start: 0,
        end: bytes.len() as u64,
        first_line: 0,
        breaks: memchr_iter(b'\n', bytes).count() as u64,
    })))
}

const ATOM_INTERIOR: u32 = 1 << 31;

fn offset_entry(offsets: &[u8], at: u64) -> u32 {
    let at = usize::try_from(at).expect("bounded opaque logical offset");
    let bytes = offsets.get(at * 4..at * 4 + 4).expect("opaque endpoint table");
    u32::from_le_bytes(bytes.try_into().unwrap())
}

fn offset_at(offsets: &[u8], at: u64) -> usize {
    (offset_entry(offsets, at) & !ATOM_INTERIOR) as usize
}

fn atom_boundary(offsets: &[u8], at: usize) -> bool {
    offset_entry(offsets, at as u64) & ATOM_INTERIOR == 0
}

fn validate_detached_endpoint(bytes: &[u8], table: &[u8], at: usize) -> io::Result<()> {
    if !atom_boundary(table, at) {
        let mut first = at;
        let mut last = at;
        while !atom_boundary(table, first) {
            first -= 1;
        }
        while !atom_boundary(table, last) {
            last += 1;
        }
        if !std::str::from_utf8(&bytes[first..last]).is_ok_and(|text| text.is_char_boundary(at - first)) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "import splits a preserved scalar"));
        }
    }
    Ok(())
}

/// One bounded packing buffer across all short valid/opaque runs. Valid-only
/// blocks can fill ADD_BLOCK; mixed blocks leave room for a u32 per logical
/// byte plus the final endpoint. The table stays flat over valid UTF-8 bytes.
type ImportAdmission<'a> = &'a mut dyn FnMut(usize, &Tree) -> io::Result<()>;

struct ImportBuilder<'a> {
    encoding: Encoding,
    budget: &'a Arc<AddBudget>,
    logical: Vec<u8>,
    raw: Vec<u8>,
    offsets: Vec<u8>,
    opaque_atoms: usize,
    encoder: encoding_rs::Encoder,
    tree: Tree,
    allocation: Option<&'a mut WorkerAllocation>,
    worker_cancel: Option<&'a std::sync::atomic::AtomicBool>,
    admission: Option<ImportAdmission<'a>>,
}
impl<'a> ImportBuilder<'a> {
    fn new(encoding: Encoding, budget: &'a Arc<AddBudget>) -> Self {
        Self {
            encoding,
            budget,
            logical: Vec::with_capacity(ADD_BLOCK),
            raw: Vec::with_capacity(ADD_BLOCK),
            offsets: Vec::with_capacity(ADD_BLOCK),
            opaque_atoms: 0,
            encoder: encoding_rs::Encoding::for_label_no_replacement(encoding.name().as_bytes())
                .expect("validated stateless encoding")
                .new_encoder(),
            tree: None,
            admission: None,
            allocation: None,
            worker_cancel: None,
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.logical.is_empty() {
            return Ok(());
        }
        // Recovery admits storage/path metadata and transient join paths
        // before any payload allocation or private spill file is created.
        if let Some(admit) = &mut self.admission {
            admit(if self.raw.is_empty() { 1 } else { 3 }, &self.tree)?;
        }
        let block = if let Some(allocation) = self.allocation.as_deref_mut() {
            worker_detached(
                self.encoding,
                &self.logical,
                &self.raw,
                &self.offsets,
                self.budget,
                allocation,
                self.worker_cancel.expect("worker allocation cancellation"),
            )?
        } else if self.raw.is_empty() {
            added(&self.logical, self.budget)?
        } else if self.opaque_atoms == self.logical.len() {
            opaque_run(self.encoding, &self.raw, &self.offsets, self.budget)?
        } else {
            detached_block(self.encoding, &self.logical, &self.raw, &self.offsets, self.budget)?
        };
        self.tree = join(self.tree.take(), block);
        self.logical.clear();
        self.raw.clear();
        self.offsets.clear();
        self.opaque_atoms = 0;
        Ok(())
    }
    fn text(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let limit = if self.raw.is_empty() { ADD_BLOCK } else { ADD_BLOCK / 4 - 1 };
            let count = bytes.len().min(limit - self.logical.len());
            self.logical.extend_from_slice(&bytes[..count]);
            if !self.raw.is_empty() {
                for _ in 0..count {
                    self.offsets.extend_from_slice(&(self.raw.len() as u32).to_le_bytes());
                }
            }
            bytes = &bytes[count..];
            if self.logical.len() == limit {
                self.flush()?;
            }
        }
        Ok(())
    }
    fn opaque(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.logical.len() >= ADD_BLOCK / 4 - 1 || self.raw.len() + bytes.len() > ADD_BLOCK {
            self.flush()?;
        }
        if self.raw.is_empty() {
            self.offsets.resize((self.logical.len() + 1) * 4, 0);
        }
        self.logical.push(0xff);
        self.raw.extend_from_slice(bytes);
        self.offsets.extend_from_slice(&(self.raw.len() as u32).to_le_bytes());
        self.opaque_atoms += 1;
        Ok(())
    }
    /// Keep only noncanonical valid atoms. The common canonical path retains
    /// its existing add-byte accounting; comparison uses fixed scalar scratch.
    fn original_text(&mut self, text: &str, raw: &[u8]) -> io::Result<()> {
        let mut scratch = [0u8; 32];
        let utf16 = self.encoding == Encoding::UTF16_LE || self.encoding == Encoding::UTF16_BE;
        let canonical = if self.encoding == Encoding::UTF8 || text.is_ascii() && !utf16 {
            text.as_bytes() == raw
        } else if utf16 {
            let mut used = 0;
            for unit in text.encode_utf16() {
                let bytes = if self.encoding == Encoding::UTF16_LE { unit.to_le_bytes() } else { unit.to_be_bytes() };
                scratch[used..used + 2].copy_from_slice(&bytes);
                used += 2;
            }
            &scratch[..used] == raw
        } else {
            let (result, read, written) = self.encoder.encode_from_utf8_without_replacement(text, &mut scratch, false);
            matches!(result, encoding_rs::EncoderResult::InputEmpty) && read == text.len() && &scratch[..written] == raw
        };
        if canonical { self.text(text.as_bytes()) } else { self.preserved(text.as_bytes(), raw) }
    }
    fn preserved(&mut self, bytes: &[u8], raw: &[u8]) -> io::Result<()> {
        assert!(!bytes.is_empty() && bytes.len() < ADD_BLOCK / 4 && !raw.is_empty() && raw.len() <= ADD_BLOCK);
        if self.logical.len() + bytes.len() >= ADD_BLOCK / 4 || self.raw.len() + raw.len() > ADD_BLOCK {
            self.flush()?;
        }
        if self.raw.is_empty() {
            self.offsets.resize((self.logical.len() + 1) * 4, 0);
        }
        let interior = (self.raw.len() as u32 | ATOM_INTERIOR).to_le_bytes();
        self.logical.extend_from_slice(bytes);
        self.raw.extend_from_slice(raw);
        for _ in 1..bytes.len() {
            self.offsets.extend_from_slice(&interior);
        }
        self.offsets.extend_from_slice(&(self.raw.len() as u32).to_le_bytes());
        Ok(())
    }
    fn finish(mut self) -> io::Result<Tree> {
        self.flush()?;
        Ok(self.tree)
    }
}

/// Ordinary raw-v1 storage only. Inspect provenance without allocating a
/// descriptor vector or reading a source byte; empty decoded identities are
/// checked separately by the recovery registry.
fn validate_raw_v1(tree: &Tree, check: &mut impl FnMut() -> io::Result<()>) -> io::Result<()> {
    let Some(node) = tree else { return Ok(()) };
    check()?;
    match &node.kind {
        Kind::Branch(left, right) => {
            validate_raw_v1(&Some(left.clone()), check)?;
            validate_raw_v1(&Some(right.clone()), check)
        }
        Kind::Leaf(piece) => match &*piece.storage {
            Storage::Original(_) | Storage::Memory { .. } | Storage::Disk(_) => Ok(()),
            Storage::Decoded(_) | Storage::Opaque { .. } => {
                Err(io::Error::new(io::ErrorKind::InvalidData, "raw-v1 promotion refuses typed or decoded storage"))
            }
        },
    }
}

/// Caller has admitted fixed packing/traversal scratch while retaining the
/// entire source. Each target flush is admitted before growth/file creation.
pub(crate) fn recovery_promote_raw_v1(
    source: &Tree,
    budget: &Arc<AddBudget>,
    admission: &mut dyn FnMut(usize, &Tree) -> io::Result<()>,
    check: &mut impl FnMut() -> io::Result<()>,
) -> io::Result<Tree> {
    validate_raw_v1(source, check)?;
    check()?;
    let mut builder = ImportBuilder::new(Encoding::UTF8, budget);
    builder.admission = Some(admission);
    let mut decoder = legacy_raw::RawUtf8::default();
    let len = source.as_ref().map_or(0, |node| node.metrics.bytes);
    for chunk in Chunks::new(source, 0..len) {
        let chunk = chunk.map_err(io::Error::other)?;
        for bytes in chunk.chunks(ADD_BLOCK) {
            check()?;
            decoder.feed(bytes, &mut |span| match span {
                legacy_raw::Span::Text(bytes) => builder.text(bytes),
                legacy_raw::Span::Opaque(byte) => builder.opaque(&[byte]),
            })?;
        }
    }
    check()?;
    decoder.finish(&mut |span| match span {
        legacy_raw::Span::Text(bytes) => builder.text(bytes),
        legacy_raw::Span::Opaque(byte) => builder.opaque(&[byte]),
    })?;
    check()?;
    let result = builder.finish()?;
    check()?;
    Ok(result)
}

fn import_atoms<'a>(
    origin: &'a DecodedSnapshot,
    range: Range<u64>,
    _budget: &AddBudget,
) -> io::Result<crate::decoded_backing::Atoms<'a>> {
    #[cfg(test)]
    _budget.import_streams.fetch_add(1, Ordering::Relaxed);
    origin.atoms(range).map_err(io::Error::other)
}

fn import_decoded(origin: &DecodedSnapshot, range: Range<u64>, budget: &Arc<AddBudget>) -> io::Result<Tree> {
    let covering = crate::decoded_piece::validate_range(origin, range.clone())?;
    let mut builder = ImportBuilder::new(origin.interpretation().encoding, budget);
    // Exactly one cursor: endpoint replay is constant per import, rather than
    // once for each short opaque run. Valid partial edge atoms are clipped as
    // UTF-8, never assigned an invented raw interior boundary.
    for atom in import_atoms(origin, covering, budget)? {
        let atom = atom.map_err(io::Error::other)?;
        budget.observe_import_atom(&atom.raw);
        match atom.content {
            crate::decoded_backing::AtomContent::Text(text) => {
                let clipped = atom.logical.start.max(range.start)..atom.logical.end.min(range.end);
                if clipped == atom.logical {
                    builder.original_text(text, atom.raw_bytes)?;
                } else {
                    builder.text(&origin.logical_bytes()[clipped.start as usize..clipped.end as usize])?;
                }
            }
            crate::decoded_backing::AtomContent::Opaque => builder.opaque(atom.raw_bytes)?,
        }
    }
    builder.finish()
}

/// Repack only the selected detached atoms. Partial valid edge atoms become
/// text; retaining or copying their complete raw bytes would invent endpoints.
fn feed_detached(
    builder: &mut ImportBuilder<'_>,
    bytes: &[u8],
    raw: &[u8],
    table: &[u8],
    range: Range<u64>,
    check: &mut impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    for at in [range.start as usize, range.end as usize] {
        validate_detached_endpoint(bytes, table, at)?;
    }
    let chunks = DetachedChunks {
        encoding: builder.encoding,
        bytes,
        raw,
        table,
        next: range.start as usize,
        end: range.end as usize,
    };
    for chunk in chunks {
        check()?;
        match chunk {
            SourceChunk::Added(bytes) => builder.text(bytes)?,
            SourceChunk::Opaque { raw_bytes, raw_offsets, bytes, .. } => {
                let base = offset_at(raw_offsets, 0);
                for at in 0..bytes.len() {
                    check()?;
                    builder.opaque(
                        &raw_bytes
                            [offset_at(raw_offsets, at as u64) - base..offset_at(raw_offsets, at as u64 + 1) - base],
                    )?;
                }
            }
            SourceChunk::Preserved { bytes, raw_bytes, raw_offsets, .. } => {
                let base = offset_at(raw_offsets, 0);
                let mut first = 0;
                while first < bytes.len() {
                    check()?;
                    let mut last = first + 1;
                    while !atom_boundary(raw_offsets, last) {
                        last += 1;
                    }
                    builder.preserved(
                        &bytes[first..last],
                        &raw_bytes
                            [offset_at(raw_offsets, first as u64) - base..offset_at(raw_offsets, last as u64) - base],
                    )?;
                    first = last;
                }
            }
            _ => unreachable!("detached traversal only yields detached payloads"),
        }
    }
    Ok(())
}

pub(crate) fn decoded_checked<E>(
    origin: DecodedSnapshot,
    owner: Arc<()>,
    check: &mut impl FnMut() -> Result<(), E>,
) -> Result<Tree, E> {
    if origin.logical_bytes().is_empty() {
        check()?;
        check()?;
        return Ok(None);
    }
    let source = Arc::new(IndexedDecoded::new_checked(origin, check)?);
    let end = source.origin.logical_bytes().len() as u64;
    let breaks = source.index.line_count() - 1;
    Ok(Some(leaf(Piece {
        storage: Arc::new(Storage::Decoded(source)),
        owner: Some(owner),
        start: 0,
        end,
        first_line: 0,
        breaks,
    })))
}

/// Import foreign original ranges and resident add blocks into the destination
/// cap. Its own originals/add blocks and immutable spill blocks remain shared.
pub(crate) fn import_added(tree: Tree, budget: &Arc<AddBudget>, owner: &Arc<()>) -> io::Result<Tree> {
    import_observed(tree, budget, owner, &mut || {})
}

fn import_observed(tree: Tree, budget: &Arc<AddBudget>, owner: &Arc<()>, visit: &mut impl FnMut()) -> io::Result<Tree> {
    let Some(node) = tree else { return Ok(None) };
    visit();
    // Cached lineage and capped storage ownership make an own-document move
    // O(1), including a composite root or a root retained before checkpoint.
    if node.affinity.accepts(owner) {
        return Ok(Some(node));
    }
    match &node.kind {
        Kind::Leaf(piece) => {
            if let Storage::Decoded(source) = &*piece.storage {
                return import_decoded(&source.origin, piece.start..piece.end, budget);
            }
            if let Storage::Opaque { encoding, raw, offsets, .. } = &*piece.storage {
                let mut builder = ImportBuilder::new(*encoding, budget);
                feed_detached(
                    &mut builder,
                    piece.storage.bytes().map_err(io::Error::other)?,
                    raw.bytes().map_err(io::Error::other)?,
                    offsets.bytes().map_err(io::Error::other)?,
                    piece.start..piece.end,
                    &mut || Ok(()),
                )?;
                return builder.finish();
            }
            let foreign = match piece.storage.as_ref() {
                Storage::Memory { budget: source, .. } => !Arc::ptr_eq(source, budget),
                Storage::Original(_) => !piece.owner.as_ref().is_some_and(|source| Arc::ptr_eq(source, owner)),
                Storage::Decoded(_) => !piece.owner.as_ref().is_some_and(|source| Arc::ptr_eq(source, owner)),
                Storage::Disk(_) => false,
                Storage::Opaque { .. } => unreachable!(),
            };
            if foreign {
                let bytes = piece.storage.bytes().map_err(io::Error::other)?;
                added(&bytes[piece.start as usize..piece.end as usize], budget)
            } else {
                Ok(Some(node))
            }
        }
        Kind::Branch(left, right) => {
            let a = import_observed(Some(left.clone()), budget, owner, visit)?;
            let b = import_observed(Some(right.clone()), budget, owner, visit)?;
            if a.as_ref().is_some_and(|a| Arc::ptr_eq(a, left)) && b.as_ref().is_some_and(|b| Arc::ptr_eq(b, right)) {
                Ok(Some(node))
            } else {
                Ok(join(a, b))
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn import_visit_count(tree: Tree, budget: &Arc<AddBudget>, owner: &Arc<()>) -> usize {
    let mut count = 0;
    import_observed(tree, budget, owner, &mut || count += 1).unwrap();
    count
}

/// One operation's physical allocation ledger. It is registered in a stable
/// worker depot slot before payload creation. Baseline certificates retain actual
/// storage identities; neither affinity nor a leased boolean grants coverage.
pub(crate) struct WorkerAllocation {
    plan: crate::worker_edit::WorkerEditPlan,
    blocks: Vec<Arc<Storage>>,
    baseline: Vec<Arc<Storage>>,
    decoded: Vec<WorkerDecodedCopy>,
    metadata: usize,
    external_metadata: usize,
    tree_work: usize,
    disk: u64,
    claimed: bool,
    #[cfg(test)]
    pub(crate) fault: WorkerFault,
    #[cfg(test)]
    pub(crate) creates: usize,
    #[cfg(test)]
    pub(crate) collision_path: Option<PathBuf>,
}

/// Actual ownership is registered before each creation. The weak source key
/// avoids duplicate copies without retaining any foreign decoded owner.
struct WorkerDecodedCopy {
    source: crate::decoded_backing::DecodedOriginKey,
    paths: crate::decoded_backing::BackingPaths,
    directory_owned: bool,
    files: [Option<File>; 3],
    owned: [bool; 3],
    owner: Option<crate::decoded_backing::DecodedBacking>,
    storage: Option<Arc<Storage>>,
}
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerFault {
    None,
    Write,
    Map,
    AfterOpenPanic,
    Cleanup,
    CancelAfterWrite,
    CancelAfterClaim,
    DecodedCopyCancel,
    DecodedCopyPanic,
    ClaimPrefixCancel,
    ClaimPrefixPanic,
    ResidentPanic,
}
impl WorkerAllocation {
    pub(crate) fn new(plan: crate::worker_edit::WorkerEditPlan) -> io::Result<Self> {
        use crate::worker_edit::{capacity, overflow};
        let paths = plan.max_path_bytes.checked_mul(2).ok_or_else(overflow)?;
        let descriptor = (std::mem::size_of::<Storage>() + ARC_HEADER + std::mem::size_of::<Arc<Storage>>())
            .checked_add(paths)
            .ok_or_else(overflow)?;
        let descriptors = plan.max_new_blocks.checked_mul(descriptor).ok_or_else(overflow)?;
        let certificates =
            plan.max_certificates.checked_mul(std::mem::size_of::<Arc<Storage>>()).ok_or_else(overflow)?;
        // Three files per decoded copy. Paths/index are charged per actual copy
        // before allocation; this vector's bounded capacity is charged up front.
        let copies =
            (plan.max_new_blocks / 3).checked_mul(std::mem::size_of::<WorkerDecodedCopy>()).ok_or_else(overflow)?;
        let metadata = descriptors
            .checked_add(certificates)
            .and_then(|n| n.checked_add(copies))
            .and_then(|n| {
                n.checked_add(4 * ADD_BLOCK + std::mem::size_of::<Self>() + 1024 + crate::worker_edit::slot_metadata())
            })
            .ok_or_else(overflow)?;
        let recipe_disk = if plan.recipe_bytes == 0 { 0 } else { worker_disk_charge(plan.recipe_bytes)? };
        if metadata > plan.metadata_bytes || recipe_disk > plan.disk_bytes {
            return Err(capacity());
        }
        let mut blocks = Vec::new();
        let mut baseline = Vec::new();
        let mut decoded = Vec::new();
        blocks.try_reserve_exact(plan.max_new_blocks).map_err(io::Error::other)?;
        baseline.try_reserve_exact(plan.max_certificates).map_err(io::Error::other)?;
        decoded.try_reserve_exact(plan.max_new_blocks / 3).map_err(io::Error::other)?;
        Ok(Self {
            plan,
            blocks,
            baseline,
            decoded,
            metadata,
            external_metadata: 0,
            tree_work: 0,
            disk: recipe_disk,
            claimed: false,
            #[cfg(test)]
            fault: WorkerFault::None,
            #[cfg(test)]
            creates: 0,
            #[cfg(test)]
            collision_path: None,
        })
    }
    fn charge(&mut self, bytes: usize) -> io::Result<()> {
        self.metadata = self
            .metadata
            .checked_add(bytes)
            .filter(|n| *n <= self.plan.metadata_bytes)
            .ok_or_else(crate::worker_edit::capacity)?;
        Ok(())
    }
    pub(crate) fn reserve_external(&mut self, bytes: usize) -> io::Result<()> {
        if bytes > self.external_metadata {
            self.charge(bytes - self.external_metadata)?;
            self.external_metadata = bytes;
        }
        Ok(())
    }
    pub(crate) fn charge_tree(&mut self, tree: &Tree) -> io::Result<()> {
        let nodes = tree.as_ref().map_or(0, |n| n.metrics.resources.tree_nodes);
        if nodes > self.plan.max_tree_nodes {
            return Err(crate::worker_edit::capacity());
        }
        let work = recovery_tree_work(tree)
            .and_then(|n| nodes.checked_mul(512).and_then(|d| n.checked_add(d)))
            .and_then(|n| {
                // One directory node per physical storage (including opaque
                // wrapper/children), plus path-copying scratch while building.
                let blocks = tree.as_ref().map_or(0, |root| root.metrics.resources.blocks);
                blocks
                    .checked_add(nodes)
                    .and_then(|count| count.checked_mul(2 * (std::mem::size_of::<RetainedNode>() + ARC_HEADER)))
                    .and_then(|directory| n.checked_add(directory))
            })
            .ok_or_else(crate::worker_edit::overflow)?;
        // One active core import/split/join working set. Returned/composed roots
        // and source expansion are covered by the separately reserved VM peak.
        // Charge its HIGH WATER, not every transient allocation ever performed:
        // cumulative full-tree charges would falsely make streaming quadratic.
        if work > self.tree_work {
            self.charge(work - self.tree_work)?;
            self.tree_work = work;
        }
        Ok(())
    }
    pub(crate) fn owns_storage(&self, storage: &Arc<Storage>) -> bool {
        self.blocks.iter().any(|known| Arc::ptr_eq(known, storage))
            || self.decoded.iter().any(|entry| entry.storage.as_ref().is_some_and(|known| Arc::ptr_eq(known, storage)))
    }
    fn certified(&self, storage: &Arc<Storage>) -> bool {
        self.owns_storage(storage)
            || self
                .baseline
                .binary_search_by_key(&(Arc::as_ptr(storage) as usize), |known| Arc::as_ptr(known) as usize)
                .is_ok()
    }
    fn certificate(
        &mut self,
        storage: &Arc<Storage>,
        covers: &mut impl FnMut(&Arc<Storage>) -> bool,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> io::Result<()> {
        crate::worker_edit::poll(cancelled)?;
        if self.baseline.len() >= self.plan.max_certificates {
            return Err(crate::worker_edit::capacity());
        }
        if matches!(storage.as_ref(), Storage::Disk(_)) && !covers(storage) {
            return Err(crate::worker_edit::invalid("baseline spill has no retained worker owner"));
        }
        self.baseline.push(storage.clone());
        if let Storage::Opaque { logical, raw, offsets, .. } = storage.as_ref() {
            for child in [logical, raw, offsets] {
                self.certificate(child, covers, cancelled)?;
            }
        }
        Ok(())
    }
    pub(crate) fn certify_baseline(
        &mut self,
        tree: &Tree,
        covers: &mut impl FnMut(&Arc<Storage>) -> bool,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> io::Result<()> {
        fn visit(
            a: &mut WorkerAllocation,
            node: &Node,
            covers: &mut impl FnMut(&Arc<Storage>) -> bool,
            cancelled: &std::sync::atomic::AtomicBool,
        ) -> io::Result<()> {
            crate::worker_edit::poll(cancelled)?;
            match &node.kind {
                Kind::Leaf(piece) => a.certificate(&piece.storage, covers, cancelled),
                Kind::Branch(left, right) => {
                    visit(a, left, covers, cancelled)?;
                    visit(a, right, covers, cancelled)
                }
            }
        }
        self.charge_tree(tree)?;
        if let Some(root) = tree {
            visit(self, root, covers, cancelled)?;
        }
        self.baseline.sort_unstable_by_key(|s| Arc::as_ptr(s) as usize);
        self.baseline.dedup_by(|a, b| Arc::ptr_eq(a, b));
        Ok(())
    }
    fn block(
        &mut self,
        bytes: &[u8],
        budget: &Arc<AddBudget>,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> io::Result<Arc<Storage>> {
        use crate::worker_edit::{capacity, overflow};
        crate::worker_edit::poll(cancelled)?;
        if bytes.is_empty()
            || bytes.len() > ADD_BLOCK
            || self
                .blocks
                .len()
                .checked_add(self.decoded.len().checked_mul(3).ok_or_else(overflow)?)
                .ok_or_else(overflow)?
                >= self.plan.max_new_blocks
        {
            return Err(capacity());
        }
        // Reserve worst-case spill BEFORE the resident CAS. A concurrent importer
        // can consume all resident headroom; no file creation depends on a guess.
        let spilled_disk = self
            .disk
            .checked_add(worker_disk_charge(bytes.len() as u64)?)
            .filter(|n| *n <= self.plan.disk_bytes)
            .ok_or_else(capacity)?;
        struct DiskReservation<'a> {
            used: &'a mut u64,
            previous: u64,
            keep: bool,
        }
        impl Drop for DiskReservation<'_> {
            fn drop(&mut self) {
                if !self.keep {
                    *self.used = self.previous;
                }
            }
        }
        let previous = self.disk;
        self.disk = spilled_disk;
        let mut spill = DiskReservation { used: &mut self.disk, previous, keep: false };
        let reserved = budget
            .used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes.len()).filter(|&next| next <= budget.limit.load(Ordering::Acquire))
            })
            .is_ok();
        if reserved {
            struct Reservation<'a> {
                budget: &'a AddBudget,
                len: usize,
            }
            impl Drop for Reservation<'_> {
                fn drop(&mut self) {
                    self.budget.used.fetch_sub(self.len, Ordering::Relaxed);
                }
            }
            let mut guard = Reservation { budget, len: bytes.len() };
            #[cfg(test)]
            if self.fault == WorkerFault::ResidentPanic {
                panic!("controlled resident allocation unwind");
            }
            let storage = Arc::new(Storage::Memory { bytes: bytes.into(), budget: budget.clone() });
            guard.len = 0; // Storage now owns the exact shared resident charge.
            self.blocks.push(storage.clone());
            return Ok(storage);
        }
        // From here the spill descriptor/partial file keeps the reservation
        // until explicit retirement. Resident success/error/unwind restores it.
        spill.keep = true;
        drop(spill);
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1)).map_err(|_| overflow())?;
        let name = format!("kwil-worker-{}-{id}", std::process::id());
        let private = std::env::temp_dir().join(name);
        #[cfg(unix)]
        let path = private.join("block");
        #[cfg(not(unix))]
        let path = private;
        #[cfg(test)]
        let path = self.collision_path.clone().unwrap_or(path);
        if path.as_os_str().as_encoded_bytes().len() > self.plan.max_path_bytes
            || path.capacity() > self.plan.max_path_bytes * 2
        {
            return Err(capacity());
        }
        // The exact physical Arc is registered BEFORE mkdir/open/write/map.
        // Partial descriptors never escape; explicit Drop cannot unlink them.
        self.blocks.push(Arc::new(Storage::Disk(DiskBlock {
            map: None,
            file: None,
            path,
            worker_file_owned: false,
            worker_directory_owned: false,
            payload_len: bytes.len(),
            explicit_retirement: std::sync::atomic::AtomicBool::new(true),
            retirement_leased: std::sync::atomic::AtomicBool::new(false),
        })));
        let index = self.blocks.len() - 1;
        let Storage::Disk(block) = Arc::get_mut(&mut self.blocks[index]).unwrap() else { unreachable!() };
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).create(block.path.parent().unwrap())?;
            block.worker_directory_owned = true;
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        block.file = Some(options.open(&block.path)?);
        block.worker_file_owned = true; // BEFORE the next fallible operation.
        #[cfg(test)]
        eprintln!("owned worker allocation created: {}", block.path.display());
        crate::worker_edit::poll(cancelled)?;
        #[cfg(test)]
        {
            self.creates += 1;
            if self.fault == WorkerFault::AfterOpenPanic {
                panic!("controlled worker write unwind");
            }
            if self.fault == WorkerFault::Write {
                return Err(io::ErrorKind::StorageFull.into());
            }
        }
        block.file.as_mut().unwrap().write_all(bytes)?;
        #[cfg(test)]
        if self.fault == WorkerFault::CancelAfterWrite {
            cancelled.store(true, Ordering::Release);
        }
        crate::worker_edit::poll(cancelled)?;
        #[cfg(test)]
        if self.fault == WorkerFault::Map {
            return Err(io::ErrorKind::Other.into());
        }
        // SAFETY: private create_new ownership; immutable after mapping; both
        // file and map remain registered until the unique worker cleanup pass.
        block.map = Some(unsafe { memmap2::Mmap::map(block.file.as_ref().unwrap())? });
        crate::worker_edit::poll(cancelled)?;
        Ok(self.blocks[index].clone())
    }
    fn decoded_copy(
        &mut self,
        source: &Arc<IndexedDecoded>,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> io::Result<Arc<Storage>> {
        use crate::decoded_backing::{BackingPaths, DecodedBacking};
        use crate::worker_edit::{capacity, overflow};
        crate::worker_edit::poll(cancelled)?;
        if let Some(storage) = self
            .decoded
            .iter()
            .find_map(|entry| entry.source.matches(&source.origin).then_some(entry.storage.as_ref()).flatten())
        {
            return Ok(storage.clone());
        }
        let count = self
            .blocks
            .len()
            .checked_add(self.decoded.len().checked_mul(3).ok_or_else(overflow)?)
            .and_then(|n| n.checked_add(3))
            .ok_or_else(overflow)?;
        if count > self.plan.max_new_blocks {
            return Err(capacity());
        }
        let lengths = [
            source.origin.raw_bytes().len(),
            source.origin.logical_bytes().len(),
            source.origin.checkpoint_count().checked_mul(16).ok_or_else(overflow)?,
        ];
        for length in lengths {
            self.disk = self
                .disk
                .checked_add(worker_disk_charge(length as u64)?)
                .filter(|n| *n <= self.plan.disk_bytes)
                .ok_or_else(capacity)?;
        }
        // Sequential LineIndex has two doubling Vecs: one per1024 newlines,
        // one per64KiB. Four times their maximum record count covers min-growth
        // capacity too. Charge before index construction or touching cold bytes.
        let len = lengths[1];
        let records = (len / crate::line_index::DEFAULT_EVERY as usize)
            .checked_add(1)
            .and_then(|n| n.checked_add(len.div_ceil(ADD_BLOCK)))
            .and_then(|n| n.checked_add(1))
            .ok_or_else(overflow)?;
        let index_bytes = records
            .checked_mul(4 * std::mem::size_of::<(u64, u64)>())
            .and_then(|n| {
                n.checked_add(std::mem::size_of::<IndexedDecoded>() + std::mem::size_of::<Storage>() + 2 * ARC_HEADER)
            })
            .and_then(|n| n.checked_add(DecodedBacking::registered_copy_metadata()))
            .ok_or_else(overflow)?;
        let path_bytes =
            self.plan.max_path_bytes.checked_mul(2).and_then(|n| n.checked_mul(12)).ok_or_else(overflow)?;
        self.charge(index_bytes.checked_add(path_bytes).ok_or_else(overflow)?)?;
        static NEXT_COPY: AtomicU64 = AtomicU64::new(0);
        let id =
            NEXT_COPY.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1)).map_err(|_| overflow())?;
        let directory = std::env::temp_dir().join(format!("kwil-worker-decoded-{}-{id}", std::process::id()));
        let paths = BackingPaths {
            raw: directory.join("raw"),
            logical: directory.join("logical"),
            checkpoints: directory.join("checkpoints"),
            directory,
        };
        let path_capacity = self.plan.max_path_bytes.checked_mul(2).ok_or_else(overflow)?;
        for path in [&paths.directory, &paths.raw, &paths.logical, &paths.checkpoints] {
            if path.as_os_str().as_encoded_bytes().len() > self.plan.max_path_bytes || path.capacity() > path_capacity {
                return Err(capacity());
            }
        }
        // Registration precedes even mkdir. Failed create never conveys ownership.
        self.decoded.push(WorkerDecodedCopy {
            source: source.origin.weak_identity(),
            paths,
            directory_owned: false,
            files: [None, None, None],
            owned: [false; 3],
            owner: None,
            storage: None,
        });
        let at = self.decoded.len() - 1;
        let entry = &mut self.decoded[at];
        let directory = std::fs::DirBuilder::new();
        #[cfg(unix)]
        let mut directory = directory;
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directory.mode(0o700);
        }
        directory.create(&entry.paths.directory)?;
        entry.directory_owned = true;
        #[cfg(test)]
        eprintln!("owned decoded directory created: {}", entry.paths.directory.display());
        let paths = [&entry.paths.raw, &entry.paths.logical, &entry.paths.checkpoints];
        for (i, path) in paths.into_iter().enumerate() {
            crate::worker_edit::poll(cancelled)?;
            let mut options = OpenOptions::new();
            options.create_new(true).read(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            entry.files[i] = Some(options.open(path)?);
            entry.owned[i] = true;
            #[cfg(test)]
            eprintln!("owned decoded allocation created: {}", path.display());
            #[cfg(test)]
            {
                self.creates += 1;
                if self.fault == WorkerFault::AfterOpenPanic {
                    panic!("controlled decoded open unwind");
                }
                if self.fault == WorkerFault::Write {
                    return Err(io::ErrorKind::StorageFull.into());
                }
            }
        }
        let [raw, logical, checkpoints] = &mut entry.files;
        #[cfg(test)]
        let mut polls = 0;
        let owner = DecodedBacking::copy_registered(
            &source.origin,
            entry.paths.clone(),
            [raw.as_mut().unwrap(), logical.as_mut().unwrap(), checkpoints.as_mut().unwrap()],
            &mut || {
                #[cfg(test)]
                {
                    polls += 1;
                    if polls == 3 {
                        if self.fault == WorkerFault::DecodedCopyCancel {
                            cancelled.store(true, Ordering::Release);
                        }
                        if self.fault == WorkerFault::DecodedCopyPanic {
                            panic!("controlled decoded copy unwind");
                        }
                    }
                }
                crate::worker_edit::poll(cancelled)
            },
        )?;
        entry.owner = Some(owner); // rescue owns maps BEFORE next fallible step
        #[cfg(test)]
        if self.fault == WorkerFault::Map {
            return Err(io::ErrorKind::Other.into());
        }
        let indexed = IndexedDecoded::new_checked(entry.owner.as_ref().unwrap().snapshot(), &mut || {
            crate::worker_edit::poll(cancelled)
        })?;
        let storage = Arc::new(Storage::Decoded(Arc::new(indexed)));
        entry.storage = Some(storage.clone());
        Ok(storage)
    }

    fn allowed(&self, storage: &Arc<Storage>, budget: &Arc<AddBudget>) -> bool {
        match storage.as_ref() {
            Storage::Opaque { logical, raw, offsets, .. } => {
                [logical, raw, offsets].iter().all(|s| self.allowed(s, budget))
            }
            Storage::Memory { budget: source, .. } => Arc::ptr_eq(source, budget) && self.certified(storage),
            _ => self.certified(storage),
        }
    }
    pub(crate) fn snapshot(
        &mut self,
        root: Tree,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> io::Result<crate::Snapshot> {
        crate::worker_edit::poll(cancelled)?;
        self.charge_tree(&root)?; // bounds OriginalUses BTreeMap and traversal.
        Ok(crate::buffer::Buffer::recovery_snapshot(root))
    }
    pub(crate) fn validate(
        &mut self,
        snapshot: &crate::Snapshot,
        budget: &Arc<AddBudget>,
        owner: &Arc<()>,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> io::Result<()> {
        fn storage_check(storage: &Storage) -> io::Result<()> {
            if let Storage::Opaque { logical, raw, offsets, .. } = storage {
                let logical = logical.bytes().map_err(io::Error::other)?;
                let raw = raw.bytes().map_err(io::Error::other)?;
                let offsets = offsets.bytes().map_err(io::Error::other)?;
                if logical.len() >= ADD_BLOCK / 4
                    || offsets.len() != (logical.len() + 1) * 4
                    || offsets.len() > ADD_BLOCK
                    || raw.len() > ADD_BLOCK
                    || offset_at(offsets, 0) != 0
                    || offset_at(offsets, logical.len() as u64) != raw.len()
                {
                    return Err(crate::worker_edit::invalid("opaque table shape"));
                }
                let mut i = 0;
                while i < logical.len() {
                    let a = offset_at(offsets, i as u64);
                    let b = offset_at(offsets, i as u64 + 1);
                    if !atom_boundary(offsets, i) || a > b || b > raw.len() {
                        return Err(crate::worker_edit::invalid("opaque provenance offsets"));
                    }
                    if logical[i] == 0xff {
                        if !atom_boundary(offsets, i + 1) || a == b {
                            return Err(crate::worker_edit::invalid("opaque atom endpoints"));
                        }
                        i += 1;
                    } else if atom_boundary(offsets, i + 1) && a == b {
                        i += 1; // Canonical/added UTF-8 has no raw payload.
                    } else {
                        let first = i;
                        i += 1;
                        while i < logical.len() && !atom_boundary(offsets, i) {
                            if offset_at(offsets, i as u64) != a {
                                return Err(crate::worker_edit::invalid("preserved atom interior"));
                            }
                            i += 1;
                        }
                        if !atom_boundary(offsets, i)
                            || offset_at(offsets, i as u64) <= a
                            || offset_at(offsets, i as u64) > raw.len()
                            || std::str::from_utf8(&logical[first..i]).is_err()
                        {
                            return Err(crate::worker_edit::invalid("preserved text endpoints"));
                        }
                    }
                }
            }
            Ok(())
        }
        fn visit(
            a: &WorkerAllocation,
            node: &Node,
            budget: &Arc<AddBudget>,
            owner: &Arc<()>,
            cancelled: &std::sync::atomic::AtomicBool,
        ) -> io::Result<(u64, u64, StorageResources)> {
            crate::worker_edit::poll(cancelled)?;
            let (bytes, breaks, resources) = match &node.kind {
                Kind::Leaf(piece) => {
                    if !a.allowed(&piece.storage, budget) {
                        return Err(crate::worker_edit::invalid("untracked candidate graft"));
                    }
                    if matches!(piece.storage.as_ref(), Storage::Original(_) | Storage::Decoded(_))
                        && !piece.owner.as_ref().is_some_and(|p| Arc::ptr_eq(p, owner))
                    {
                        return Err(crate::worker_edit::invalid("foreign source owner"));
                    }
                    storage_check(&piece.storage)?;
                    let data = piece.storage.bytes().map_err(io::Error::other)?;
                    if piece.start > piece.end || piece.end > data.len() as u64 {
                        return Err(crate::worker_edit::invalid("candidate piece range"));
                    }
                    if let Storage::Decoded(source) = piece.storage.as_ref() {
                        crate::decoded_piece::validate_range(&source.origin, piece.start..piece.end)?;
                    } else if let Storage::Opaque { offsets, .. } = piece.storage.as_ref() {
                        let offsets = offsets.bytes().map_err(io::Error::other)?;
                        validate_detached_endpoint(data, offsets, piece.start as usize)?;
                        validate_detached_endpoint(data, offsets, piece.end as usize)?;
                    }
                    for _ in data[piece.start as usize..piece.end as usize].chunks(ADD_BLOCK) {
                        crate::worker_edit::poll(cancelled)?;
                    }
                    let breaks = piece
                        .storage
                        .rank(piece.end)
                        .map_err(io::Error::other)?
                        .checked_sub(piece.storage.rank(piece.start).map_err(io::Error::other)?)
                        .ok_or_else(crate::worker_edit::overflow)?;
                    if breaks != piece.breaks {
                        return Err(crate::worker_edit::invalid("candidate line metrics"));
                    }
                    (
                        piece.end - piece.start,
                        breaks,
                        piece.storage.resources().plus(StorageResources { tree_nodes: 1, ..Default::default() }),
                    )
                }
                Kind::Branch(left, right) => {
                    let (lb, ll, lr) = visit(a, left, budget, owner, cancelled)?;
                    let (rb, rl, rr) = visit(a, right, budget, owner, cancelled)?;
                    (
                        lb.checked_add(rb).ok_or_else(crate::worker_edit::overflow)?,
                        ll.checked_add(rl).ok_or_else(crate::worker_edit::overflow)?,
                        lr.plus(rr).plus(StorageResources { tree_nodes: 1, ..Default::default() }),
                    )
                }
            };
            if node.metrics.bytes != bytes || node.metrics.breaks != breaks || node.metrics.resources != resources {
                return Err(crate::worker_edit::invalid("candidate cached metrics"));
            }
            Ok((bytes, breaks, resources))
        }
        self.charge_tree(&snapshot.root)?;
        if let Some(root) = &snapshot.root {
            visit(self, root, budget, owner, cancelled)?;
        }
        let actual = original_uses(&snapshot.root, 0..snapshot.len());
        if actual.len() != snapshot.originals.len()
            || actual
                .iter()
                .any(|(key, (_, count))| snapshot.originals.get(key).is_none_or(|(_, other)| count != other))
        {
            return Err(crate::worker_edit::invalid("candidate original directory"));
        }
        crate::worker_edit::poll(cancelled)
    }
    pub(crate) fn claim(&mut self, cancelled: &std::sync::atomic::AtomicBool) -> io::Result<()> {
        #[cfg(test)]
        let mut polls = 0;
        spill_retirement::claim_worker_allocations(&self.blocks, &mut || {
            #[cfg(test)]
            {
                polls += 1;
                if polls == 2 {
                    if self.fault == WorkerFault::ClaimPrefixPanic {
                        panic!("controlled prefix claim unwind");
                    }
                    if self.fault == WorkerFault::ClaimPrefixCancel {
                        cancelled.store(true, Ordering::Release);
                    }
                }
            }
            cancelled.load(Ordering::Acquire)
        })?;
        self.claimed = true;
        #[cfg(test)]
        if self.fault == WorkerFault::CancelAfterClaim {
            cancelled.store(true, Ordering::Release);
        }
        Ok(())
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.blocks.is_empty() && self.decoded.is_empty()
    }
    pub(crate) fn visit_owned_paths(&self, visit: &mut impl FnMut(&std::path::Path)) {
        for entry in &self.decoded {
            if entry.directory_owned {
                visit(&entry.paths.directory);
            }
            for (owned, path) in
                entry.owned.iter().zip([&entry.paths.raw, &entry.paths.logical, &entry.paths.checkpoints])
            {
                if *owned {
                    visit(path);
                }
            }
        }
        for storage in &self.blocks {
            if let Storage::Disk(block) = storage.as_ref()
                && (block.worker_file_owned || block.worker_directory_owned)
            {
                visit(&block.path);
            }
        }
    }
    #[cfg(test)]
    pub(crate) fn worker_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<_> = self
            .blocks
            .iter()
            .filter_map(|s| if let Storage::Disk(b) = s.as_ref() { Some(b.path.clone()) } else { None })
            .collect();
        for entry in &self.decoded {
            paths.extend([entry.paths.raw.clone(), entry.paths.logical.clone(), entry.paths.checkpoints.clone()]);
        }
        paths
    }
    #[cfg(test)]
    pub(crate) fn worker_reserved_disk(&self) -> u64 {
        self.disk
    }
    pub(crate) fn retire(&mut self) -> SpillRetirementReport {
        self.baseline.clear(); // release source pins only on the worker.
        let mut report = SpillRetirementReport::default();
        let mut at = 0;
        while at < self.decoded.len() {
            let entry = &mut self.decoded[at];
            if entry.storage.as_ref().is_some_and(|storage| Arc::strong_count(storage) != 1) {
                report.live += 1;
                at += 1;
                continue;
            }
            entry.storage.take();
            if let Some(owner) = entry.owner.take()
                && let Err(owner) = owner.close_registered()
            {
                entry.owner = Some(owner);
                report.live += 1;
                at += 1;
                continue;
            }
            for file in &mut entry.files {
                file.take();
            }
            let result = (|| {
                #[cfg(test)]
                if self.fault == WorkerFault::Cleanup {
                    return Err(io::Error::from(io::ErrorKind::PermissionDenied));
                }
                for (owned, path) in
                    entry.owned.iter_mut().zip([&entry.paths.raw, &entry.paths.logical, &entry.paths.checkpoints])
                {
                    if *owned {
                        match std::fs::remove_file(path) {
                            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                            _ => {}
                        }
                        *owned = false;
                    }
                }
                if entry.directory_owned {
                    match std::fs::remove_dir(&entry.paths.directory) {
                        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                        _ => {}
                    }
                    entry.directory_owned = false;
                }
                Ok(())
            })();
            match result {
                Ok(()) => {
                    self.decoded.swap_remove(at);
                    report.retired += 3;
                }
                Err(error) => {
                    report.cleanup_pending += 1;
                    if report.first_error.is_none() {
                        report.first_error = Some(error);
                    }
                    at += 1;
                }
            }
        }
        let mut index = 0;
        while index < self.blocks.len() {
            let Some(storage) = Arc::get_mut(&mut self.blocks[index]) else {
                report.live += 1;
                index += 1;
                continue;
            };
            let result = if let Storage::Disk(block) = storage {
                block.map.take();
                block.file.take();
                #[cfg(test)]
                let blocked = self.fault == WorkerFault::Cleanup;
                #[cfg(not(test))]
                let blocked = false;
                (|| {
                    if blocked {
                        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
                    }
                    if block.worker_file_owned {
                        match std::fs::remove_file(&block.path) {
                            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                            _ => {}
                        }
                        block.worker_file_owned = false;
                    }
                    if block.worker_directory_owned {
                        match std::fs::remove_dir(block.path.parent().unwrap()) {
                            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                            _ => {}
                        }
                        block.worker_directory_owned = false;
                    }
                    if self.claimed {
                        block.retirement_leased.store(false, Ordering::Release);
                    }
                    Ok(())
                })()
            } else {
                Ok(())
            };
            match result {
                Ok(()) => {
                    self.blocks.swap_remove(index);
                    report.retired += 1;
                }
                Err(error) => {
                    report.cleanup_pending += 1;
                    if report.first_error.is_none() {
                        report.first_error = Some(error);
                    }
                    index += 1;
                }
            }
        }
        report
    }
}

fn worker_added(
    bytes: &[u8],
    budget: &Arc<AddBudget>,
    allocation: &mut WorkerAllocation,
    cancelled: &std::sync::atomic::AtomicBool,
) -> io::Result<Tree> {
    let mut tree = None;
    for chunk in bytes.chunks(ADD_BLOCK) {
        allocation.charge_tree(&tree)?;
        let storage = allocation.block(chunk, budget, cancelled)?;
        tree = join(
            tree,
            Some(leaf(Piece {
                storage,
                owner: Some(budget.owner.clone()),
                start: 0,
                end: chunk.len() as u64,
                first_line: 0,
                breaks: memchr_iter(b'\n', chunk).count() as u64,
            })),
        );
    }
    Ok(tree)
}
fn worker_detached(
    encoding: Encoding,
    bytes: &[u8],
    raw: &[u8],
    offsets: &[u8],
    budget: &Arc<AddBudget>,
    allocation: &mut WorkerAllocation,
    cancelled: &std::sync::atomic::AtomicBool,
) -> io::Result<Tree> {
    if raw.is_empty() {
        return worker_added(bytes, budget, allocation, cancelled);
    }
    allocation.charge_tree(&None)?;
    let logical = allocation.block(bytes, budget, cancelled)?;
    let raw = allocation.block(raw, budget, cancelled)?;
    let offsets = allocation.block(offsets, budget, cancelled)?;
    Ok(Some(leaf(Piece {
        storage: Arc::new(Storage::Opaque { encoding, logical, raw, offsets }),
        owner: Some(budget.owner.clone()),
        start: 0,
        end: bytes.len() as u64,
        first_line: 0,
        breaks: memchr_iter(b'\n', bytes).count() as u64,
    })))
}
pub(crate) fn worker_import(
    tree: Tree,
    budget: &Arc<AddBudget>,
    allocation: &mut WorkerAllocation,
    cancelled: &std::sync::atomic::AtomicBool,
) -> io::Result<Tree> {
    crate::worker_edit::poll(cancelled)?;
    let Some(node) = tree else { return Ok(None) };
    match &node.kind {
        Kind::Branch(left, right) => {
            allocation.charge_tree(&Some(node.clone()))?;
            let a = worker_import(Some(left.clone()), budget, allocation, cancelled)?;
            let b = worker_import(Some(right.clone()), budget, allocation, cancelled)?;
            Ok(join(a, b))
        }
        Kind::Leaf(piece) => {
            if allocation.allowed(&piece.storage, budget) {
                return Ok(Some(node));
            }
            match piece.storage.as_ref() {
                Storage::Decoded(source) => {
                    crate::decoded_piece::validate_range(&source.origin, piece.start..piece.end)?;
                    let storage = allocation.decoded_copy(source, cancelled)?;
                    let first_line = storage.rank(piece.start).map_err(io::Error::other)?;
                    let breaks = storage.rank(piece.end).map_err(io::Error::other)? - first_line;
                    Ok(Some(leaf(Piece {
                        storage,
                        owner: Some(budget.owner.clone()),
                        start: piece.start,
                        end: piece.end,
                        first_line,
                        breaks,
                    })))
                }
                Storage::Opaque { encoding, raw, offsets, .. } => {
                    let mut builder = ImportBuilder::new(*encoding, budget);
                    builder.allocation = Some(allocation);
                    builder.worker_cancel = Some(cancelled);
                    feed_detached(
                        &mut builder,
                        piece.storage.bytes().map_err(io::Error::other)?,
                        raw.bytes().map_err(io::Error::other)?,
                        offsets.bytes().map_err(io::Error::other)?,
                        piece.start..piece.end,
                        &mut || crate::worker_edit::poll(cancelled),
                    )?;
                    builder.finish()
                }
                _ => {
                    // COPY foreign spills too, even Free/all-spilled leaves.
                    let bytes = piece.storage.bytes().map_err(io::Error::other)?;
                    worker_added(&bytes[piece.start as usize..piece.end as usize], budget, allocation, cancelled)
                }
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
                let source = match &*piece.storage {
                    Storage::Original(file) => Some(SourceRef::Raw(file.history_backing_ref())),
                    Storage::Decoded(source) => {
                        Some(SourceRef::Decoded { source: Arc::downgrade(source), cost: source.retained_bytes() })
                    }
                    _ => None,
                };
                if let Some(source) = source {
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
            Storage::Decoded(source) => piece.start == 0 && piece.end == source.origin.logical_bytes().len() as u64,
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
            Storage::Original(_) | Storage::Decoded(_) => {
                Some(leaf(Piece { owner: Some(owner.clone()), ..piece.clone() }))
            }
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
    /// Logical UTF-8 coordinates in this immutable decoded generation. Its
    /// typed opaque atoms retain their original bytes, independently of BOMs.
    Decoded {
        origin: &'a DecodedSnapshot,
        range: Range<u64>,
        bytes: &'a [u8],
    },
    /// Detached foreign opaque atoms. `raw_offsets` has one LE u32 per atom
    /// boundary, including the end, relative to the block's original raw start.
    /// Subtract its first entry to index into this chunk's `raw_bytes`.
    /// Each atom corresponds to one FF in `bytes`, NOT to a literal FF byte.
    Opaque {
        encoding: Encoding,
        bytes: &'a [u8],
        raw_bytes: &'a [u8],
        raw_offsets: &'a [u8],
    },
    /// Complete valid UTF-8 atoms with exact noncanonical source bytes. Each
    /// LE u32 entry describes one logical byte boundary, including the end.
    /// Bit 31 marks an atom interior, which is NOT a raw boundary; the low bits
    /// retain that atom's raw start. Exact entries are relative to the storage
    /// block; subtract the first entry to index this chunk's `raw_bytes`.
    /// Partial atoms are yielded as Added text, never with invented endpoints.
    Preserved {
        encoding: Encoding,
        bytes: &'a [u8],
        raw_bytes: &'a [u8],
        raw_offsets: &'a [u8],
    },
    /// Added bytes are UTF-8 for an encoding-aware editor.
    Added(&'a [u8]),
}

/// Read-only traversal with the same O(tree height) scratch bound as `Chunks`.
pub struct SourceChunks<'a> {
    chunks: Chunks<'a>,
    detached: Option<DetachedChunks<'a>>,
}

struct DetachedChunks<'a> {
    encoding: Encoding,
    bytes: &'a [u8],
    raw: &'a [u8],
    table: &'a [u8],
    next: usize,
    end: usize,
}
impl<'a> DetachedChunks<'a> {
    fn byte_admitted<E>(&self, at: usize, admit: &mut impl FnMut(usize) -> Result<(), E>) -> Result<u8, E> {
        admit(1)?;
        Ok(self.bytes[at])
    }
    fn offset_admitted<E>(&self, at: usize, admit: &mut impl FnMut(usize) -> Result<(), E>) -> Result<usize, E> {
        admit(std::mem::size_of::<u32>())?;
        Ok(offset_at(self.table, at as u64))
    }
    fn boundary_admitted<E>(&self, at: usize, admit: &mut impl FnMut(usize) -> Result<(), E>) -> Result<bool, E> {
        admit(std::mem::size_of::<u32>())?;
        Ok(atom_boundary(self.table, at))
    }
    fn next_admitted<E>(
        &mut self,
        admit: &mut impl FnMut(usize) -> Result<(), E>,
    ) -> Result<Option<SourceChunk<'a>>, E> {
        if self.next == self.end {
            return Ok(None);
        }
        let start = self.next;
        if self.byte_admitted(start, admit)? == 0xff {
            let mut end = start;
            while end < self.end && self.byte_admitted(end, admit)? == 0xff {
                end += 1;
            }
            let begin = self.offset_admitted(start, admit)?;
            let finish = self.offset_admitted(end, admit)?;
            self.next = end;
            return Ok(Some(SourceChunk::Opaque {
                encoding: self.encoding,
                bytes: &self.bytes[start..end],
                raw_bytes: &self.raw[begin..finish],
                raw_offsets: &self.table[start * 4..(end + 1) * 4],
            }));
        }
        if !self.boundary_admitted(start, admit)? {
            let mut end = start + 1;
            while end < self.end && !self.boundary_admitted(end, admit)? {
                end += 1;
            }
            self.next = end;
            return Ok(Some(SourceChunk::Added(&self.bytes[start..end])));
        }
        let mut end = start;
        let preserved = !self.boundary_admitted(start + 1, admit)?
            || self.offset_admitted(start + 1, admit)? > self.offset_admitted(start, admit)?;
        while end < self.end && self.byte_admitted(end, admit)? != 0xff {
            let mut next = end + 1;
            while next < self.end && !self.boundary_admitted(next, admit)? {
                next += 1;
            }
            if !self.boundary_admitted(next, admit)? {
                // A trailing partial atom has logical bytes only.
                if end == start {
                    self.next = self.end;
                    return Ok(Some(SourceChunk::Added(&self.bytes[start..self.end])));
                }
                break;
            }
            let has_raw = self.offset_admitted(next, admit)? > self.offset_admitted(end, admit)?;
            if has_raw != preserved {
                break;
            }
            end = next;
        }
        let chunk = if preserved {
            let begin = self.offset_admitted(start, admit)?;
            let finish = self.offset_admitted(end, admit)?;
            SourceChunk::Preserved {
                encoding: self.encoding,
                bytes: &self.bytes[start..end],
                raw_bytes: &self.raw[begin..finish],
                raw_offsets: &self.table[start * 4..(end + 1) * 4],
            }
        } else {
            SourceChunk::Added(&self.bytes[start..end])
        };
        self.next = end;
        Ok(Some(chunk))
    }
}
impl<'a> Iterator for DetachedChunks<'a> {
    type Item = SourceChunk<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        match self.next_admitted(&mut |_| Ok::<(), std::convert::Infallible>(())) {
            Ok(chunk) => chunk,
            Err(never) => match never {},
        }
    }
}

impl<'a> SourceChunks<'a> {
    pub(crate) fn new(tree: &'a Tree, range: Range<u64>) -> Self {
        Self { chunks: Chunks::new(tree, range), detached: None }
    }
    /// Admit every detached classification read BEFORE accessing the logical
    /// byte (1 byte) or raw endpoint entry (4 bytes). Borrowing the returned
    /// slices does not read their contents: callers separately admit payload
    /// copies, decoding and validation. Plain Added/Original/Decoded pieces
    /// need no payload classification and do not call `admit`.
    ///
    /// Any admission/read error terminates this cursor. Admission errors are
    /// returned unchanged; no rejected read is performed or replayed later.
    pub fn next_admitted(
        &mut self,
        mut admit: impl FnMut(usize) -> io::Result<()>,
    ) -> io::Result<Option<SourceChunk<'a>>> {
        match self.next_checked(&mut admit) {
            Ok(chunk) => chunk.transpose().map_err(io::Error::other),
            Err(error) => {
                self.chunks.stack.clear();
                self.detached = None;
                Err(error)
            }
        }
    }
    fn next_checked<E>(
        &mut self,
        admit: &mut impl FnMut(usize) -> Result<(), E>,
    ) -> Result<Option<Result<SourceChunk<'a>, ReadError>>, E> {
        if let Some(cursor) = &mut self.detached {
            if let Some(chunk) = cursor.next_admitted(admit)? {
                return Ok(Some(Ok(chunk)));
            }
            self.detached = None;
        }
        while let Some((node, offset)) = self.chunks.stack.pop() {
            if offset >= self.chunks.range.end || offset + node.metrics.bytes <= self.chunks.range.start {
                continue;
            }
            match &node.kind {
                Kind::Branch(left, right) => {
                    self.chunks.stack.push((right, offset + left.metrics.bytes));
                    self.chunks.stack.push((left, offset));
                }
                Kind::Leaf(piece) => {
                    let start = piece.start + self.chunks.range.start.saturating_sub(offset);
                    let end = piece.start + (self.chunks.range.end - offset).min(node.metrics.bytes);
                    let result = if let Storage::Opaque { encoding, raw, offsets, .. } = &*piece.storage {
                        let payloads =
                            piece.storage.bytes().and_then(|bytes| Ok((bytes, raw.bytes()?, offsets.bytes()?)));
                        match payloads {
                            Ok((bytes, raw, table)) => {
                                let mut cursor = DetachedChunks {
                                    encoding: *encoding,
                                    bytes,
                                    raw,
                                    table,
                                    next: start as usize,
                                    end: end as usize,
                                };
                                let first = cursor.next_admitted(admit)?.expect("nonempty detached piece range");
                                self.detached = Some(cursor);
                                Ok(first)
                            }
                            Err(error) => Err(error),
                        }
                    } else {
                        piece.storage.bytes().map(|bytes| {
                            let bytes = &bytes[start as usize..end as usize];
                            match piece.storage.as_ref() {
                                Storage::Original(file) => SourceChunk::Original { file, range: start..end, bytes },
                                Storage::Decoded(source) => {
                                    SourceChunk::Decoded { origin: &source.origin, range: start..end, bytes }
                                }
                                _ => SourceChunk::Added(bytes),
                            }
                        })
                    };
                    if result.is_err() {
                        self.chunks.stack.clear();
                        self.detached = None;
                    }
                    return Ok(Some(result));
                }
            }
        }
        Ok(None)
    }
}

impl<'a> Iterator for SourceChunks<'a> {
    type Item = Result<SourceChunk<'a>, ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_checked(&mut |_| Ok::<(), std::convert::Infallible>(())) {
            Ok(chunk) => chunk,
            Err(never) => match never {},
        }
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
    fn storage_paths(storage: &Storage, paths: &mut Vec<PathBuf>) {
        match storage {
            Storage::Disk(block) => paths.push(block.path.clone()),
            Storage::Opaque { logical, raw, offsets, .. } => {
                for storage in [logical, raw, offsets] {
                    storage_paths(storage, paths);
                }
            }
            _ => {}
        }
    }
    let mut paths = Vec::new();
    if let Some(node) = tree {
        match &node.kind {
            Kind::Leaf(piece) => {
                storage_paths(&piece.storage, &mut paths);
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
    fn source_chunks_admission_refusal_precedes_byte_and_endpoint_reads() {
        // Deliberately inaccessible payloads: reading before the callback
        // refuses would panic. Exercise each centralized classification read.
        let encoding = Encoding::for_label("big5").unwrap();
        let empty = [];
        let zero = 0_u32.to_le_bytes();
        for (bytes, table, refuse_at, refused_width) in [
            (&empty[..], &empty[..], 1, 1),   // first logical-byte peek
            (&b"x"[..], &empty[..], 2, 4),    // first atom-boundary entry
            (&b"\xff"[..], &empty[..], 3, 4), // opaque raw start
            (&b"\xff"[..], &zero[..], 4, 4),  // opaque raw end
        ] {
            let mut cursor = DetachedChunks { encoding, bytes, raw: &[], table, next: 0, end: 1 };
            let mut calls = 0;
            let result = cursor.next_admitted(&mut |width| {
                calls += 1;
                if calls == refuse_at {
                    assert_eq!(width, refused_width);
                    Err(io::Error::new(io::ErrorKind::WouldBlock, "classification admission marker"))
                } else {
                    Ok(())
                }
            });
            let error = result.err().expect("inaccessible read must be refused first");
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            assert_eq!(error.to_string(), "classification admission marker");
            assert_eq!(calls, refuse_at);
            assert_eq!(cursor.next, 0, "failed classification never publishes advancement");
        }
    }

    #[test]
    fn source_chunks_admitted_preserves_error_identity_and_terminates_after_refusal() {
        #[derive(Debug)]
        struct Marker(Arc<()>);
        impl std::fmt::Display for Marker {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("admission identity")
            }
        }
        impl std::error::Error for Marker {}
        let encoding = Encoding::for_label("big5").unwrap();
        let budget = AddBudget::new(128, Arc::new(()));
        let mut builder = ImportBuilder::new(encoding, &budget);
        builder.preserved("Ê\u{304}".as_bytes(), b"\x88\x62").unwrap();
        builder.text(b"tail").unwrap();
        let tree = builder.finish().unwrap();
        let identity = Arc::new(());
        for refuse_at in 1..=12 {
            let mut chunks = SourceChunks::new(&tree, 0..8);
            let mut calls = 0;
            let error = chunks
                .next_admitted(|_| {
                    calls += 1;
                    if calls == refuse_at {
                        Err(io::Error::new(io::ErrorKind::WouldBlock, Marker(identity.clone())))
                    } else {
                        Ok(())
                    }
                })
                .err()
                .expect("first preserved chunk needs more than twelve classification reads");
            assert!(Arc::ptr_eq(&error.get_ref().unwrap().downcast_ref::<Marker>().unwrap().0, &identity));
            assert_eq!(calls, refuse_at);
            assert!(chunks.next_admitted(|_| panic!("failed cursor must not read again")).unwrap().is_none());
            assert!(chunks.next().is_none());
        }
        drop(tree);
        assert_eq!(budget.used.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn source_chunks_admitted_matches_ordinary_iteration_for_all_detached_subranges() {
        fn fingerprint(chunk: SourceChunk<'_>) -> (u8, Vec<u8>, Vec<u8>, Vec<u8>) {
            match chunk {
                SourceChunk::Added(bytes) => (0, bytes.to_vec(), vec![], vec![]),
                SourceChunk::Opaque { bytes, raw_bytes, raw_offsets, .. } => {
                    (1, bytes.to_vec(), raw_bytes.to_vec(), raw_offsets.to_vec())
                }
                SourceChunk::Preserved { bytes, raw_bytes, raw_offsets, .. } => {
                    (2, bytes.to_vec(), raw_bytes.to_vec(), raw_offsets.to_vec())
                }
                _ => panic!("expected detached payload"),
            }
        }
        let encoding = Encoding::for_label("big5").unwrap();
        let budget = AddBudget::new(128, Arc::new(()));
        let mut builder = ImportBuilder::new(encoding, &budget);
        builder.text(b"a").unwrap();
        builder.preserved("Ê\u{304}".as_bytes(), b"\x88\x62").unwrap();
        builder.opaque(b"\xff").unwrap();
        builder.text(b"Z").unwrap();
        builder.preserved("ê\u{30c}".as_bytes(), b"\x88\xa5").unwrap();
        let tree = builder.finish().unwrap();
        for first in 0..=11 {
            for last in first..=11 {
                let ordinary: Vec<_> =
                    SourceChunks::new(&tree, first..last).map(|chunk| fingerprint(chunk.unwrap())).collect();
                let mut chunks = SourceChunks::new(&tree, first..last);
                let mut admitted = Vec::new();
                let mut reads = 0;
                while let Some(chunk) = chunks
                    .next_admitted(|width| {
                        assert!(width == 1 || width == 4);
                        reads += width;
                        Ok(())
                    })
                    .unwrap()
                {
                    admitted.push(fingerprint(chunk));
                }
                assert_eq!(admitted, ordinary, "range {first}..{last}");
                assert_eq!(reads == 0, first == last);
            }
        }
    }

    #[test]
    fn recovery_preserved_validates_codec_atoms_and_offsets_before_allocating() {
        let encoding = Encoding::for_label("big5").unwrap();
        let bytes = "Ê\u{304}".as_bytes();
        let raw = b"\x88\x62";
        let table: Vec<_> = [7, ATOM_INTERIOR | 7, ATOM_INTERIOR | 7, ATOM_INTERIOR | 7, 9]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect();
        let budget = AddBudget::new(128, Arc::new(()));
        let tree = recovery_preserved(encoding, bytes, raw, &table, &budget, &mut || Ok(())).unwrap();
        assert_eq!(budget.used.load(Ordering::Relaxed), 4 + 2 + 20);
        let SourceChunk::Preserved { raw_offsets, raw_bytes, .. } =
            SourceChunks::new(&tree, 0..4).next().unwrap().unwrap()
        else {
            panic!("typed alias lost");
        };
        assert_eq!(offset_entry(raw_offsets, 0), 0, "external base is normalized exactly once");
        assert_eq!(offset_entry(raw_offsets, 2), ATOM_INTERIOR);
        assert_eq!(raw_bytes, raw);
        drop(tree);
        assert_eq!(budget.used.load(Ordering::Relaxed), 0);
        let mut tables = vec![table[..16].to_vec()];
        for (at, value) in [(0, ATOM_INTERIOR | 7), (1, ATOM_INTERIOR | 8), (2, 8), (4, ATOM_INTERIOR | 9), (4, 10)] {
            let mut broken = table.clone();
            broken[at * 4..at * 4 + 4].copy_from_slice(&value.to_le_bytes());
            tables.push(broken);
        }
        for broken in tables {
            assert_eq!(
                recovery_preserved(encoding, bytes, raw, &broken, &budget, &mut || Ok(())).err().unwrap().kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(budget.used.load(Ordering::Relaxed), 0);
        }
        assert!(recovery_preserved(Encoding::UTF8, bytes, raw, &table, &budget, &mut || Ok(())).is_err());
        assert!(recovery_preserved(encoding, "ê\u{304}".as_bytes(), raw, &table, &budget, &mut || Ok(())).is_err());
        assert!(recovery_preserved(encoding, &[0xff; 4], raw, &table, &budget, &mut || Ok(())).is_err());
        let forged: Vec<_> = [0, ATOM_INTERIOR, 2].into_iter().flat_map(u32::to_le_bytes).collect();
        assert!(
            recovery_preserved(encoding, b"\r\n", b"\r\n", &forged, &budget, &mut || Ok(())).is_err(),
            "adjacent newline scalars must not become one preserved atom"
        );
        assert_eq!(budget.used.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn recovery_preserved_cancel_after_partial_payload_returns_exact_resident_charge() {
        let encoding = Encoding::for_label("big5").unwrap();
        let bytes = "Ê\u{304}".as_bytes();
        let raw = b"\x88\x62";
        let table: Vec<_> =
            [0, ATOM_INTERIOR, ATOM_INTERIOR, ATOM_INTERIOR, 2].into_iter().flat_map(u32::to_le_bytes).collect();
        let mut validation_polls = 0;
        validate_recovery_preserved(encoding, bytes, raw, &table, &mut || {
            validation_polls += 1;
            Ok(())
        })
        .unwrap();
        for cancel_at in [1, validation_polls + 2, validation_polls + 3, validation_polls + 4] {
            let budget = AddBudget::new(128, Arc::new(()));
            let mut polls = 0;
            let result = recovery_preserved(encoding, bytes, raw, &table, &budget, &mut || {
                polls += 1;
                if polls == cancel_at { Err(io::ErrorKind::Interrupted.into()) } else { Ok(()) }
            });
            assert_eq!(result.err().unwrap().kind(), io::ErrorKind::Interrupted);
            assert_eq!(budget.used.load(Ordering::Relaxed), 0);
        }
    }

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
