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
pub(crate) type Tree = Option<Arc<Node>>;
pub(crate) type OriginalUses = BTreeMap<(u8, usize), (SourceRef, u64)>;

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
    pub limit: usize,
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
            limit,
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

struct DiskBlock {
    map: Option<memmap2::Mmap>,
    file: Option<File>,
    path: PathBuf,
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

enum Storage {
    Original(TextFile),
    Decoded(Arc<IndexedDecoded>),
    Memory {
        bytes: Box<[u8]>,
        budget: Arc<AddBudget>,
    },
    Disk(DiskBlock),
    /// Bounded detached mixed text/opaque block. Each payload/table is <= ADD_BLOCK
    /// and uses the destination budget (or its existing immutable spill path).
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
            retained: std::mem::size_of::<Node>() + ARC_HEADER + piece.storage.retained_bytes(),
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
            used.checked_add(bytes.len()).filter(|&next| next <= budget.limit)
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

fn offset_at(offsets: &[u8], at: u64) -> usize {
    let at = usize::try_from(at).expect("bounded opaque logical offset");
    let bytes = offsets.get(at * 4..at * 4 + 4).expect("opaque endpoint table");
    u32::from_le_bytes(bytes.try_into().unwrap()) as usize
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
    tree: Tree,
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
            tree: None,
            admission: None,
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
        let block = if self.raw.is_empty() {
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
            crate::decoded_backing::AtomContent::Text(_) => {
                let clipped = atom.logical.start.max(range.start)..atom.logical.end.min(range.end);
                builder.text(&origin.logical_bytes()[clipped.start as usize..clipped.end as usize])?;
            }
            crate::decoded_backing::AtomContent::Opaque => builder.opaque(atom.raw_bytes)?,
        }
    }
    builder.finish()
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
                let offsets = offsets.bytes().map_err(io::Error::other)?;
                let begin = offset_at(offsets, piece.start);
                let end = offset_at(offsets, piece.end);
                let mut rebased = Vec::with_capacity((piece.end - piece.start + 1) as usize * 4);
                for at in piece.start..=piece.end {
                    rebased.extend_from_slice(&((offset_at(offsets, at) - begin) as u32).to_le_bytes());
                }
                let bytes = piece.storage.bytes().map_err(io::Error::other)?;
                return detached_block(
                    *encoding,
                    &bytes[piece.start as usize..piece.end as usize],
                    &raw.bytes().map_err(io::Error::other)?[begin..end],
                    &rebased,
                    budget,
                );
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
impl<'a> Iterator for DetachedChunks<'a> {
    type Item = SourceChunk<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.next == self.end {
            return None;
        }
        let start = self.next;
        let opaque = self.bytes[start] == 0xff;
        let end =
            self.bytes[start..self.end].iter().position(|&b| (b == 0xff) != opaque).map_or(self.end, |at| start + at);
        self.next = end;
        let bytes = &self.bytes[start..end];
        Some(if opaque {
            let begin = offset_at(self.table, start as u64);
            let finish = offset_at(self.table, end as u64);
            SourceChunk::Opaque {
                encoding: self.encoding,
                bytes,
                raw_bytes: &self.raw[begin..finish],
                raw_offsets: &self.table[start * 4..(end + 1) * 4],
            }
        } else {
            SourceChunk::Added(bytes)
        })
    }
}

impl<'a> SourceChunks<'a> {
    pub(crate) fn new(tree: &'a Tree, range: Range<u64>) -> Self {
        Self { chunks: Chunks::new(tree, range), detached: None }
    }
}

impl<'a> Iterator for SourceChunks<'a> {
    type Item = Result<SourceChunk<'a>, ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(cursor) = &mut self.detached {
            if let Some(chunk) = cursor.next() {
                return Some(Ok(chunk));
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
                    let result = piece.storage.bytes().and_then(|bytes| {
                        if let Storage::Opaque { encoding, raw, offsets, .. } = &*piece.storage {
                            let mut cursor = DetachedChunks {
                                encoding: *encoding,
                                bytes,
                                raw: raw.bytes()?,
                                table: offsets.bytes()?,
                                next: start as usize,
                                end: end as usize,
                            };
                            let first = cursor.next().expect("nonempty detached piece range");
                            self.detached = Some(cursor);
                            return Ok(first);
                        }
                        let bytes = &bytes[start as usize..end as usize];
                        Ok(match piece.storage.as_ref() {
                            Storage::Original(file) => SourceChunk::Original { file, range: start..end, bytes },
                            Storage::Decoded(source) => {
                                SourceChunk::Decoded { origin: &source.origin, range: start..end, bytes }
                            }
                            _ => SourceChunk::Added(bytes),
                        })
                    });
                    if result.is_err() {
                        self.chunks.stack.clear();
                        self.detached = None;
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
