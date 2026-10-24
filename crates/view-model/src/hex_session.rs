//! IO-free hex host state plus worker entry points; no new thread or IO owner.
//!
//! ROOT hooks (deliberately outside this package's four-file ownership):
//! - Export `pub mod hex_session;` from view-model; `mod hex_ui;` in each UI.
//! - OpenAsHex/ToggleHex must first obtain a worker-confirmed RawBytes document.
//!   RawBytes bypasses decoding AND decompression, includes BOMs, saves without
//!   encoding conversion, whitespace trimming or compression, and journals that
//!   policy through recovery. Never infer RawBytes merely from UTF8 save policy.
//! - Dirty decoded/compressed sources return NeedsRawReopen and stay untouched.
//!   Clean sources may reopen the same native path through explicit raw mode.
//! - Per tab retain Session, not a Snapshot. Call bind after every document,
//!   revision, input-epoch, transport or size change. Clear it on close/reopen.
//! - Existing DocumentWorker owns Snapshot/Editor transfers and disposal. Admit
//!   PageRequest into one latest bounded slot, verify full Source against the
//!   worker's captured snapshot identity, run read_page on that worker,
//!   return owned <=64KiB Page in the existing wake. Never read from draw/input.
//! - Admit EditRequest into one bounded slot. Move the existing Editor to that
//!   worker; apply_edit borrows it there, preserving its undo/receipt observer.
//!   Return Editor + EditCompletion even on failure; attach Editor before calling
//!   complete_edit_with_damage, then bind current source. Use returned damage;
//!   on failure repaint body/status and bind the returned editor's actual Source.
//!   Retain rejected Editor/snapshot
//!   ownership for worker retry/shutdown. A busy UI must never fall through to
//!   ordinary text mutations. UI may hold one rejected small request for retry;
//!   rejected admission must use reject_edit (only before worker acceptance).
//! - Source.epoch is ROOT's monotonic document/transport/input incarnation. It
//!   must change when raw policy changes, on reopening and when identity changes
//!   without a revision change. Do not reuse it for another source incarnation.
//! - Pages are fenced by full Source, viewport and generation. Edits also fence
//!   operation and input epoch; consume completion exactly once. Source changes
//!   invalidate pending nibbles, page and operation. No snapshot dies on the UI.
//! - Save/SaveAs/close/tab/palette remain ROOT chrome commands; body edits, IME,
//!   paste, mouse and scroll go to hex_ui before the ordinary text editor.
//!   Capture focus/source at input origin; discard deferred IME/paste after any
//!   tab, mode, cursor or focus change. Both UIs consume unsupported body input.
//! - accept_page_with_damage fills only new/changed rows. Equal-length byte
//!   overwrite completion patches only the bounded cache after worker success.
//!   Damage belongs to the source body/status only. Suppress ordinary text
//!   caret/selection/highlights. Hex scrolling uses Damage.scroll + exposed
//!   rows; use the hex retained body's copy path, never ordinary text pixels.
use crate::hex::{BYTES_PER_ROW, HexRow, HexView, MAX_ROWS};
use crate::{Damage, Editor};
use kwil_core_text::{Snapshot, TextFile};
use std::{io, ops::Range};

pub const MAX_PAGE_BYTES: usize = MAX_ROWS * BYTES_PER_ROW as usize;
pub const MAX_EDIT_BYTES: usize = MAX_PAGE_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Representation {
    /// Explicit worker-confirmed physical file bytes, including BOM/transport.
    RawBytes,
    DecodedText,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Source {
    pub document: u64,
    pub revision: u64,
    pub epoch: u64,
    pub len: u64,
    pub representation: Representation,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenOutcome {
    Ready,
    NeedsRawReopen,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageRequest {
    pub source: Source,
    pub generation: u64,
    pub top: u64,
    pub rows: usize,
}
#[derive(Clone, Debug)]
pub struct Page {
    pub request: PageRequest,
    pub rows: Vec<HexRow>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mutation {
    Replace { range: Range<u64>, bytes: Vec<u8> },
    Undo,
    Redo,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditRequest {
    pub source: Source,
    pub operation: u64,
    pub input_epoch: u64,
    pub mutation: Mutation,
}
#[derive(Debug)]
pub struct EditCompletion {
    pub before: Source,
    pub after: Source,
    pub operation: u64,
    pub input_epoch: u64,
    pub result: Result<(), String>,
}
/// Retained by the existing worker per raw tab. Revision can go backwards on
/// undo; monotonic operation admission prevents replay even after revision ABA.
#[derive(Default)]
pub struct EditFence {
    identity: Option<(u64, u64)>,
    operation: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input<'a> {
    Digit(char),
    Paste(&'a str),
    Move { bytes: i64, extend: bool },
    Home { document: bool, extend: bool },
    End { document: bool, extend: bool },
    SelectAll,
    Click { row: usize, column: usize, extend: bool },
    Scroll(i64),
    ScrollTo(u64),
    Delete { backwards: bool },
    ToggleInsert,
    Undo,
    Redo,
    Cancel,
}
#[derive(Default, Debug)]
pub struct Outcome {
    pub damage: Damage,
    pub edit: Option<EditRequest>,
}
struct Pending {
    operation: u64,
    input_epoch: u64,
    cursor: u64,
    overwrite: Option<(Range<u64>, Vec<u8>)>,
}

/// Scalar metadata, one bounded page and one bounded outstanding overwrite.
/// No snapshot/file/worker handle, source-sized buffers or unbounded queues.
pub struct Session {
    source: Option<Source>,
    view: HexView,
    generation: u64,
    input_epoch: u64,
    operation: u64,
    page: Option<Page>,
    pending: Option<Pending>,
}
impl Session {
    pub fn new(rows: usize) -> Self {
        Self {
            source: None,
            view: HexView::new(rows),
            generation: 0,
            input_epoch: 0,
            operation: 0,
            page: None,
            pending: None,
        }
    }
    pub fn source(&self) -> Option<Source> {
        self.source
    }
    pub fn view(&self) -> &HexView {
        &self.view
    }
    pub fn input_epoch(&self) -> u64 {
        self.input_epoch
    }
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }
    pub fn is_open(&self) -> bool {
        self.source.is_some()
    }
    pub fn bind(&mut self, source: Source) -> OpenOutcome {
        if source.representation != Representation::RawBytes {
            self.close();
            return OpenOutcome::NeedsRawReopen;
        }
        if self.source == Some(source) {
            return OpenOutcome::Ready;
        }
        let same = self.source.is_some_and(|old| old.document == source.document && old.epoch == source.epoch);
        let cursor = if same { self.view.cursor() } else { 0 };
        self.source = Some(source);
        self.pending = None;
        self.view.set_cursor(cursor, source.len, false);
        if !same {
            self.view.set_top(0, source.len);
        }
        self.invalidate();
        OpenOutcome::Ready
    }
    pub fn close(&mut self) {
        self.source = None;
        self.pending = None;
        self.view.cancel_nibble();
        self.invalidate();
    }
    fn next(value: &mut u64) {
        *value = value.checked_add(1).expect("hex session identity exhausted");
    }
    fn invalidate(&mut self) {
        Self::next(&mut self.generation);
        Self::next(&mut self.input_epoch);
        self.page = None;
    }
    /// Focus loss/IME cancellation fence; does not discard an accepted edit.
    pub fn cancel_input(&mut self) {
        self.view.cancel_nibble();
        Self::next(&mut self.input_epoch);
    }
    pub fn set_rows(&mut self, rows: usize) -> Damage {
        if self.view.rows() == rows.min(MAX_ROWS) {
            return Damage::default();
        }
        self.view.set_rows(rows, self.source.map_or(0, |s| s.len));
        self.invalidate();
        body_damage()
    }
    pub fn page_request(&self) -> Option<PageRequest> {
        Some(PageRequest {
            source: self.source?,
            generation: self.generation,
            top: self.view.top(),
            rows: self.view.rows(),
        })
    }
    pub fn accept_page(&mut self, page: Page) -> bool {
        self.accept_page_with_damage(page).is_some()
    }
    pub fn accept_page_with_damage(&mut self, page: Page) -> Option<Damage> {
        if Some(page.request) != self.page_request() || !valid_page(&page) {
            return None;
        }
        let rows =
            (0..self.view.rows()).filter(|&index| self.row(self.source, index) != page.rows.get(index)).collect();
        self.page = Some(page);
        Some(Damage { rows, status: true, ..Default::default() })
    }
    /// A stale current identity always paints blank, never the last tab's bytes.
    pub fn rows(&self, current: Option<Source>) -> &[HexRow] {
        if current != self.source {
            return &[];
        }
        self.page.as_ref().filter(|p| Some(p.request) == self.page_request()).map_or(&[], |p| &p.rows)
    }
    /// Reuse already accepted immutable rows while a newer viewport page is
    /// pending. Only the exact same Source can reuse bytes; late result pages
    /// still require the latest generation. Missing exposed rows draw blank.
    pub fn row(&self, current: Option<Source>, index: usize) -> Option<&HexRow> {
        if current != self.source || index >= self.view.rows() {
            return None;
        }
        let page = self.page.as_ref().filter(|page| Some(page.request.source) == self.source)?;
        let wanted = self.view.top().checked_add(index as u64)?;
        let index = wanted.checked_sub(page.request.top)?;
        page.rows.get(usize::try_from(index).ok()?)
    }
    pub fn status(&self) -> String {
        let Some(source) = self.source else {
            return "Raw byte reopen required".into();
        };
        format!(
            "HEX {}  {:016X} / {:016X}{}{}",
            if self.view.insert_mode() { "INS" } else { "OVR" },
            self.view.cursor(),
            source.len,
            if self.busy() { "  Editing..." } else { "" },
            self.view.pending_high_nibble().map_or(String::new(), |n| format!("  {n:X}_"))
        )
    }
    /// Input must carry the captured origin identity/epoch, including IME/paste.
    pub fn input(&mut self, source: Source, epoch: u64, input: Input<'_>) -> io::Result<Outcome> {
        if self.source != Some(source) || self.input_epoch != epoch {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "stale hex input"));
        }
        if self.busy() {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "hex edit awaiting worker"));
        }
        let old = self.view.clone();
        let old_selection = old.selection();
        let mut mutation = None;
        let mut cursor = self.view.cursor();
        match input {
            Input::Digit(digit) => {
                if let Some(byte) = self.view.input_digit(digit)? {
                    let range = replacement_range(&self.view, source.len, 1);
                    let Some(after) = range.start.checked_add(1) else {
                        self.view = old;
                        return Err(invalid_edit());
                    };
                    cursor = after;
                    mutation = Some(Mutation::Replace { range, bytes: vec![byte] });
                }
            }
            Input::Paste(text) => {
                // Parse all before changing anything; malformed/odd/oversize
                // paste must never commit a prefix or complete a pending nibble.
                let bytes = parse_hex(text)?;
                if bytes.is_empty() {
                    return Ok(Outcome::default());
                }
                let range = replacement_range(&self.view, source.len, bytes.len() as u64);
                cursor = range.start.checked_add(bytes.len() as u64).ok_or_else(invalid_edit)?;
                self.view.cancel_nibble();
                mutation = Some(Mutation::Replace { range, bytes });
            }
            Input::Delete { backwards } => {
                if self.view.pending_high_nibble().is_some() {
                    self.view.cancel_nibble();
                } else {
                    let selected = self.view.selection();
                    let range = if !selected.is_empty() {
                        selected
                    } else if backwards {
                        cursor.saturating_sub(1)..cursor
                    } else {
                        cursor..cursor.saturating_add(1).min(source.len)
                    };
                    if !range.is_empty() {
                        cursor = range.start;
                        mutation = Some(Mutation::Replace { range, bytes: Vec::new() });
                    }
                }
            }
            Input::Move { bytes, extend } => self.view.move_by(bytes, source.len, extend),
            Input::Home { document, extend } => {
                self.view.set_cursor(if document { 0 } else { cursor / 16 * 16 }, source.len, extend);
            }
            Input::End { document, extend } => {
                let end = if document { source.len } else { (cursor / 16 * 16).saturating_add(15).min(source.len) };
                self.view.set_cursor(end, source.len, extend);
            }
            Input::SelectAll => {
                self.view.set_cursor(0, source.len, false);
                self.view.set_cursor(source.len, source.len, true);
            }
            Input::Click { row, column, extend } => {
                if row >= self.view.rows() {
                    return Ok(Outcome::default());
                }
                let Some(byte) = crate::hex::byte_at_column(column) else {
                    return Ok(Outcome::default());
                };
                let at = self
                    .view
                    .top()
                    .checked_add(row as u64)
                    .and_then(|row| row.checked_mul(16))
                    .and_then(|at| at.checked_add(byte as u64));
                if let Some(at) = at.filter(|&at| at <= source.len) {
                    self.view.set_cursor(at, source.len, extend);
                }
            }
            Input::Scroll(delta) => {
                let top = (i128::from(self.view.top()) + i128::from(delta)).clamp(0, i128::from(u64::MAX));
                self.view.set_top(top as u64, source.len);
            }
            Input::ScrollTo(top) => self.view.set_top(top, source.len),
            Input::ToggleInsert => self.view.toggle_insert(),
            Input::Undo => {
                self.view.cancel_nibble();
                mutation = Some(Mutation::Undo);
            }
            Input::Redo => {
                self.view.cancel_nibble();
                mutation = Some(Mutation::Redo);
            }
            Input::Cancel => self.view.cancel_nibble(),
        }
        if let Some(mutation) = &mutation
            && let Err(error) = validate_mutation(mutation, source.len)
        {
            self.view = old;
            return Err(error);
        }
        // Every accepted input fences delayed text; a high nibble itself lives
        // only until the next input from the freshly captured current epoch.
        Self::next(&mut self.input_epoch);
        let edit = if let Some(mutation) = mutation {
            Self::next(&mut self.operation);
            let request = EditRequest { source, operation: self.operation, input_epoch: self.input_epoch, mutation };
            let overwrite = match &request.mutation {
                Mutation::Replace { range, bytes } if range.end - range.start == bytes.len() as u64 => {
                    Some((range.clone(), bytes.clone()))
                }
                _ => None,
            };
            self.pending =
                Some(Pending { operation: request.operation, input_epoch: request.input_epoch, cursor, overwrite });
            Some(request)
        } else {
            None
        };
        let damage = if old.top() != self.view.top() {
            Self::next(&mut self.generation);
            let delta = i128::from(self.view.top()) - i128::from(old.top());
            if delta.unsigned_abs() >= self.view.rows().max(1) as u128 {
                body_damage()
            } else {
                let delta = delta as i64;
                let mut rows: Vec<_> = if delta > 0 {
                    (self.view.rows() - delta as usize..self.view.rows()).collect()
                } else {
                    (0..(-delta) as usize).collect()
                };
                for cursor in [old.cursor(), self.view.cursor()] {
                    if let Some(row) =
                        (cursor / 16).checked_sub(self.view.top()).filter(|&r| r < self.view.rows() as u64)
                    {
                        rows.push(row as usize);
                    }
                }
                if old_selection != self.view.selection()
                    && (!old_selection.is_empty() || !self.view.selection().is_empty())
                {
                    rows.extend(0..self.view.rows());
                }
                rows.sort_unstable();
                rows.dedup();
                Damage { scroll: delta, rows, status: true, ..Default::default() }
            }
        } else if old_selection != self.view.selection()
            && (!old_selection.is_empty() || !self.view.selection().is_empty())
        {
            body_damage()
        } else {
            let mut rows = [old.cursor() / 16, self.view.cursor() / 16]
                .into_iter()
                .filter_map(|row| row.checked_sub(self.view.top()))
                .filter(|&row| row < self.view.rows() as u64)
                .map(|row| row as usize)
                .collect::<Vec<_>>();
            rows.sort_unstable();
            rows.dedup();
            Damage { rows, status: true, ..Default::default() }
        };
        Ok(Outcome { damage, edit })
    }
    /// Only for rejected queue admission. Accepted requests must complete once.
    pub fn reject_edit(&mut self, request: &EditRequest) -> bool {
        let matches = self.source == Some(request.source)
            && self
                .pending
                .as_ref()
                .is_some_and(|p| p.operation == request.operation && p.input_epoch == request.input_epoch);
        if matches {
            self.pending = None;
            self.cancel_input();
        }
        matches
    }
    pub fn complete_edit(&mut self, completion: EditCompletion) -> io::Result<bool> {
        self.complete_edit_with_damage(completion).map(|damage| damage.is_some())
    }
    pub fn complete_edit_with_damage(&mut self, completion: EditCompletion) -> io::Result<Option<Damage>> {
        let Some(pending) = &self.pending else {
            return Ok(None);
        };
        if self.source != Some(completion.before)
            || pending.operation != completion.operation
            || pending.input_epoch != completion.input_epoch
        {
            return Ok(None);
        }
        let pending = self.pending.take().expect("matched pending hex edit");
        let old_cursor = self.view.cursor();
        let old_top = self.view.top();
        let mut cached = self.page.take();
        self.view.cancel_nibble();
        self.invalidate();
        if let Err(error) = completion.result {
            return Err(io::Error::other(error));
        }
        if completion.after.document != completion.before.document
            || completion.after.epoch != completion.before.epoch
            || completion.after.representation != Representation::RawBytes
        {
            self.close();
            return Err(io::Error::new(io::ErrorKind::InvalidData, "hex worker returned a different source"));
        }
        self.source = Some(completion.after);
        self.view.set_cursor(pending.cursor, completion.after.len, false);
        // A successful equal-length overwrite leaves every other byte exact.
        // Patch only the bounded display copy AFTER worker acknowledgement;
        // no optimistic document mutation, mapped reads or source ownership.
        if let Some((range, bytes)) = pending.overwrite
            && completion.after.len == completion.before.len
            && let Some(page) = &mut cached
            && page.request.source == completion.before
        {
            let mut rows = Vec::new();
            for row in &mut page.rows {
                let start = row.offset.max(range.start);
                let end = (row.offset + row.bytes.len() as u64).min(range.end);
                if start < end {
                    row.bytes[(start - row.offset) as usize..(end - row.offset) as usize]
                        .copy_from_slice(&bytes[(start - range.start) as usize..(end - range.start) as usize]);
                    if let Some(index) =
                        (row.offset / 16).checked_sub(self.view.top()).filter(|&r| r < self.view.rows() as u64)
                    {
                        rows.push(index as usize);
                    }
                }
            }
            page.request.source = completion.after;
            page.request.generation = self.generation;
            self.page = cached;
            if old_top == self.view.top() {
                for cursor in [old_cursor, self.view.cursor()] {
                    if let Some(index) =
                        (cursor / 16).checked_sub(self.view.top()).filter(|&r| r < self.view.rows() as u64)
                    {
                        rows.push(index as usize);
                    }
                }
                rows.sort_unstable();
                rows.dedup();
                return Ok(Some(Damage { rows, status: true, ..Default::default() }));
            }
        }
        Ok(Some(body_damage()))
    }
}

pub fn body_damage() -> Damage {
    Damage { full: true, status: true, ..Default::default() }
}
fn invalid_edit() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "hex edit exceeds byte bounds")
}
fn replacement_range(view: &HexView, len: u64, bytes: u64) -> Range<u64> {
    let selected = view.selection();
    if !selected.is_empty() {
        return selected;
    }
    let start = view.cursor();
    start..if view.insert_mode() { start } else { start.saturating_add(bytes).min(len) }
}
fn validate_mutation(mutation: &Mutation, len: u64) -> io::Result<()> {
    if let Mutation::Replace { range, bytes } = mutation {
        if range.start > range.end
            || range.end > len
            || bytes.len() > MAX_EDIT_BYTES
            || len.checked_sub(range.end - range.start).and_then(|len| len.checked_add(bytes.len() as u64)).is_none()
        {
            return Err(invalid_edit());
        }
    }
    Ok(())
}
/// ASCII hexadecimal pairs with optional ASCII whitespace; bounded before scan.
pub fn parse_hex(text: &str) -> io::Result<Vec<u8>> {
    if text.len() > MAX_EDIT_BYTES * 3 {
        return Err(invalid_edit());
    }
    let mut bytes = Vec::new();
    let mut high = None;
    for byte in text.bytes().filter(|byte| !byte.is_ascii_whitespace()) {
        let value = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return Err(invalid_edit()),
        };
        if let Some(first) = high.take() {
            if bytes.len() == MAX_EDIT_BYTES {
                return Err(invalid_edit());
            }
            bytes.push(first << 4 | value);
        } else {
            high = Some(value);
        }
    }
    if high.is_some() {
        return Err(invalid_edit());
    }
    Ok(bytes)
}
fn valid_page(page: &Page) -> bool {
    let r = page.request;
    if r.source.representation != Representation::RawBytes
        || r.rows > MAX_ROWS
        || r.top > HexView::row_count(r.source.len).saturating_sub(r.rows.max(1) as u64)
    {
        return false;
    }
    let mut view = HexView::new(r.rows);
    view.set_top(r.top, r.source.len);
    let range = view.range(r.source.len);
    let byte_len = range.end - range.start;
    let expected = (byte_len / 16 + u64::from(!byte_len.is_multiple_of(16))) as usize
        + usize::from(r.rows > 0 && byte_len / 16 < r.rows as u64 && range.end.is_multiple_of(16));
    page.rows.len() == expected
        && page.rows.iter().enumerate().all(|(i, row)| {
            let offset = range.start + i as u64 * 16;
            row.offset == offset
                && r.source.len.checked_sub(offset).is_some_and(|n| row.bytes.len() == n.min(16) as usize)
        })
}

/// Worker-only, <=4096 rows/64KiB, no line scan, no retained snapshot in result.
pub fn read_page(snapshot: &Snapshot, request: PageRequest, mut cancelled: impl FnMut() -> bool) -> io::Result<Page> {
    if request.source.representation != Representation::RawBytes
        || request.source.len != snapshot.len()
        || request.rows > MAX_ROWS
    {
        return Err(invalid_edit());
    }
    let mut view = HexView::new(request.rows);
    view.set_top(request.top, snapshot.len());
    if view.top() != request.top {
        return Err(invalid_edit());
    }
    let range = view.range(snapshot.len());
    let mut bytes = Vec::with_capacity((range.end - range.start) as usize);
    for chunk in snapshot.chunks(range.clone())? {
        if cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "hex page cancelled"));
        }
        bytes.extend_from_slice(chunk.map_err(io::Error::other)?);
    }
    if cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "hex page cancelled"));
    }
    Ok(Page { request, rows: view.layout(range, &bytes) })
}
/// Worker-only. ROOT must supply worker-confirmed physical RawBytes ownership;
/// encoding=Utf8 alone is insufficient proof of that identity. No private undo.
pub fn apply_edit(
    editor: &mut Editor,
    current: Source,
    request: &EditRequest,
    fence: &mut EditFence,
) -> EditCompletion {
    let result = (|| -> io::Result<()> {
        if current != request.source
            || current.representation != Representation::RawBytes
            || editor.document().revision() != current.revision
            || editor.document().snapshot().len() != current.len
        {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "stale hex mutation"));
        }
        validate_mutation(&request.mutation, current.len)?;
        let identity = (current.document, current.epoch);
        if fence.identity == Some(identity) && request.operation <= fence.operation {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "replayed hex mutation"));
        }
        // Consume admission even if the editor refuses (for example receipt
        // backpressure). The host receives that failure and creates a new op.
        fence.identity = Some(identity);
        fence.operation = request.operation;
        match &request.mutation {
            Mutation::Replace { range, bytes } => {
                editor.replace_byte_range(range.clone(), bytes)?;
            }
            Mutation::Undo => {
                editor.undo()?;
            }
            Mutation::Redo => {
                editor.redo()?;
            }
        }
        Ok(())
    })();
    let after = Source { revision: editor.document().revision(), len: editor.document().snapshot().len(), ..current };
    EditCompletion {
        before: request.source,
        after,
        operation: request.operation,
        input_epoch: request.input_epoch,
        result: result.map_err(|e| e.to_string()),
    }
}

/// Worker-only pre-index page. TextFile stays with the worker and is never
/// retained in a page/result. Only this bounded slice is touched/copied.
pub fn read_file_page(file: &TextFile, request: PageRequest, mut cancelled: impl FnMut() -> bool) -> io::Result<Page> {
    if request.source.representation != Representation::RawBytes
        || request.source.len != file.len()
        || request.rows > MAX_ROWS
    {
        return Err(invalid_edit());
    }
    let mut view = HexView::new(request.rows);
    view.set_top(request.top, file.len());
    if view.top() != request.top {
        return Err(invalid_edit());
    }
    if cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "hex page cancelled"));
    }
    let range = view.range(file.len());
    let bytes = file.bytes().map_err(io::Error::other)?;
    let page = Page { request, rows: view.layout(range.clone(), &bytes[range.start as usize..range.end as usize]) };
    if cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "hex page cancelled"));
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Document;

    fn source(editor: &Editor) -> Source {
        Source {
            document: 7,
            revision: editor.document().revision(),
            epoch: 3,
            len: editor.document().snapshot().len(),
            representation: Representation::RawBytes,
        }
    }
    fn fixture(bytes: &[u8]) -> (Editor, Session, EditFence) {
        let mut editor = Editor::new(Document::new());
        if !bytes.is_empty() {
            editor.replace_byte_range(0..0, bytes).unwrap();
        }
        let mut session = Session::new(3);
        session.bind(source(&editor));
        (editor, session, EditFence::default())
    }
    fn input(session: &mut Session, input: Input<'_>) -> Outcome {
        session.input(session.source().unwrap(), session.input_epoch(), input).unwrap()
    }
    fn commit(editor: &mut Editor, session: &mut Session, fence: &mut EditFence, input_value: Input<'_>) {
        let request = input(session, input_value).edit.unwrap();
        let current = source(editor);
        let completion = apply_edit(editor, current, &request, fence);
        assert!(session.complete_edit(completion).unwrap());
        assert_eq!(session.source(), Some(source(editor)));
    }
    fn bytes(editor: &Editor) -> Vec<u8> {
        let mut bytes = Vec::new();
        editor.document().snapshot().write_to(&mut bytes).unwrap();
        bytes
    }
    #[test]
    fn invalid_binary_overwrite_insert_delete_share_undo_and_receipts() {
        let original = [0xff, 0, 0xfe, b'A', b'\r', b'\n'];
        let (mut editor, mut session, mut fence) = fixture(&original);
        let receipts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = receipts.clone();
        editor
            .set_edit_observer(Some(Box::new(move |receipt| {
                captured.lock().unwrap().push((receipt.kind, receipt.replacement_count()));
                Ok(())
            })))
            .unwrap();
        assert!(input(&mut session, Input::Digit('8')).edit.is_none());
        assert_eq!(bytes(&editor), original, "half-byte must not mutate the document");
        commit(&mut editor, &mut session, &mut fence, Input::Digit('0'));
        assert_eq!(bytes(&editor), [0x80, 0, 0xfe, b'A', b'\r', b'\n']);
        input(&mut session, Input::ToggleInsert);
        commit(&mut editor, &mut session, &mut fence, Input::Paste("FF 00"));
        assert_eq!(bytes(&editor), [0x80, 0xff, 0, 0, 0xfe, b'A', b'\r', b'\n']);
        commit(&mut editor, &mut session, &mut fence, Input::Undo);
        assert_eq!(bytes(&editor), [0x80, 0, 0xfe, b'A', b'\r', b'\n']);
        commit(&mut editor, &mut session, &mut fence, Input::Undo);
        assert_eq!(bytes(&editor), original);
        commit(&mut editor, &mut session, &mut fence, Input::Redo);
        assert_eq!(bytes(&editor), [0x80, 0, 0xfe, b'A', b'\r', b'\n']);
        input(&mut session, Input::SelectAll);
        commit(&mut editor, &mut session, &mut fence, Input::Delete { backwards: false });
        assert!(bytes(&editor).is_empty());
        commit(&mut editor, &mut session, &mut fence, Input::Undo);
        assert_eq!(bytes(&editor), [0x80, 0, 0xfe, b'A', b'\r', b'\n']);
        use crate::EditKind::{Edit, Redo, Undo};
        assert_eq!(
            *receipts.lock().unwrap(),
            vec![(Edit, 1), (Edit, 1), (Undo, 1), (Undo, 1), (Redo, 1), (Edit, 1), (Undo, 1)]
        );
    }
    #[test]
    fn old_page_cannot_reappear_after_scroll_resize_or_revision_aba() {
        let (editor, mut session, _) = fixture(&[b'x'; 96]);
        let old = read_page(&editor.document().snapshot(), session.page_request().unwrap(), || false).unwrap();
        assert!(session.accept_page(old.clone()));
        input(&mut session, Input::Scroll(1));
        assert!(session.rows(session.source()).is_empty());
        assert!(!session.accept_page(old.clone()));
        input(&mut session, Input::Scroll(-1));
        assert!(!session.accept_page(old.clone()), "same viewport still has a newer generation");
        session.set_rows(2);
        assert!(!session.accept_page(old));
    }
    #[test]
    fn malformed_pages_and_decoded_sources_never_display_as_binary() {
        let (editor, mut session, _) = fixture(b"abc");
        let page = read_page(&editor.document().snapshot(), session.page_request().unwrap(), || false).unwrap();
        let mut bad = page.clone();
        bad.rows[0].offset = 4;
        assert!(!session.accept_page(bad));
        let mut bad = page.clone();
        bad.rows[0].bytes.pop();
        assert!(!session.accept_page(bad));
        assert!(session.accept_page(page));
        let current = session.source().unwrap();
        assert!(session.rows(Some(Source { epoch: 9, ..current })).is_empty());
        assert_eq!(
            session.bind(Source { representation: Representation::DecodedText, ..current }),
            OpenOutcome::NeedsRawReopen
        );
        assert!(!session.is_open());
        assert!(session.page_request().is_none());
    }
    #[test]
    fn nibble_and_delayed_paste_are_fenced_by_cursor_focus_and_source() {
        let (_, mut session, _) = fixture(b"ab");
        let source = session.source().unwrap();
        let stale = session.input_epoch();
        input(&mut session, Input::Digit('f'));
        assert!(session.input(source, stale, Input::Paste("00")).is_err());
        input(&mut session, Input::Move { bytes: 1, extend: false });
        assert_eq!(session.view().pending_high_nibble(), None);
        assert!(input(&mut session, Input::Digit('a')).edit.is_none());
        session.cancel_input();
        assert_eq!(session.view().pending_high_nibble(), None);
        let epoch = session.input_epoch();
        session.bind(Source { document: 9, ..source });
        assert!(session.input(source, epoch, Input::Digit('0')).is_err());
        assert_eq!(session.view().cursor(), 0);
    }
    #[test]
    fn paste_validation_is_atomic_and_one_selection_is_one_undo() {
        let (mut editor, mut session, mut fence) = fixture(b"abcdef");
        input(&mut session, Input::Move { bytes: 1, extend: false });
        input(&mut session, Input::Move { bytes: 3, extend: true });
        let epoch = session.input_epoch();
        for bad in ["FF 0", "00 zz", "０１", "12\0"] {
            assert!(session.input(source(&editor), epoch, Input::Paste(bad)).is_err());
            assert_eq!(bytes(&editor), b"abcdef");
            assert_eq!(session.view().selection(), 1..4);
            assert_eq!(session.input_epoch(), epoch);
        }
        commit(&mut editor, &mut session, &mut fence, Input::Paste("ff\t00\nFE"));
        assert_eq!(bytes(&editor), [b'a', 0xff, 0, 0xfe, b'e', b'f']);
        commit(&mut editor, &mut session, &mut fence, Input::Undo);
        assert_eq!(bytes(&editor), b"abcdef");
        assert!(parse_hex(&"FF".repeat(MAX_EDIT_BYTES + 1)).is_err());
    }
    #[test]
    fn worker_rejects_stale_and_replayed_operations_even_after_undo() {
        let (mut editor, mut session, mut fence) = fixture(b"abc");
        let request = input(&mut session, Input::Paste("FF")).edit.unwrap();
        let before = source(&editor);
        let completion = apply_edit(&mut editor, before, &request, &mut fence);
        assert!(session.complete_edit(completion).unwrap());
        commit(&mut editor, &mut session, &mut fence, Input::Undo);
        assert_eq!(source(&editor), before, "undo deliberately creates revision ABA");
        let stale = apply_edit(&mut editor, before, &request, &mut fence);
        assert!(stale.result.is_err());
        assert_eq!(bytes(&editor), b"abc");
        let mut request = request;
        request.operation += 100;
        request.source.epoch += 1;
        assert!(apply_edit(&mut editor, before, &request, &mut fence).result.is_err());
        assert_eq!(bytes(&editor), b"abc");
    }
    #[test]
    fn busy_refusal_and_duplicate_completion_cannot_advance_cursor_twice() {
        let (mut editor, mut session, mut fence) = fixture(b"abc");
        let request = input(&mut session, Input::Paste("FF")).edit.unwrap();
        assert!(session.input(source(&editor), session.input_epoch(), Input::Digit('0')).is_err());
        assert!(session.reject_edit(&request));
        assert!(!session.reject_edit(&request));
        let request = input(&mut session, Input::Paste("FF")).edit.unwrap();
        let current = source(&editor);
        let completion = apply_edit(&mut editor, current, &request, &mut fence);
        assert!(session.complete_edit(completion).unwrap());
        let duplicate = EditCompletion {
            before: request.source,
            after: source(&editor),
            operation: request.operation,
            input_epoch: request.input_epoch,
            result: Ok(()),
        };
        assert!(!session.complete_edit(duplicate).unwrap());
        assert_eq!(session.view().cursor(), 1);
    }
    #[test]
    fn page_limits_empty_eof_cancel_and_huge_offsets_stay_bounded() {
        let (editor, mut session, _) = fixture(&[0; MAX_PAGE_BYTES + 19]);
        session.set_rows(usize::MAX);
        let request = session.page_request().unwrap();
        assert_eq!(request.rows, MAX_ROWS);
        let page = read_page(&editor.document().snapshot(), request, || false).unwrap();
        assert_eq!(page.rows.iter().map(|r| r.bytes.len()).sum::<usize>(), MAX_PAGE_BYTES);
        assert!(session.accept_page(page));
        assert!(read_page(&editor.document().snapshot(), request, || true).is_err());
        let (editor, mut session, _) = fixture(&[]);
        let page = read_page(&editor.document().snapshot(), session.page_request().unwrap(), || false).unwrap();
        assert_eq!(page.rows, vec![HexRow { offset: 0, bytes: vec![] }]);
        assert!(session.accept_page(page));
        session.bind(Source { len: u64::MAX, ..source(&editor) });
        input(&mut session, Input::End { document: true, extend: false });
        input(&mut session, Input::ToggleInsert);
        let epoch = session.input_epoch();
        assert!(session.input(session.source().unwrap(), epoch, Input::Paste("00")).is_err());
        assert_eq!(session.input_epoch(), epoch);
        assert_eq!(session.view().cursor(), u64::MAX);
    }
    #[test]
    fn both_hex_and_ascii_clicks_target_exact_bytes_and_eof() {
        let (_, mut session, _) = fixture(b"abcdefghijklmnopq");
        input(&mut session, Input::Click { row: 0, column: crate::hex::hex_column(8), extend: false });
        assert_eq!(session.view().cursor(), 8);
        input(&mut session, Input::Click { row: 1, column: crate::hex::ASCII_COLUMN, extend: true });
        assert_eq!(session.view().selection(), 8..16);
        input(&mut session, Input::Click { row: 1, column: crate::hex::hex_column(1), extend: false });
        assert_eq!(session.view().cursor(), 17);
    }
    #[test]
    fn growing_viewport_and_shrinking_document_keep_page_requests_valid() {
        let (mut editor, mut session, _) = fixture(&[b'x'; 160]);
        session.set_rows(1);
        input(&mut session, Input::End { document: true, extend: false });
        session.set_rows(10);
        assert!(read_page(&editor.document().snapshot(), session.page_request().unwrap(), || false).is_ok());
        editor.replace_byte_range(0..159, &[]).unwrap();
        session.bind(source(&editor));
        let page = read_page(&editor.document().snapshot(), session.page_request().unwrap(), || false).unwrap();
        assert!(session.accept_page(page));
        assert_eq!(session.view().top(), 0);
    }
    #[test]
    fn one_row_scroll_copies_and_only_new_rows_change_when_worker_page_arrives() {
        let (editor, mut session, _) = fixture(&[b'x'; 96]);
        let page = read_page(&editor.document().snapshot(), session.page_request().unwrap(), || false).unwrap();
        assert!(session.accept_page(page));
        let damage = input(&mut session, Input::Scroll(1)).damage;
        assert!(!damage.full);
        assert_eq!(damage.scroll, 1);
        assert_eq!(damage.rows, [2]);
        assert_eq!(session.row(session.source(), 0).unwrap().offset, 16);
        assert_eq!(session.row(session.source(), 1).unwrap().offset, 32);
        assert!(session.row(session.source(), 2).is_none());
        let page = read_page(&editor.document().snapshot(), session.page_request().unwrap(), || false).unwrap();
        let damage = session.accept_page_with_damage(page).unwrap();
        assert_eq!(damage.rows, [2]);
        assert!(!damage.full);
    }
    #[test]
    fn one_byte_acknowledged_overwrite_damages_one_row_and_keeps_other_rows() {
        let (mut editor, mut session, mut fence) = fixture(&[b'x'; 48]);
        let page = read_page(&editor.document().snapshot(), session.page_request().unwrap(), || false).unwrap();
        assert!(session.accept_page(page));
        let request = input(&mut session, Input::Paste("FF")).edit.unwrap();
        assert_eq!(session.row(session.source(), 0).unwrap().bytes[0], b'x', "no optimistic mutation");
        let current = source(&editor);
        let completion = apply_edit(&mut editor, current, &request, &mut fence);
        let damage = session.complete_edit_with_damage(completion).unwrap().unwrap();
        assert!(!damage.full);
        assert_eq!(damage.rows, [0]);
        assert_eq!(session.row(session.source(), 0).unwrap().bytes[0], 255);
        assert_eq!(session.row(session.source(), 1).unwrap().bytes, [b'x'; 16]);
        let page = read_page(&editor.document().snapshot(), session.page_request().unwrap(), || false).unwrap();
        assert!(session.accept_page_with_damage(page).unwrap().rows.is_empty());
    }
}
