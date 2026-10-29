//! Frontend-independent editing. Offsets are bytes; user columns share the
//! frontend grapheme/tab grid. Large bulk commands can move this model to a worker.

use crate::{Damage, Document, text};
use kwil_core_commands::{
    Mode, Settings,
    vim::{Invocation, Motion as VimMotion},
};
use kwil_core_text::{Buffer, Snapshot};
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    ops::Range,
    sync::Arc,
    time::Duration,
};

mod commands;
mod compound;
mod formatting;
mod hex;
mod replacement;
use compound::CompoundEdit;
#[cfg(test)]
mod receipt_delivery_tests;
pub(crate) mod receipts;
pub(crate) mod save;
pub use save::{SaveCompletionRefusal, SaveCompletionRefusalReason};
#[cfg(test)]
mod save_completion_tests;
pub mod visual;
pub(crate) use receipts::ReceiptDeliveryRefusal;
use receipts::{EditKind, ReceiptState, Recipe, Transition};
pub use receipts::{EditObserverRefusal, EditObserverRefusalReason};

const MAX_CURSORS: usize = 4096;
const BURST_GAP: Duration = Duration::from_millis(500);

fn metadata_overflow() -> io::Error {
    // ErrorKind-only construction keeps the read-only inspector allocation
    // free even on refusal; diagnostic context belongs to its caller.
    io::Error::from(io::ErrorKind::InvalidData)
}
fn metadata_add(total: &mut usize, bytes: usize) -> io::Result<()> {
    *total = total.checked_add(bytes).ok_or_else(metadata_overflow)?;
    Ok(())
}
fn metadata_capacity<T>(capacity: usize) -> io::Result<usize> {
    capacity.checked_mul(std::mem::size_of::<T>()).ok_or_else(metadata_overflow)
}
fn invocation_metadata_bytes(call: &Invocation) -> io::Result<usize> {
    use kwil_core_commands::vim::Argument;
    match &call.argument {
        Argument::Search(pattern) => Ok(pattern.capacity()),
        Argument::Substitute { pattern, replacement, .. } => {
            pattern.capacity().checked_add(replacement.capacity()).ok_or_else(metadata_overflow)
        }
        _ => Ok(0),
    }
}
fn register_entry_metadata_bytes(data: &ClipboardData) -> io::Result<usize> {
    // Conservative node bound for the pinned BTreeMap implementation: a full
    // node per ENTRY, allowing 16 key/value slots and 32 pointer-sized header/
    // edge slots (the implementation uses 11 entries and 12 child edges).
    // Every nonempty node owns at least one entry. This is an allocation upper
    // bound, not a measurement of physical nodes or RSS. Snapshot payloads keep
    // their existing source accounting; only inline handles/ranges count here.
    let node = std::mem::size_of::<(char, ClipboardData)>()
        .checked_mul(16)
        .and_then(|n| n.checked_add(32 * std::mem::size_of::<usize>()))
        .ok_or_else(metadata_overflow)?;
    node.checked_add(metadata_capacity::<Range<u64>>(data.ranges.capacity())?).ok_or_else(metadata_overflow)
}
fn register_map_metadata_bytes(registers: &BTreeMap<char, ClipboardData>) -> io::Result<usize> {
    // Called only when an existing compound transaction CLONES its rollback
    // map: cloned range vectors can have different capacities. Never called by
    // the document-response inspector.
    let mut bytes = 0;
    for data in registers.values() {
        metadata_add(&mut bytes, register_entry_metadata_bytes(data)?)?;
    }
    Ok(bytes)
}

#[cfg(test)]
mod document_metadata_tests {
    use super::*;

    #[test]
    fn register_scalar_tracks_all_vim_write_paths_and_actual_range_capacities() {
        let mut editor = Editor::new(Document::new());
        editor.insert_text("abcd", Duration::ZERO).unwrap();
        editor.set_cursors(vec![Cursor::caret(0)], 0).unwrap();
        for register in ['a', 'a', 'A', 'b', '_'] {
            let call = Invocation {
                id: "operator.yank",
                count: 1,
                explicit_count: false,
                motion: Some(VimMotion::Right),
                register: Some(register),
                argument: kwil_core_commands::vim::Argument::None,
            };
            editor.execute_vim(&call).unwrap();
            assert_eq!(
                editor.register_metadata_bytes,
                register_map_metadata_bytes(&editor.registers).unwrap(),
                "cache follows overwrite, append, named and unnamed register writes"
            );
        }
        assert!(editor.register_metadata_bytes > 0);
        let before = editor.register_metadata_bytes;
        let mut data = ClipboardData::text(b"z").unwrap();
        data.ranges.reserve(64);
        let expected = register_entry_metadata_bytes(&data).unwrap();
        let replaced = register_entry_metadata_bytes(editor.registers.get(&'a').unwrap()).unwrap();
        editor.install_register('a', data).unwrap();
        assert_eq!(editor.register_metadata_bytes, before - replaced + expected);
        assert_eq!(editor.register_metadata_bytes, register_map_metadata_bytes(&editor.registers).unwrap());
    }

    #[test]
    fn inspector_excludes_nested_history_with_existing_buffer_charge_witness() {
        let mut editor = Editor::new(Document::new());
        editor.insert_text("x", Duration::ZERO).unwrap();
        let metadata = editor.document_metadata_bytes().unwrap();
        let history = editor.document.buffer.undo_memory_bytes();
        editor.undo.back_mut().unwrap().before.cursors.reserve(256);
        editor.charge_model_history();
        assert!(
            editor.document.buffer.undo_memory_bytes() > history,
            "nested history metadata is charged by the document history owner"
        );
        assert_eq!(
            editor.document_metadata_bytes().unwrap(),
            metadata,
            "aggregate counts deque allocation headers without charging nested history twice"
        );
    }
}

// One normalization algorithm serves ordinary edits and guarded save staging.
fn normalize_cursor_set(cursors: &mut Vec<Cursor>, primary: &mut usize) {
    let selected = cursors[*primary].clone();
    cursors.sort_by_key(|c| (c.selection().start, c.selection().end));
    let mut output = 0;
    let mut selected_index = 0;
    for index in 0..cursors.len() {
        let cursor = cursors[index].clone();
        let is_primary = cursor == selected;
        if output > 0
            && (cursor.selection().start < cursors[output - 1].selection().end
                || cursor.selection() == cursors[output - 1].selection())
        {
            let last = &mut cursors[output - 1];
            let start = last.selection().start.min(cursor.selection().start);
            let end = last.selection().end.max(cursor.selection().end);
            let reverse = if is_primary { cursor.head < cursor.anchor } else { last.head < last.anchor };
            last.anchor = if reverse { end } else { start };
            last.head = if reverse { start } else { end };
            if is_primary {
                selected_index = output - 1;
            }
        } else {
            if is_primary {
                selected_index = output;
            }
            cursors[output] = cursor;
            output += 1;
        }
    }
    cursors.truncate(output);
    *primary = selected_index.min(output - 1);
}

enum CursorDamageBefore<'a> {
    Rows(&'a [u64]),
    Source { snapshot: &'a Snapshot, cursors: &'a [Cursor], top: u64 },
}
fn visit_cursor_rows(
    snapshot: &Snapshot,
    cursors: &[Cursor],
    top: u64,
    rows: usize,
    visit: &mut impl FnMut(u64),
) -> io::Result<()> {
    for cursor in cursors {
        let start = text::row(snapshot, cursor.selection().start)?;
        let end = text::row(snapshot, cursor.selection().end)?;
        visit(start);
        visit(end);
        for row in start.max(top)..=end.min(top.saturating_add(rows as u64)) {
            visit(row);
        }
    }
    Ok(())
}
/// Reads/reveal/damage are shared by legacy publication and sealed preparation.
/// The latter passes only staged scalars, cursors and borrowed guarded sources.
fn evaluate_cursor_damage(
    snapshot: &Snapshot,
    cursors: &[Cursor],
    primary: usize,
    top: &mut u64,
    visible_rows: usize,
    before: CursorDamageBefore<'_>,
    edited: Option<&Range<u64>>,
) -> io::Result<Damage> {
    let row = text::row(snapshot, cursors[primary].head)?;
    let previous_top = *top;
    if visible_rows > 0 {
        if row < *top {
            *top = row;
        } else if row >= top.saturating_add(visible_rows as u64) {
            *top = row + 1 - visible_rows as u64;
        }
    }
    let delta = *top as i128 - previous_top as i128;
    let mut damage = Damage { status: true, ..Damage::default() };
    if delta.unsigned_abs() >= visible_rows.max(1) as u128 {
        damage.full = true;
        return Ok(damage);
    }
    damage.scroll = delta as i64;
    let mut rows = save::reserve_vec::<bool>(visible_rows)?;
    rows.resize(visible_rows, false);
    {
        let mut mark = |row: u64| {
            if row >= *top && row - *top < visible_rows as u64 {
                rows[(row - *top) as usize] = true;
            }
        };
        match before {
            CursorDamageBefore::Rows(old) => {
                for row in old {
                    mark(*row);
                }
            }
            CursorDamageBefore::Source { snapshot, cursors, top } => {
                visit_cursor_rows(snapshot, cursors, top, visible_rows, &mut mark)?
            }
        }
        visit_cursor_rows(snapshot, cursors, *top, visible_rows, &mut mark)?;
    }
    for (index, changed) in rows.iter_mut().enumerate() {
        if edited.is_some_and(|range| range.contains(&top.saturating_add(index as u64)))
            || delta > 0 && index >= visible_rows - delta as usize
            || delta < 0 && index < (-delta) as usize
        {
            *changed = true;
        }
    }
    let mut result = save::reserve_vec(visible_rows)?;
    result.extend(rows.into_iter().enumerate().filter_map(|(index, changed)| changed.then_some(index)));
    damage.rows = result;
    Ok(damage)
}

#[cfg(test)]
mod workspace_wrap_cursor_tests {
    use super::*;

    #[test]
    fn workspace_wrap_external_normalization_restore_and_exhaustion_are_explicit() {
        let mut editor = Editor::new(Document::new());
        let initial = editor.external_cursor_generation().unwrap();
        editor.set_viewport(0, 2);
        assert_eq!(editor.external_cursor_generation().unwrap(), initial);
        editor.normalize();
        assert!(editor.external_cursor_generation().unwrap() > initial);
        let before_restore = editor.external_cursor_generation().unwrap();
        let state = editor.state();
        editor.restore(state);
        assert_eq!(editor.primary_cursor(), &Cursor::caret(0));
        assert!(editor.external_cursor_generation().unwrap() > before_restore);
        editor.external_cursor_generation = Some(u64::MAX);
        editor.normalize();
        assert!(editor.external_cursor_generation().is_err());
        editor.restore(editor.state());
        assert!(editor.external_cursor_generation().is_err(), "exhausted identities never restart");
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub anchor: u64,
    pub head: u64,
    pub desired_column: Option<u64>,
}
impl Cursor {
    pub fn caret(at: u64) -> Self {
        Self { anchor: at, head: at, desired_column: None }
    }
    pub fn selection(&self) -> Range<u64> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }
    pub fn is_caret(&self) -> bool {
        self.anchor == self.head
    }
}

/// Snapshot-backed clipboard payload. Frontends stream write_to to their OS
/// clipboard or spill file; large selections never need a contiguous String.
#[derive(Clone)]
pub struct ClipboardData {
    pub snapshot: Snapshot,
    pub ranges: Vec<Range<u64>>,
    pub linewise: bool,
}
impl ClipboardData {
    pub fn text(bytes: &[u8]) -> io::Result<Self> {
        let mut buffer = Buffer::empty();
        buffer.insert(0, bytes)?;
        Ok(Self {
            snapshot: buffer.snapshot(),
            ranges: std::iter::once(0..bytes.len() as u64).collect(),
            linewise: false,
        })
    }
    pub fn write_to(&self, writer: &mut impl io::Write) -> io::Result<()> {
        for (index, range) in self.ranges.iter().enumerate() {
            if index > 0 && !self.linewise {
                writer.write_all(b"\n")?;
            }
            for chunk in self.snapshot.chunks(range.clone())? {
                writer.write_all(chunk.map_err(io::Error::other)?)?;
            }
        }
        Ok(())
    }
    fn joined(&self, ending: &[u8]) -> io::Result<Snapshot> {
        let mut joined = Snapshot::default();
        let separator = literal_snapshot(ending)?;
        for (i, range) in self.ranges.iter().enumerate() {
            if i > 0 && !self.linewise {
                joined = joined.concat(&separator)?;
            }
            joined = joined.concat(&self.snapshot.slice(range.clone())?)?;
        }
        Ok(joined)
    }
}

pub trait Clipboard {
    fn set(&mut self, data: ClipboardData) -> io::Result<()>;
    fn get(&mut self) -> io::Result<Option<ClipboardData>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    WordForward,
    WordBackward,
    WordEnd,
    SmartHome,
    LineStart,
    LineEnd,
    DocumentStart,
    DocumentEnd,
    PageUp,
    PageDown,
}

#[derive(Clone, Debug, Default)]
pub struct CommandArgs {
    pub text: Option<String>,
    /// One-based line and column (palette/CLI convention).
    pub line: Option<u64>,
    pub column: Option<u64>,
    pub extend: bool,
    pub cursor: Option<Cursor>,
    /// Zero-based (anchor row, anchor column, head row, head column).
    pub column_selection: Option<(u64, u64, u64, u64)>,
    pub at: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandStatus {
    Applied,
    NotImplemented(String),
    Message(String),
    SaveRequested,
    IndexNotReady,
}

#[derive(Clone, Debug)]
pub struct CommandResult {
    pub damage: Damage,
    pub status: CommandStatus,
}

#[derive(Clone)]
struct State {
    cursors: Vec<Cursor>,
    primary: usize,
    revision: u64,
    box_column: Option<u64>,
}
struct HistoryEntry {
    before: State,
    after: State,
    edited: Range<u64>,
    structural: bool,
    transition: Transition,
}

#[derive(Clone)]
enum Insert {
    Bytes(Arc<[u8]>),
    Pieces(Snapshot, Range<u64>),
    Padded(u64, Arc<[u8]>),
}
impl Insert {
    fn len(&self) -> u64 {
        match self {
            Self::Bytes(b) => b.len() as u64,
            Self::Pieces(_, r) => r.end - r.start,
            Self::Padded(p, b) => p + b.len() as u64,
        }
    }
    fn apply(&self, buffer: &mut Buffer, at: u64) -> io::Result<()> {
        match self {
            Self::Bytes(bytes) => buffer.insert(at, bytes),
            Self::Pieces(source, range) => buffer.insert_snapshot_range(at, source, range.clone()),
            Self::Padded(padding, bytes) => {
                static SPACES: [u8; 64 << 10] = [b' '; 64 << 10];
                buffer.insert(at, &SPACES[..*padding as usize])?;
                buffer.insert(at + padding, bytes)
            }
        }
    }
}
#[derive(Clone)]
struct Edit {
    range: Range<u64>,
    insert: Insert,
}
impl Edit {
    fn is_noop(&self) -> bool {
        self.range.is_empty() && self.insert.len() == 0
    }
    fn bytes(range: Range<u64>, bytes: &[u8]) -> Self {
        Self { range, insert: Insert::Bytes(Arc::from(bytes)) }
    }
    fn delete(range: Range<u64>) -> Self {
        Self::bytes(range, b"")
    }
}

#[derive(Clone)]
enum Repeat {
    Insert(Vec<u8>),
    Vim(Invocation, Vec<u8>),
}

pub struct Editor {
    pub(crate) document: Document,
    cursors: Vec<Cursor>,
    // A wrap-created cursor allocation remains charged while Editor owns it,
    // independently of transient visual display state.
    visual_cursor_charge: Option<crate::visual_rows::AllocationCharge>,
    // External normalization/restore is distinct from atomic wrap publication,
    // including normalization that preserves every byte endpoint.
    external_cursor_generation: Option<u64>,
    primary: usize,
    top: u64,
    rows: usize,
    pub settings: Settings,
    undo: VecDeque<HistoryEntry>,
    redo: VecDeque<HistoryEntry>,
    burst: Option<(Duration, Vec<Cursor>)>,
    pub mode: Mode,
    registers: BTreeMap<char, ClipboardData>,
    // Maintained at each register replacement/clear and compound rollback.
    // Includes conservative map nodes and actual range-vector capacities.
    register_metadata_bytes: usize,
    repeat: Option<Repeat>,
    insert_prefix: Option<Invocation>,
    inserted: Vec<u8>,
    join_insert: bool,
    language: Option<String>,
    comment_override: Option<String>,
    saving: bool,
    box_column: Option<u64>,
    receipts: ReceiptState,
    compound: Option<CompoundEdit>,
}

impl Editor {
    pub fn new(document: Document) -> Self {
        Self {
            document,
            cursors: vec![Cursor::caret(0)],
            visual_cursor_charge: None,
            external_cursor_generation: Some(0),
            primary: 0,
            top: 0,
            rows: 0,
            settings: Settings::default(),
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            burst: None,
            mode: Mode::Normal,
            registers: BTreeMap::new(),
            register_metadata_bytes: 0,
            repeat: None,
            insert_prefix: None,
            inserted: Vec::new(),
            join_insert: false,
            language: None,
            comment_override: None,
            saving: false,
            box_column: None,
            receipts: ReceiptState::default(),
            compound: None,
        }
    }
    pub fn document(&self) -> &Document {
        &self.document
    }

    /// Current non-source model metadata. No history/register traversal,
    /// allocation, source access, or charge release occurs here.
    ///
    /// Dynamic Settings are covered separately by Workspace's borrowed current
    /// Settings measurement. Nested history State/Recipe storage remains in the
    /// document history budget (`charge_model_history`); saved/deferred states
    /// retain their independent reservations. This is not a growth permit.
    pub(crate) fn document_metadata_bytes(&self) -> io::Result<usize> {
        let mut bytes = std::mem::size_of::<Self>();
        metadata_add(&mut bytes, metadata_capacity::<Cursor>(self.cursors.capacity())?)?;
        metadata_add(&mut bytes, metadata_capacity::<HistoryEntry>(self.undo.capacity())?)?;
        metadata_add(&mut bytes, metadata_capacity::<HistoryEntry>(self.redo.capacity())?)?;
        if let Some((_, cursors)) = &self.burst {
            metadata_add(&mut bytes, metadata_capacity::<Cursor>(cursors.capacity())?)?;
        }
        metadata_add(&mut bytes, self.inserted.capacity())?;
        metadata_add(&mut bytes, self.language.as_ref().map_or(0, String::capacity))?;
        metadata_add(&mut bytes, self.comment_override.as_ref().map_or(0, String::capacity))?;
        metadata_add(&mut bytes, self.register_metadata_bytes)?;
        if let Some(call) = &self.insert_prefix {
            metadata_add(&mut bytes, invocation_metadata_bytes(call)?)?;
        }
        if let Some(repeat) = &self.repeat {
            match repeat {
                Repeat::Insert(data) => metadata_add(&mut bytes, data.capacity())?,
                Repeat::Vim(call, data) => {
                    metadata_add(&mut bytes, data.capacity())?;
                    metadata_add(&mut bytes, invocation_metadata_bytes(call)?)?;
                }
            }
        }
        if let Some(compound) = &self.compound {
            metadata_add(&mut bytes, compound.document_metadata_bytes()?)?;
        }
        metadata_add(&mut bytes, self.receipts.document_metadata_bytes()?)?;
        Ok(bytes)
    }

    fn install_register(&mut self, key: char, data: ClipboardData) -> io::Result<()> {
        let incoming = register_entry_metadata_bytes(&data)?;
        let old = self.registers.get(&key).map(register_entry_metadata_bytes).transpose()?.unwrap_or(0);
        let next = self
            .register_metadata_bytes
            .checked_sub(old)
            .and_then(|n| n.checked_add(incoming))
            .ok_or_else(metadata_overflow)?;
        self.registers.insert(key, data);
        self.register_metadata_bytes = next;
        Ok(())
    }

    /// Cached scalar only. Exhaustion disables wrap admission explicitly;
    /// generations never wrap and borrowing an editor does not advance them.
    pub fn external_cursor_generation(&self) -> io::Result<u64> {
        self.external_cursor_generation.ok_or_else(|| io::Error::other("external cursor generation exhausted"))
    }
    fn note_external_cursor_change(&mut self) {
        self.external_cursor_generation = self.external_cursor_generation.and_then(|value| value.checked_add(1));
    }

    /// Settle a failed save only after Workspace validates its active flight.
    /// The caller retains the error and every source-bearing owner.
    pub(crate) fn fail_save_completion(&mut self) {
        self.saving = false;
    }

    /// Capture raw Save/Save As through the same in-flight save state. The
    /// worker's SaveRaw request independently enforces these transport rules.
    pub fn raw_save_request(
        &mut self,
        path: Option<std::path::PathBuf>,
    ) -> io::Result<kwil_core_workspace::SaveRequest> {
        if self.document.representation != crate::hex_session::Representation::RawBytes {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "raw save requires RawBytes transport"));
        }
        let mut request = self.save_request(path)?;
        request.trim_trailing_whitespace = false;
        request.compression = kwil_core_workspace::compressed::CompressionFormat::Raw;
        Ok(request)
    }

    pub(crate) fn raw_reopen_ready(&self) -> io::Result<()> {
        if self.saving
            || self.has_pending_edit_receipt()
            || self.has_pending_save_completion()
            || self.compound.is_some()
        {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "raw reopen awaits admitted editor work"));
        }
        self.receipts.destination_change_ready()?;
        Ok(())
    }

    /// Called only on the document worker. All old roots/history retire there;
    /// no state is changed until the new physical document has been prepared.
    pub(crate) fn install_raw_reopen(&mut self, document: Document) {
        self.receipts.destination_change_ready().expect("worker raw reopen destination preflight");
        self.note_external_cursor_change();
        self.end_burst();
        self.undo.clear();
        self.redo.clear();
        self.registers.clear();
        self.register_metadata_bytes = 0;
        self.repeat = None;
        self.insert_prefix = None;
        self.inserted.clear();
        self.join_insert = false;
        self.receipts.reset_for_raw_reopen();
        self.box_column = None;
        self.cursors = vec![Cursor::caret(0)];
        self.primary = 0;
        self.top = 0;
        self.document = document;
    }
    pub fn cursors(&self) -> &[Cursor] {
        &self.cursors
    }
    pub fn primary_index(&self) -> usize {
        self.primary
    }
    pub fn primary_cursor(&self) -> &Cursor {
        &self.cursors[self.primary]
    }
    pub fn top(&self) -> u64 {
        self.top
    }
    pub fn rows(&self) -> usize {
        self.rows
    }
    /// Zero-based row and display column (graphemes and configured tab stops).
    /// Scanning a column is bounded to 64 KiB; long-line frontends can use
    /// byte_position/line_range and their bounded horizontal layout instead.
    pub fn position_at(&self, offset: u64) -> io::Result<(u64, u64)> {
        kwil_core_text::with_checked_read_batch(|| {
            let s = self.document.snapshot();
            Ok((text::row(&s, offset)?, text::column(&s, offset, self.settings.editor.tab_size)?))
        })
    }
    pub fn byte_position(&self, offset: u64) -> io::Result<(u64, u64)> {
        self.document
            .buffer
            .offset_to_line_col(offset)
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "offset outside document"))
    }
    pub fn offset_at(&self, row: u64, column: u64) -> io::Result<u64> {
        let s = self.document.snapshot();
        text::at_column(&s, row.min(s.line_count() - 1), column, self.settings.editor.tab_size)
    }
    pub fn line_range(&self, row: u64) -> io::Result<Range<u64>> {
        text::line(&self.document.snapshot(), row)
    }
    /// Absolute byte ranges clipped to a line. Frontends intersect these with
    /// their horizontal window; the returned count is bounded by cursor count.
    pub fn selection_ranges(&self, row: u64) -> io::Result<Vec<Range<u64>>> {
        let line = self.line_range(row)?;
        Ok(self
            .cursors
            .iter()
            .filter_map(|c| {
                let r = c.selection();
                let a = r.start.max(line.start);
                let b = r.end.min(line.end);
                (a < b).then_some(a..b)
            })
            .collect())
    }
    pub fn set_viewport(&mut self, top: u64, rows: usize) {
        self.top = top.min(self.document.buffer.line_count().saturating_sub(rows as u64));
        self.rows = rows;
    }
    pub fn set_language(&mut self, language: Option<String>) {
        self.language = language;
    }
    /// core-syntax detection can supply a token without a dependency cycle.
    pub fn set_line_comment_token(&mut self, token: Option<String>) {
        self.comment_override = token;
    }
    pub fn is_markdown(&self) -> bool {
        self.language.as_deref().is_some_and(|s| matches!(s, "markdown" | "md"))
            || self
                .document
                .path()
                .and_then(|p| p.extension())
                .and_then(|e| e.to_str())
                .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "md" | "markdown" | "mdown" | "mkd"))
    }
    fn state(&self) -> State {
        State {
            cursors: self.cursors.clone(),
            primary: self.primary,
            revision: self.document.revision,
            box_column: self.box_column,
        }
    }
    fn restore(&mut self, state: State) {
        self.note_external_cursor_change();
        self.cursors = state.cursors;
        self.primary = state.primary;
        self.document.revision = state.revision;
        self.box_column = state.box_column;
    }
    fn end_burst(&mut self) {
        self.burst = None;
        self.document.buffer.end_typing_burst();
    }

    pub fn set_cursors(&mut self, cursors: Vec<Cursor>, primary: usize) -> io::Result<Damage> {
        if cursors.is_empty()
            || cursors.len() > MAX_CURSORS
            || primary >= cursors.len()
            || cursors.iter().any(|c| c.anchor > self.document.buffer.len() || c.head > self.document.buffer.len())
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid cursor set (maximum 4096)"));
        }
        let old = self.cursor_rows()?;
        self.end_burst();
        self.cursors = cursors;
        self.primary = primary;
        self.box_column = None;
        self.normalize();
        self.finish_damage(old, None)
    }

    fn normalize(&mut self) {
        self.note_external_cursor_change();
        normalize_cursor_set(&mut self.cursors, &mut self.primary);
    }

    fn cursor_rows(&self) -> io::Result<Vec<u64>> {
        let s = self.document.snapshot();
        let mut result = Vec::with_capacity(self.cursors.len() * 2 + self.rows);
        visit_cursor_rows(&s, &self.cursors, self.top, self.rows, &mut |row| result.push(row))?;
        Ok(result)
    }

    fn finish_damage(&mut self, old: Vec<u64>, edited: Option<Range<u64>>) -> io::Result<Damage> {
        let s = self.document.snapshot();
        evaluate_cursor_damage(
            &s,
            &self.cursors,
            self.primary,
            &mut self.top,
            self.rows,
            CursorDamageBefore::Rows(&old),
            edited.as_ref(),
        )
    }

    fn apply(&mut self, mut edits: Vec<Edit>, typing: Option<Duration>, caret_at_end: bool) -> io::Result<Damage> {
        kwil_core_text::with_checked_read_batch(|| self.apply_checked(&mut edits, typing, caret_at_end))
    }

    fn apply_checked(
        &mut self,
        edits: &mut Vec<Edit>,
        typing: Option<Duration>,
        caret_at_end: bool,
    ) -> io::Result<Damage> {
        self.admit_edit()?;
        edits.retain(|e| !e.range.is_empty() || e.insert.len() > 0);
        if edits.is_empty() {
            return Ok(Damage::default());
        }
        edits.sort_by_key(|e| (e.range.start, e.range.end));
        // Merge intersecting deletion ranges; equal carets are normalized first.
        let mut plan: Vec<Edit> = Vec::with_capacity(edits.len());
        for edit in edits.drain(..) {
            if let Some(last) = plan.last_mut()
                && edit.range.start < last.range.end
            {
                if last.insert.len() != 0 || edit.insert.len() != 0 {
                    return Err(io::Error::other("overlapping replacements"));
                }
                last.range.end = last.range.end.max(edit.range.end);
            } else {
                plan.push(edit);
            }
        }
        let snapshot = self.document.snapshot();
        let old_rows = self.cursor_rows()?;
        let before = self.state();
        let first_row = text::row(&snapshot, plan[0].range.start)?;
        let last_row = text::row(&snapshot, plan.last().unwrap().range.end)?;
        let recipe = Recipe::explicit(&plan);
        let compound = self.compound.is_some();
        if let Some(compound) = &mut self.compound {
            compound.add(&plan)?;
        }
        let merged_recipe = self.undo.back().and_then(|entry| entry.transition.recipe.coalesced(&recipe));
        let coalesce = typing.is_some_and(|at| {
            self.join_insert
                || self.burst.as_ref().is_some_and(|(previous, cursors)| {
                    *cursors == self.cursors && at.checked_sub(*previous).is_some_and(|gap| gap <= BURST_GAP)
                })
        }) && merged_recipe.is_some();
        if !compound {
            self.document.buffer.begin();
        }
        for edit in plan.iter().rev() {
            let result = (|| {
                self.document.buffer.delete(edit.range.clone())?;
                edit.insert.apply(&mut self.document.buffer, edit.range.start)
            })();
            if let Err(error) = result {
                if !compound {
                    self.document.buffer.cancel();
                }
                return Err(error);
            }
        }
        if !compound {
            self.document.buffer.commit_coalesced(coalesce);
        }
        for c in &mut self.cursors {
            if caret_at_end {
                let selection = c.selection();
                let found = plan.iter().position(|e| e.range == selection).or_else(|| {
                    plan.iter().position(|e| e.range.start <= selection.start && selection.end <= e.range.end)
                });
                let end = if let Some(index) = found {
                    let shift: i128 = plan[..index]
                        .iter()
                        .map(|e| e.insert.len() as i128 - (e.range.end - e.range.start) as i128)
                        .sum();
                    (plan[index].range.start as i128 + shift + plan[index].insert.len() as i128) as u64
                } else {
                    transform(c.head, &plan, true)
                };
                c.anchor = end;
                c.head = end;
            } else {
                c.anchor = transform(c.anchor, &plan, false);
                c.head = transform(c.head, &plan, false);
            }
            c.desired_column = None;
        }
        self.normalize();
        if !compound {
            self.document.changed();
        }
        self.box_column = None;
        if compound {
            let delta = self.document.buffer.line_count() as i128 - snapshot.line_count() as i128;
            let end = if delta != 0 { self.top + self.rows as u64 } else { last_row + 1 };
            return self.finish_damage(old_rows, Some(first_row..end));
        }
        let after = self.state();
        let transition = Transition { before: snapshot.clone(), after: self.document.snapshot(), recipe };
        let structural = snapshot.line_count() != self.document.buffer.line_count();
        self.redo.clear();
        if coalesce {
            let entry = self.undo.back_mut().unwrap();
            entry.after = after;
            entry.edited.start = entry.edited.start.min(first_row);
            entry.edited.end = entry.edited.end.max(last_row + 1);
            entry.structural |= structural;
            entry.transition.after = transition.after.clone();
            entry.transition.recipe = merged_recipe.unwrap();
        } else {
            self.undo.push_back(HistoryEntry {
                before: before.clone(),
                after,
                edited: first_row..last_row + 1,
                structural,
                transition: transition.clone(),
            });
        }
        self.charge_model_history();
        self.publish(transition, before.revision, self.document.revision, EditKind::Edit);
        self.burst = typing.map(|at| (at, self.cursors.clone()));
        if typing.is_some() {
            self.join_insert = self.mode == Mode::Insert;
        }
        let after_snapshot = self.document.snapshot();
        let line_delta = after_snapshot.line_count() as i128 - snapshot.line_count() as i128;
        let end = if line_delta != 0 { self.top + self.rows as u64 } else { last_row + 1 };
        self.finish_damage(old_rows, Some(first_row..end))
    }

    fn trim_history(&mut self) {
        let (undo, redo) = self.document.buffer.history_depth();
        while self.undo.len() > undo {
            self.undo.pop_front();
        }
        while self.redo.len() > redo {
            self.redo.pop_front();
        }
    }

    /// Streaming bulk edits arrive in descending original offsets. Only one
    /// line's recipe and the cursor set are resident, even for select-all.
    #[cfg(test)]
    fn apply_descending<I>(&mut self, edits: I) -> io::Result<Damage>
    where
        I: DoubleEndedIterator<Item = io::Result<Option<Edit>>> + Clone + Send + Sync + 'static,
    {
        // Compatibility for private/test callers whose captures are unknown.
        // A retained receipt from this route explicitly refuses metadata
        // admission until its producer supplies a certified heap bound.
        self.apply_descending_with_recipe(edits, None, None)
    }

    /// A bounded prepared shape recipe must not retain a byte-producing
    /// iterator (for example an inline search plan's replacement payloads).
    fn apply_descending_with_recipe<I>(
        &mut self,
        edits: I,
        prepared: Option<Recipe>,
        seed_heap_bytes: Option<usize>,
    ) -> io::Result<Damage>
    where
        I: DoubleEndedIterator<Item = io::Result<Option<Edit>>> + Clone + Send + Sync + 'static,
    {
        if self.compound.is_some() {
            return Err(io::Error::other("bulk edits cannot nest inside a compound repeat"));
        }
        self.admit_edit()?;
        // Application, the retained bulk factory and its replacement count
        // must all see the same effective edits. Empty-to-empty records are
        // identities, not model transitions that can outrun core history.
        let edits = edits.map(|result| result.map(|edit| edit.filter(|edit| !edit.is_noop())));
        let recipe_seed = edits.clone();
        let mut replacement_count = 0;
        let before = self.state();
        let old = self.cursor_rows()?;
        let s = self.document.snapshot();
        let mut cursors = self.cursors.clone();
        let mut changed = false;
        let mut first_row = u64::MAX;
        let mut last_row = 0;
        let mut began = false;
        for edit in edits {
            let result = (|| {
                let Some(edit) = edit? else {
                    return Ok(());
                };
                first_row = first_row.min(text::row(&s, edit.range.start)?);
                last_row = last_row.max(text::row(&s, edit.range.end)?);
                if !began {
                    self.end_burst();
                    self.document.buffer.begin();
                    began = true;
                }
                self.document.buffer.delete(edit.range.clone())?;
                edit.insert.apply(&mut self.document.buffer, edit.range.start)?;
                for c in &mut cursors {
                    c.anchor = transform(c.anchor, std::slice::from_ref(&edit), false);
                    c.head = transform(c.head, std::slice::from_ref(&edit), false);
                    c.desired_column = None;
                }
                changed = true;
                replacement_count += 1;
                Ok(())
            })();
            if let Err(error) = result {
                if began {
                    self.document.buffer.cancel();
                }
                return Err(error);
            }
        }
        if !changed {
            return Ok(Damage::default());
        }
        self.document.buffer.commit();
        self.cursors = cursors;
        self.normalize();
        self.document.changed();
        self.box_column = None;
        self.redo.clear();
        let transition = Transition {
            before: s.clone(),
            after: self.document.snapshot(),
            recipe: prepared.unwrap_or_else(|| Recipe::bulk(recipe_seed, replacement_count, seed_heap_bytes)),
        };
        self.undo.push_back(HistoryEntry {
            before: before.clone(),
            after: self.state(),
            edited: first_row..last_row + 1,
            structural: s.line_count() != self.document.buffer.line_count(),
            transition: transition.clone(),
        });
        self.charge_model_history();
        self.publish(transition, before.revision, self.document.revision, EditKind::Edit);
        self.finish_damage(old, Some(first_row..last_row + 1))
    }
    pub fn set_undo_memory_limit(&mut self, bytes: usize) {
        self.document.set_undo_memory_limit(bytes);
        self.trim_history();
    }

    pub fn insert_text(&mut self, text: &str, at: Duration) -> io::Result<Damage> {
        kwil_core_text::with_checked_read_batch(|| self.insert_bytes(text.as_bytes(), Some(at)))
    }
    fn insert_bytes(&mut self, bytes: &[u8], at: Option<Duration>) -> io::Result<Damage> {
        let continuing = at.is_some_and(|at| {
            self.burst.as_ref().is_some_and(|(previous, cursors)| {
                *cursors == self.cursors && at.checked_sub(*previous).is_some_and(|gap| gap <= BURST_GAP)
            })
        });
        let data: Arc<[u8]> = Arc::from(bytes);
        let snapshot = self.document.snapshot();
        let edits = self
            .cursors
            .iter()
            .map(|c| {
                let padding = if let Some(column) = self.box_column {
                    column.saturating_sub(text::column(&snapshot, c.selection().start, self.settings.editor.tab_size)?)
                } else {
                    0
                };
                Ok(Edit {
                    range: c.selection(),
                    insert: if padding > 0 {
                        Insert::Padded(padding, data.clone())
                    } else {
                        Insert::Bytes(data.clone())
                    },
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        let d = self.apply(edits, at, true)?;
        if !bytes.is_empty() {
            if self.mode == Mode::Insert {
                if self.inserted.len() + bytes.len() <= 8 << 20 {
                    self.inserted.extend_from_slice(bytes);
                } else {
                    self.insert_prefix = None;
                    self.repeat = None;
                }
            } else if at.is_some() {
                if let Some(Repeat::Insert(previous)) = &mut self.repeat
                    && continuing
                {
                    if previous.len() + bytes.len() <= 8 << 20 {
                        previous.extend_from_slice(bytes);
                    }
                } else {
                    self.repeat = Some(Repeat::Insert(bytes.to_vec()));
                }
            }
        }
        Ok(d)
    }

    pub fn newline(&mut self) -> io::Result<Damage> {
        let s = self.document.snapshot();
        let mut edits = Vec::new();
        for c in &self.cursors {
            let mut bytes = self.document.line_ending.bytes().to_vec();
            if self.settings.editor.auto_indent {
                let bounds = text::line(&s, text::row(&s, c.selection().start)?)?;
                let sample = text::read(&s, bounds.start..bounds.end.min(bounds.start + text::WINDOW), text::WINDOW)?;
                let mut columns = 0;
                let tab = usize::from(self.settings.editor.tab_size.max(1));
                for b in sample.iter().take_while(|&&b| b == b' ' || b == b'\t') {
                    columns += if *b == b'\t' { tab - columns % tab } else { 1 };
                }
                if self.settings.editor.insert_spaces {
                    bytes.extend(std::iter::repeat_n(b' ', columns));
                } else {
                    bytes.extend(std::iter::repeat_n(b'\t', columns / tab));
                    bytes.extend(std::iter::repeat_n(b' ', columns % tab));
                }
            }
            edits.push(Edit::bytes(c.selection(), &bytes));
        }
        self.apply(edits, None, true)
    }

    pub fn delete(&mut self, backwards: bool, word: bool) -> io::Result<Damage> {
        let s = self.document.snapshot();
        let edits = self
            .cursors
            .iter()
            .map(|c| {
                let range = if !c.is_caret() {
                    c.selection()
                } else if backwards {
                    (if word { text::word_backward(&s, c.head)? } else { text::previous(&s, c.head)? })..c.head
                } else {
                    c.head..if word { text::word_forward(&s, c.head)? } else { text::next(&s, c.head)? }
                };
                Ok(Edit::delete(range))
            })
            .collect::<io::Result<Vec<_>>>()?;
        self.apply(edits, None, true)
    }

    pub fn undo(&mut self) -> io::Result<Damage> {
        self.history(false)
    }
    pub fn redo(&mut self) -> io::Result<Damage> {
        self.history(true)
    }
    fn history(&mut self, redo: bool) -> io::Result<Damage> {
        self.admit_edit()?;
        let before_revision = self.document.revision;
        let old = self.cursor_rows()?;
        self.end_burst();
        self.trim_history();
        let changed = if redo { self.document.buffer.redo() } else { self.document.buffer.undo() };
        if !changed {
            return Ok(Damage::default());
        }
        let mut edited = self.top..self.top + self.rows as u64;
        if redo {
            if let Some(entry) = self.redo.pop_back() {
                if !entry.structural {
                    edited = entry.edited.clone();
                }
                self.restore(entry.after.clone());
                self.publish(entry.transition.clone(), before_revision, self.document.revision, EditKind::Redo);
                self.undo.push_back(entry);
            }
        } else if let Some(entry) = self.undo.pop_back() {
            if !entry.structural {
                edited = entry.edited.clone();
            }
            self.restore(entry.before.clone());
            self.publish(entry.transition.clone(), before_revision, self.document.revision, EditKind::Undo);
            self.redo.push_back(entry);
        }
        self.finish_damage(old, Some(edited))
    }

    pub fn move_cursors(&mut self, motion: Motion, count: u32, extend: bool) -> io::Result<Damage> {
        kwil_core_text::with_checked_read_batch(|| self.move_cursors_checked(motion, count, extend))
    }
    fn move_cursors_checked(&mut self, motion: Motion, count: u32, extend: bool) -> io::Result<Damage> {
        let old = self.cursor_rows()?;
        // A later source lookup may fail after earlier cursors moved.
        self.note_external_cursor_change();
        self.end_burst();
        let s = self.document.snapshot();
        for c in &mut self.cursors {
            let vertical = matches!(motion, Motion::Up | Motion::Down | Motion::PageUp | Motion::PageDown);
            let desired = if vertical {
                Some(c.desired_column.unwrap_or(text::column(&s, c.head, self.settings.editor.tab_size)?))
            } else {
                None
            };
            let mut head = c.head;
            if !extend && !c.is_caret() && matches!(motion, Motion::Left | Motion::Right) {
                head = if motion == Motion::Left { c.selection().start } else { c.selection().end };
            } else {
                head =
                    motion_target(&s, head, motion, count.max(1), desired, self.rows, self.settings.editor.tab_size)?;
            }
            c.head = head;
            if !extend {
                c.anchor = head;
            }
            c.desired_column = desired;
        }
        self.normalize();
        self.box_column = None;
        self.finish_damage(old, None)
    }

    pub fn go_to_line(&mut self, line: u64, column: u64, extend: bool) -> io::Result<Damage> {
        let s = self.document.snapshot();
        let row = line.saturating_sub(1).min(s.line_count() - 1);
        let head = text::at_column(&s, row, column.saturating_sub(1), self.settings.editor.tab_size)?;
        let anchor = if extend { self.primary_cursor().anchor } else { head };
        self.set_cursors(vec![Cursor { anchor, head, desired_column: None }], 0)
    }

    pub fn add_cursor_vertical(&mut self, below: bool) -> io::Result<Damage> {
        if self.cursors.len() >= MAX_CURSORS {
            return Err(io::Error::other("maximum cursor count reached"));
        }
        let s = self.document.snapshot();
        let current = self.primary_cursor();
        let row = text::row(&s, current.head)?;
        let target_row = if below { (row + 1).min(s.line_count() - 1) } else { row.saturating_sub(1) };
        let desired = current.desired_column.unwrap_or(text::column(&s, current.head, self.settings.editor.tab_size)?);
        let head = text::at_column(&s, target_row, desired, self.settings.editor.tab_size)?;
        let mut cursors = self.cursors.clone();
        cursors.push(Cursor { desired_column: Some(desired), ..Cursor::caret(head) });
        let primary = cursors.len() - 1;
        self.set_cursors(cursors, primary)
    }

    pub fn column_selection(&mut self, a_row: u64, a_col: u64, h_row: u64, h_col: u64) -> io::Result<Damage> {
        if a_col.max(h_col) > text::WINDOW {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "box column exceeds bounded text window"));
        }
        let s = self.document.snapshot();
        let a_row = a_row.min(s.line_count() - 1);
        let h_row = h_row.min(s.line_count() - 1);
        let (start, end) = (a_row.min(h_row), a_row.max(h_row));
        if end - start >= MAX_CURSORS as u64 {
            return Err(io::Error::other("column selection exceeds 4096 cursors"));
        }
        let cursors = (start..=end)
            .map(|row| {
                Ok(Cursor {
                    anchor: text::at_column(&s, row, a_col, self.settings.editor.tab_size)?,
                    head: text::at_column(&s, row, h_col, self.settings.editor.tab_size)?,
                    desired_column: Some(h_col),
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        let damage = self.set_cursors(cursors, (h_row - start) as usize)?;
        self.box_column = Some(a_col.min(h_col));
        Ok(damage)
    }

    pub fn select_all(&mut self) -> io::Result<Damage> {
        self.set_cursors(vec![Cursor { anchor: 0, head: self.document.buffer.len(), desired_column: None }], 0)
    }
    pub fn select_word(&mut self) -> io::Result<Damage> {
        let s = self.document.snapshot();
        let cursors = self
            .cursors
            .iter()
            .map(|c| {
                let r = text::word_range(&s, c.head, false)?;
                Ok(Cursor { anchor: r.start, head: r.end, desired_column: None })
            })
            .collect::<io::Result<Vec<_>>>()?;
        self.set_cursors(cursors, self.primary)
    }
    pub fn select_line(&mut self) -> io::Result<Damage> {
        let s = self.document.snapshot();
        let cursors = self
            .cursors
            .iter()
            .map(|c| {
                let r = text::line_full(&s, text::row(&s, c.head)?)?;
                Ok(Cursor { anchor: r.start, head: r.end, desired_column: None })
            })
            .collect::<io::Result<Vec<_>>>()?;
        self.set_cursors(cursors, self.primary)
    }

    /// Bounded literal scan, with overlap across piece/chunk boundaries and wrap.
    pub fn add_next_occurrence(&mut self, budget: u64) -> io::Result<CommandResult> {
        if self.primary_cursor().is_caret() {
            return Ok(CommandResult { damage: self.select_word()?, status: CommandStatus::Applied });
        }
        if self.cursors.len() >= MAX_CURSORS {
            return Err(io::Error::other("maximum cursor count reached"));
        }
        let s = self.document.snapshot();
        let range = self.primary_cursor().selection();
        let needle = text::read(&s, range.clone(), text::WINDOW)?;
        let mut left = budget;
        let mut found = None;
        let mut prefix = vec![0; needle.len()];
        for i in 1..needle.len() {
            let mut matched = prefix[i - 1];
            while matched > 0 && needle[i] != needle[matched] {
                matched = prefix[matched - 1];
            }
            if needle[i] == needle[matched] {
                matched += 1;
            }
            prefix[i] = matched;
        }
        for scan in [range.end..s.len(), 0..range.start] {
            let mut at = scan.start;
            let mut matched = 0;
            while at < scan.end && left > 0 {
                let end = (at + text::WINDOW).min(scan.end).min(at + left);
                let bytes = text::read(&s, at..end, text::WINDOW)?;
                for (i, byte) in bytes.into_iter().enumerate() {
                    left -= 1;
                    while matched > 0 && byte != needle[matched] {
                        matched = prefix[matched - 1];
                    }
                    if byte == needle[matched] {
                        matched += 1;
                    }
                    if matched != needle.len() {
                        continue;
                    }
                    let start = at + i as u64 + 1 - needle.len() as u64;
                    matched = prefix[matched - 1];
                    if !self
                        .cursors
                        .iter()
                        .any(|c| c.selection().start < start + needle.len() as u64 && start < c.selection().end)
                    {
                        found = Some(start);
                        break;
                    }
                }
                if found.is_some() {
                    break;
                }
                at = end;
            }
            if found.is_some() {
                break;
            }
        }
        if let Some(start) = found {
            let mut cursors = self.cursors.clone();
            cursors.push(Cursor { anchor: start, head: start + needle.len() as u64, desired_column: None });
            let primary = cursors.len() - 1;
            Ok(CommandResult { damage: self.set_cursors(cursors, primary)?, status: CommandStatus::Applied })
        } else {
            Ok(CommandResult {
                damage: Damage { status: true, ..Damage::default() },
                status: CommandStatus::Message(
                    if left == 0 { "literal search budget exhausted" } else { "no further occurrence" }.into(),
                ),
            })
        }
    }

    fn selected_line_ranges(&self) -> io::Result<Vec<Range<u64>>> {
        let s = self.document.snapshot();
        let mut ranges: Vec<Range<u64>> = Vec::new();
        for c in &self.cursors {
            let selection = c.selection();
            let start = text::row(&s, selection.start)?;
            let mut end = text::row(&s, selection.end)?;
            if !selection.is_empty() && s.line_start(end).map_err(io::Error::other)? == Some(selection.end) {
                end = end.saturating_sub(1);
            }
            if let Some(last) = ranges.last_mut()
                && start <= last.end
            {
                last.end = last.end.max(end + 1);
            } else {
                ranges.push(start..end + 1);
            }
        }
        Ok(ranges)
    }

    pub fn delete_lines(&mut self, count: u32) -> io::Result<Damage> {
        let s = self.document.snapshot();
        let edits = self
            .selected_line_ranges()?
            .into_iter()
            .map(|r| {
                let start = text::line_full(&s, r.start)?.start;
                let end_row = (r.end + u64::from(count.saturating_sub(1))).min(s.line_count());
                let end = s.line_start(end_row).map_err(io::Error::other)?.unwrap_or(s.len());
                let start = if end == s.len() && start > 0 && (start == end || text::byte(&s, end - 1)? != Some(b'\n'))
                {
                    text::previous(&s, start)?
                } else {
                    start
                };
                Ok(Edit::delete(start..end))
            })
            .collect::<io::Result<Vec<_>>>()?;
        self.apply(edits, None, true)
    }

    pub fn duplicate_lines(&mut self) -> io::Result<Damage> {
        let s = self.document.snapshot();
        let mut edits = Vec::new();
        for r in self.selected_line_ranges()? {
            let start = text::line_full(&s, r.start)?.start;
            let end = s.line_start(r.end).map_err(io::Error::other)?.unwrap_or(s.len());
            if end == s.len() && text::byte(&s, end.saturating_sub(1))? != Some(b'\n') {
                let joined = literal_snapshot(self.document.line_ending.bytes())?.concat(&s.slice(start..end)?)?;
                let len = joined.len();
                edits.push(Edit { range: end..end, insert: Insert::Pieces(joined, 0..len) });
            } else {
                edits.push(Edit { range: end..end, insert: Insert::Pieces(s.clone(), start..end) });
            }
        }
        self.apply(edits, None, false)
    }

    pub fn move_lines(&mut self, down: bool) -> io::Result<Damage> {
        let s = self.document.snapshot();
        let ranges = self.selected_line_ranges()?;
        let mut edits = Vec::new();
        let mut cursor_moves: Vec<(Range<u64>, i128)> = Vec::new();
        for r in ranges {
            if !down && r.start == 0 || down && r.end >= s.line_count() {
                continue;
            }
            let a = if down { r.start } else { r.start - 1 };
            let middle = if down { r.end } else { r.start };
            let b = if down { r.end + 1 } else { r.end };
            let start = text::line_full(&s, a)?.start;
            let mid = text::line_full(&s, middle)?.start;
            let end = s.line_start(b).map_err(io::Error::other)?.unwrap_or(s.len());
            // Swap content ranges and their separator, including an unterminated EOF.
            let left_last = text::line(&s, middle - 1)?;
            let right_last = text::line(&s, b - 1)?;
            let separator = left_last.end..mid;
            let source = s
                .slice(mid..right_last.end)?
                .concat(&s.slice(separator.clone())?)?
                .concat(&s.slice(start..left_last.end)?)?
                .concat(&s.slice(right_last.end..end)?)?;
            let len = source.len();
            edits.push(Edit { range: start..end, insert: Insert::Pieces(source, 0..len) });
            let selected = if down { start..mid } else { mid..end };
            let delta = if down {
                (right_last.end - mid + separator.end - separator.start) as i128
            } else {
                -(mid as i128 - start as i128)
            };
            cursor_moves.push((selected, delta));
        }
        let before = self.cursors.clone();
        let old_rows = self.cursor_rows()?;
        let changed = !edits.is_empty();
        let damage = self.apply(edits, None, false)?;
        if !changed {
            return Ok(damage);
        }
        self.cursors = before.clone();
        for (c, old) in self.cursors.iter_mut().zip(before) {
            // A selection ending at the following line start belongs to its
            // first selected byte. A caret at that boundary owns the next line.
            let owner = old.selection().start;
            for (range, delta) in &cursor_moves {
                if range.contains(&owner) || old.is_caret() && owner == range.end && owner == s.len() {
                    c.head = (old.head as i128 + delta).clamp(0, self.document.buffer.len() as i128) as u64;
                    c.anchor = (old.anchor as i128 + delta).clamp(0, self.document.buffer.len() as i128) as u64;
                    break;
                }
            }
        }
        self.normalize();
        if let Some(last) = self.undo.back_mut() {
            last.after.cursors = self.cursors.clone();
            last.after.primary = self.primary;
        }
        self.finish_damage(old_rows, Some(self.top..self.top + self.rows as u64))
    }

    fn line_edits(&mut self, kind: LineEdit) -> io::Result<Damage> {
        let s = self.document.snapshot();
        let ranges = self.selected_line_ranges()?;
        let token = self.comment_token();
        if matches!(kind, LineEdit::Toggle | LineEdit::Comment | LineEdit::Uncomment) && token.is_none() {
            return Err(io::Error::other("line comment token unavailable for this language"));
        }
        let token = token.unwrap_or_default();
        let mut all_commented = true;
        if matches!(kind, LineEdit::Toggle) {
            for range in &ranges {
                for row in range.clone() {
                    let bounds = text::line(&s, row)?;
                    let bytes =
                        text::read(&s, bounds.start..bounds.end.min(bounds.start + text::WINDOW), text::WINDOW)?;
                    let start = bytes.iter().position(|b| !matches!(b, b' ' | b'\t')).unwrap_or(bytes.len());
                    if start < bytes.len() && !bytes[start..].starts_with(token.as_bytes()) {
                        all_commented = false;
                    }
                }
            }
        }
        let settings = self.settings.editor.clone();
        // The retained seed owns only the selected-range iterator and comment
        // String beyond its inline captures. Iterator/String clones retain at
        // most the original capacities; snapshots keep their source budget.
        let seed_heap_bytes = metadata_capacity::<Range<u64>>(ranges.capacity())?
            .checked_add(token.capacity())
            .ok_or_else(metadata_overflow)?;
        let edits = ranges.into_iter().rev().flat_map(|r| r.rev()).map(move |row| {
            let bounds = text::line(&s, row)?;
            let bytes = text::read(&s, bounds.start..bounds.end.min(bounds.start + text::WINDOW), text::WINDOW)?;
            let white = bytes.iter().position(|b| !matches!(b, b' ' | b'\t')).unwrap_or(bytes.len());
            let at = bounds.start + white as u64;
            Ok(match kind {
                LineEdit::Indent => {
                    let unit = if settings.insert_spaces {
                        vec![b' '; usize::from(settings.tab_size.max(1))]
                    } else {
                        vec![b'\t']
                    };
                    Some(Edit::bytes(bounds.start..bounds.start, &unit))
                }
                LineEdit::Outdent => {
                    let count = if bytes.first() == Some(&b'\t') {
                        1
                    } else {
                        white.min(usize::from(settings.tab_size.max(1)))
                    };
                    (count > 0).then(|| Edit::delete(bounds.start..bounds.start + count as u64))
                }
                _ if white == bytes.len() => None,
                _ if matches!(kind, LineEdit::Uncomment) || matches!(kind, LineEdit::Toggle) && all_commented => {
                    if bytes[white..].starts_with(token.as_bytes()) {
                        let mut end = at + token.len() as u64;
                        if text::byte(&s, end)? == Some(b' ') {
                            end += 1;
                        }
                        Some(Edit::delete(at..end))
                    } else {
                        None
                    }
                }
                _ => (!bytes[white..].starts_with(token.as_bytes()))
                    .then(|| Edit::bytes(at..at, format!("{token} ").as_bytes())),
            })
        });
        self.apply_descending_with_recipe(edits, None, Some(seed_heap_bytes))
    }
    pub fn indent(&mut self, outdent: bool) -> io::Result<Damage> {
        self.line_edits(if outdent { LineEdit::Outdent } else { LineEdit::Indent })
    }
    pub fn toggle_comment(&mut self) -> io::Result<Damage> {
        self.line_edits(LineEdit::Toggle)
    }

    /// Worker operation for whole-document trim. Markdown's two-space hard
    /// breaks are always preserved, for explicit trim and trim-on-save alike.
    pub fn trim_trailing_whitespace(&mut self) -> io::Result<Damage> {
        if self.is_markdown() {
            return Ok(Damage::default());
        }
        let s = self.document.snapshot();
        let edits = (0..s.line_count()).rev().map(move |row| {
            let bounds = text::line(&s, row)?;
            let mut end = bounds.end;
            while end > bounds.start {
                let start = end.saturating_sub(text::WINDOW).max(bounds.start);
                let bytes = text::read(&s, start..end, text::WINDOW)?;
                let count = bytes.iter().rev().take_while(|&&b| b == b' ' || b == b'\t').count();
                end -= count as u64;
                if count < bytes.len() {
                    break;
                }
            }
            Ok((end < bounds.end).then(|| Edit::delete(end..bounds.end)))
        });
        // This seed captures only inline range counters and a Snapshot handle.
        self.apply_descending_with_recipe(edits, None, Some(0))
    }

    fn comment_token(&self) -> Option<String> {
        if let Some(token) = &self.comment_override {
            return Some(token.clone());
        }
        let extension = self.document.path().and_then(|p| p.extension()).and_then(|e| e.to_str()).unwrap_or("");
        let language = self.language.as_deref().unwrap_or(extension);
        Some(
            match language {
                "rs" | "rust" | "c" | "h" | "cpp" | "cc" | "cs" | "java" | "js" | "ts" | "tsx" | "jsx" | "go"
                | "swift" | "kt" => "//",
                "py" | "python" | "rb" | "ruby" | "sh" | "bash" | "toml" | "yaml" | "yml" | "ps1" => "#",
                "sql" | "lua" | "hs" => "--",
                "ini" | "lisp" => ";",
                _ => return None,
            }
            .into(),
        )
    }

    pub fn copy(&self, clipboard: &mut dyn Clipboard) -> io::Result<()> {
        clipboard.set(self.selection_data()?)
    }
    fn selection_data(&self) -> io::Result<ClipboardData> {
        let s = self.document.snapshot();
        let linewise = self.cursors.iter().all(Cursor::is_caret);
        let ranges = if linewise {
            self.selected_line_ranges()?
                .into_iter()
                .map(|r| {
                    let start = text::line_full(&s, r.start)?.start;
                    Ok(start..s.line_start(r.end).map_err(io::Error::other)?.unwrap_or(s.len()))
                })
                .collect::<io::Result<Vec<_>>>()?
        } else {
            self.cursors.iter().map(Cursor::selection).collect()
        };
        Ok(ClipboardData { snapshot: s, ranges, linewise })
    }
    pub fn cut(&mut self, clipboard: &mut dyn Clipboard) -> io::Result<Damage> {
        let data = self.selection_data()?;
        clipboard.set(data.clone())?;
        if data.linewise {
            self.delete_lines(1)
        } else {
            self.apply(data.ranges.into_iter().map(Edit::delete).collect(), None, true)
        }
    }
    pub fn paste(&mut self, clipboard: &mut dyn Clipboard) -> io::Result<Damage> {
        let Some(data) = clipboard.get()? else { return Ok(Damage::default()) };
        self.paste_data(&data, None, 1)
    }
    fn paste_data(&mut self, data: &ClipboardData, after: Option<bool>, count: u32) -> io::Result<Damage> {
        let s = self.document.snapshot();
        let joined = data.joined(self.document.line_ending.bytes())?;
        let mut source = Snapshot::default();
        let ending = literal_snapshot(self.document.line_ending.bytes())?;
        for _ in 0..count.max(1) {
            source = source.concat(&joined)?;
            if data.linewise && text::byte(&joined, joined.len().saturating_sub(1))? != Some(b'\n') {
                source = source.concat(&ending)?;
            }
        }
        let edits = self
            .cursors
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let range = if data.linewise {
                    let r = text::line_full(&s, text::row(&s, c.head)?)?;
                    let at = if after == Some(true) { r.end } else { r.start };
                    at..at
                } else if let Some(after) = after {
                    let at = if data.linewise {
                        let r = text::line_full(&s, text::row(&s, c.head)?)?;
                        if after { r.end } else { r.start }
                    } else if after {
                        text::next(&s, c.head)?
                    } else {
                        c.head
                    };
                    at..at
                } else {
                    c.selection()
                };
                let insert = if data.linewise
                    && after == Some(true)
                    && range.start == s.len()
                    && !s.is_empty()
                    && text::byte(&s, s.len() - 1)? != Some(b'\n')
                {
                    let snapshot = ending.concat(&source)?;
                    let len = snapshot.len();
                    Insert::Pieces(snapshot, 0..len)
                } else if count == 1 && data.ranges.len() == self.cursors.len() && !data.linewise {
                    Insert::Pieces(data.snapshot.clone(), data.ranges[i].clone())
                } else {
                    Insert::Pieces(source.clone(), 0..source.len())
                };
                Ok(Edit { range, insert })
            })
            .collect::<io::Result<Vec<_>>>()?;
        self.apply(edits, None, true)
    }

    pub fn match_bracket(&mut self, budget: u64, extend: bool) -> io::Result<CommandResult> {
        let s = self.document.snapshot();
        let head = self.primary_cursor().head;
        let mut at = head;
        let pairs = [(b'(', b')'), (b'[', b']'), (b'{', b'}')];
        let mut bracket = text::byte(&s, at)?;
        if !pairs.iter().any(|(a, b)| bracket == Some(*a) || bracket == Some(*b)) && at > 0 {
            at = text::previous(&s, at)?;
            bracket = text::byte(&s, at)?;
        }
        let Some((open, close)) = pairs.into_iter().find(|(a, b)| bracket == Some(*a) || bracket == Some(*b)) else {
            return Ok(CommandResult {
                damage: Damage::default(),
                status: CommandStatus::Message("no bracket at cursor".into()),
            });
        };
        let forward = bracket == Some(open);
        let mut depth = 1;
        let start = at;
        let bounds = if forward {
            start.saturating_add(1)..start.saturating_add(budget).saturating_add(1).min(s.len())
        } else {
            start.saturating_sub(budget)..start
        };
        let mut edge = if forward { bounds.start } else { bounds.end };
        while if forward { edge < bounds.end } else { edge > bounds.start } {
            let window = if forward {
                edge..(edge + text::WINDOW).min(bounds.end)
            } else {
                edge.saturating_sub(text::WINDOW).max(bounds.start)..edge
            };
            let bytes = text::read(&s, window.clone(), text::WINDOW)?;
            for step in 0..bytes.len() {
                let i = if forward { step } else { bytes.len() - 1 - step };
                let b = bytes[i];
                if b == if forward { open } else { close } {
                    depth += 1;
                }
                if b == if forward { close } else { open } {
                    depth -= 1;
                }
                if depth == 0 {
                    at = window.start + i as u64;
                    let anchor = if extend { self.primary_cursor().anchor } else { at };
                    let damage = self.set_cursors(vec![Cursor { anchor, head: at, desired_column: None }], 0)?;
                    return Ok(CommandResult { damage, status: CommandStatus::Applied });
                }
            }
            edge = if forward { window.end } else { window.start };
        }
        Ok(CommandResult {
            damage: Damage { status: true, ..Damage::default() },
            status: CommandStatus::Message("matching bracket not found within scan budget".into()),
        })
    }
}

#[derive(Clone, Copy)]
enum LineEdit {
    Indent,
    Outdent,
    Toggle,
    Comment,
    Uncomment,
}

fn literal_snapshot(bytes: &[u8]) -> io::Result<Snapshot> {
    let mut buffer = Buffer::empty();
    buffer.insert(0, bytes)?;
    Ok(buffer.snapshot())
}

fn transform(at: u64, edits: &[Edit], end_bias: bool) -> u64 {
    let mut shift = 0_i128;
    for edit in edits {
        if at < edit.range.start {
            break;
        }
        if at <= edit.range.end {
            return (edit.range.start as i128
                + shift
                + if end_bias || at > edit.range.start { edit.insert.len() as i128 } else { 0 })
                as u64;
        }
        shift += edit.insert.len() as i128 - (edit.range.end - edit.range.start) as i128;
    }
    (at as i128 + shift) as u64
}

fn motion_target(
    s: &Snapshot,
    at: u64,
    motion: Motion,
    count: u32,
    desired: Option<u64>,
    rows: usize,
    tab: u8,
) -> io::Result<u64> {
    let row = text::row(s, at)?;
    let count = u64::from(count);
    match motion {
        Motion::Up | Motion::Down | Motion::PageUp | Motion::PageDown => {
            let distance = count.saturating_mul(if matches!(motion, Motion::PageUp | Motion::PageDown) {
                rows.max(1) as u64
            } else {
                1
            });
            let row = if matches!(motion, Motion::Up | Motion::PageUp) {
                row.saturating_sub(distance)
            } else {
                row.saturating_add(distance).min(s.line_count() - 1)
            };
            text::at_column(s, row, desired.unwrap_or(0), tab)
        }
        Motion::DocumentStart => Ok(0),
        Motion::DocumentEnd => Ok(s.len()),
        Motion::LineStart => Ok(text::line(s, row)?.start),
        Motion::LineEnd => Ok(text::line(s, row)?.end),
        Motion::SmartHome => {
            let bounds = text::line(s, row)?;
            let bytes = text::read(s, bounds.start..bounds.end.min(bounds.start + text::WINDOW), text::WINDOW)?;
            let nonwhite = bounds.start + bytes.iter().take_while(|&&b| b == b' ' || b == b'\t').count() as u64;
            Ok(if at == nonwhite { bounds.start } else { nonwhite })
        }
        _ => {
            let mut head = at;
            // Counts are bounded by distance, never by an unchecked u32 loop.
            for _ in 0..count.min(text::WINDOW) {
                let next = match motion {
                    Motion::Left => text::previous(s, head)?,
                    Motion::Right => text::next(s, head)?,
                    Motion::WordForward => text::word_forward(s, head)?,
                    Motion::WordBackward => text::word_backward(s, head)?,
                    Motion::WordEnd => {
                        let mut start = text::next(s, head)?;
                        while start < s.len()
                            && start.saturating_sub(head) < text::WINDOW
                            && text::character(s, start)?.0.is_whitespace()
                        {
                            start = text::next(s, start)?;
                        }
                        let range = text::word_range(s, start, false)?;
                        text::previous(s, range.end)?
                    }
                    _ => unreachable!(),
                };
                if head == next {
                    break;
                }
                head = next;
            }
            Ok(head)
        }
    }
}
