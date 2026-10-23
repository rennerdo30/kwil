//! Byte-addressed layout and input. Only visible rows are read; editing reuses
//! the same document, atomic saves and undo transactions as the text view.
use crate::{Damage, Editor};
use kwil_core_text::{Snapshot, TextFile};
use std::{io, ops::Range};

pub const BYTES_PER_ROW: u64 = 16;
const MAX_ROWS: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HexRow {
    pub offset: u64,
    pub bytes: Vec<u8>,
}
impl HexRow {
    pub fn text(&self) -> String {
        let mut out = format!("{:016X}  ", self.offset);
        for index in 0..16 {
            if index == 8 {
                out.push(' ');
            }
            if let Some(byte) = self.bytes.get(index) {
                use std::fmt::Write;
                write!(out, "{byte:02X} ").expect("String write cannot fail");
            } else {
                out.push_str("   ");
            }
        }
        out.push_str(" | ");
        for byte in &self.bytes {
            out.push(if (0x20..=0x7e).contains(byte) { char::from(*byte) } else { '.' });
        }
        out
    }
}

#[derive(Clone, Debug, Default)]
pub struct HexView {
    top: u64,
    rows: usize,
    cursor: u64,
    anchor: u64,
    high: Option<u8>,
}
impl HexView {
    pub fn new(rows: usize) -> Self {
        Self { rows: rows.min(MAX_ROWS), ..Default::default() }
    }
    pub fn top(&self) -> u64 {
        self.top
    }
    pub fn cursor(&self) -> u64 {
        self.cursor
    }
    pub fn pending_high_nibble(&self) -> Option<u8> {
        self.high
    }
    pub fn selection(&self) -> Range<u64> {
        self.anchor.min(self.cursor)..self.anchor.max(self.cursor)
    }
    /// The last row includes the EOF insertion position, including empty files.
    pub fn row_count(len: u64) -> u64 {
        len / BYTES_PER_ROW + 1
    }
    pub fn set_rows(&mut self, rows: usize, len: u64) {
        self.rows = rows.min(MAX_ROWS);
        self.set_cursor(self.cursor, len, true);
    }
    pub fn set_top(&mut self, row: u64, len: u64) {
        self.top = row.min(Self::row_count(len).saturating_sub(self.rows.max(1) as u64));
    }
    pub fn set_cursor(&mut self, offset: u64, len: u64, extend: bool) {
        self.cursor = offset.min(len);
        if !extend {
            self.anchor = self.cursor;
        }
        self.anchor = self.anchor.min(len);
        self.high = None;
        let row = self.cursor / BYTES_PER_ROW;
        if row < self.top {
            self.top = row;
        }
        if self.rows > 0 && row.saturating_sub(self.top) >= self.rows as u64 {
            self.top = row + 1 - self.rows as u64;
        }
    }
    pub fn move_by(&mut self, bytes: i64, len: u64, extend: bool) {
        let next = (i128::from(self.cursor) + i128::from(bytes)).clamp(0, i128::from(len));
        self.set_cursor(next as u64, len, extend);
    }
    fn range(&self, len: u64) -> Range<u64> {
        let start = self.top.saturating_mul(BYTES_PER_ROW).min(len);
        start..start.saturating_add((self.rows.min(MAX_ROWS) as u64) * BYTES_PER_ROW).min(len)
    }
    fn layout(&self, range: Range<u64>, bytes: &[u8]) -> Vec<HexRow> {
        let mut rows: Vec<_> = bytes
            .chunks(16)
            .enumerate()
            .map(|(index, chunk)| HexRow { offset: range.start + index as u64 * BYTES_PER_ROW, bytes: chunk.to_vec() })
            .collect();
        if rows.len() < self.rows && range.end.is_multiple_of(BYTES_PER_ROW) {
            rows.push(HexRow { offset: range.end, bytes: Vec::new() });
        }
        rows
    }
    pub fn snapshot_rows(&self, snapshot: &Snapshot) -> io::Result<Vec<HexRow>> {
        let range = self.range(snapshot.len());
        let mut bytes = Vec::with_capacity((range.end - range.start) as usize);
        for chunk in snapshot.chunks(range.clone())? {
            bytes.extend_from_slice(chunk.map_err(io::Error::other)?);
        }
        Ok(self.layout(range, &bytes))
    }
    /// First-screen path: no line index or full-file copy is required.
    pub fn file_rows(&self, file: &TextFile) -> io::Result<Vec<HexRow>> {
        let bytes = file.bytes().map_err(io::Error::other)?;
        let range = self.range(bytes.len() as u64);
        Ok(self.layout(range.clone(), &bytes[range.start as usize..range.end as usize]))
    }
    pub fn type_hex(&mut self, editor: &mut Editor, digit: char) -> io::Result<Damage> {
        let value = digit
            .to_digit(16)
            .filter(|_| digit.is_ascii())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Enter a hexadecimal digit (0–9, A–F)"))?
            as u8;
        let Some(high) = self.high else {
            self.high = Some(value);
            return Ok(self.damage());
        };
        let old_len = editor.document().snapshot().len();
        let start = self.selection().start;
        let end = if self.selection().is_empty() {
            start.saturating_add(1).min(editor.document().snapshot().len())
        } else {
            self.selection().end
        };
        let old_top = self.top;
        editor.replace_byte_range(start..end, &[(high << 4) | value])?;
        self.set_cursor(start + 1, editor.document().snapshot().len(), false);
        Ok(self.changed(start, old_top, end - start != 1, old_len.max(editor.document().snapshot().len())))
    }
    pub fn delete(&mut self, editor: &mut Editor, backwards: bool) -> io::Result<Damage> {
        if self.high.take().is_some() {
            return Ok(self.damage());
        }
        let len = editor.document().snapshot().len();
        let selected = self.selection();
        let range = if !selected.is_empty() {
            selected
        } else if backwards {
            self.cursor.saturating_sub(1)..self.cursor
        } else {
            self.cursor..self.cursor.saturating_add(1).min(len)
        };
        let old_top = self.top;
        editor.replace_byte_range(range.clone(), &[])?;
        self.set_cursor(range.start, editor.document().snapshot().len(), false);
        Ok(self.changed(range.start, old_top, !range.is_empty(), len))
    }
    fn changed(&self, first: u64, old_top: u64, structural: bool, affected_len: u64) -> Damage {
        let delta = i128::from(self.top) - i128::from(old_top);
        if delta.unsigned_abs() >= self.rows.max(1) as u128 {
            return Damage { full: true, status: true, ..Default::default() };
        }
        let mut damage = self.damage();
        damage.scroll = delta as i64;
        let first_row = (first / BYTES_PER_ROW).saturating_sub(self.top) as usize;
        if structural {
            let end = Self::row_count(affected_len).saturating_sub(self.top).min(self.rows as u64) as usize;
            damage.rows.extend(first_row.min(end)..end);
        } else if first_row < self.rows {
            damage.rows.push(first_row);
        }
        if delta > 0 {
            damage.rows.extend(self.rows - delta as usize..self.rows);
        } else if delta < 0 {
            damage.rows.extend(0..(-delta) as usize);
        }
        damage.rows.sort_unstable();
        damage.rows.dedup();
        damage
    }
    fn damage(&self) -> Damage {
        let row = (self.cursor / BYTES_PER_ROW).saturating_sub(self.top) as usize;
        Damage { rows: if row < self.rows { vec![row] } else { Vec::new() }, status: true, ..Default::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommandArgs, Document};
    fn bytes(editor: &Editor) -> Vec<u8> {
        let mut out = Vec::new();
        editor.document().snapshot().write_to(&mut out).unwrap();
        out
    }
    #[test]
    fn hex_edits_preserve_invalid_bytes_and_share_text_undo() {
        let mut editor = Editor::new(Document::new());
        let mut view = HexView::new(3);
        view.type_hex(&mut editor, 'F').unwrap();
        assert!(bytes(&editor).is_empty());
        view.type_hex(&mut editor, 'F').unwrap();
        view.type_hex(&mut editor, '0').unwrap();
        view.type_hex(&mut editor, '0').unwrap();
        assert_eq!(bytes(&editor), [0xff, 0]);
        view.set_cursor(0, 2, false);
        view.type_hex(&mut editor, '4').unwrap();
        assert!(view.type_hex(&mut editor, 'x').is_err());
        view.type_hex(&mut editor, '1').unwrap();
        assert_eq!(bytes(&editor), [b'A', 0]);
        editor.undo().unwrap();
        assert_eq!(bytes(&editor), [0xff, 0]);
        editor.redo().unwrap();
        view.set_cursor(2, 2, false);
        view.delete(&mut editor, true).unwrap();
        assert_eq!(bytes(&editor), b"A");
        editor.undo().unwrap();
        assert_eq!(bytes(&editor), [b'A', 0]);
    }
    #[test]
    fn hex_rows_bound_reads_and_keep_exact_offsets_and_ascii() {
        let mut editor = Editor::new(Document::new());
        editor
            .execute(
                "edit.insert",
                &CommandArgs { text: Some("0123456789abcdefghijklmnopqrstuv".into()), ..Default::default() },
                1,
            )
            .unwrap();
        let mut view = HexView::new(1);
        view.set_top(1, 32);
        let rows = view.snapshot_rows(&editor.document().snapshot()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].offset, 16);
        assert_eq!(rows[0].bytes, b"ghijklmnopqrstuv");
        assert!(rows[0].text().starts_with("0000000000000010  67 68"));
        assert!(rows[0].text().ends_with(" | ghijklmnopqrstuv"));
        view.set_top(2, 32);
        assert!(view.snapshot_rows(&editor.document().snapshot()).unwrap()[0].bytes.is_empty());
        view.set_rows(0, 32);
        assert!(view.snapshot_rows(&editor.document().snapshot()).unwrap().is_empty());
    }
    #[test]
    fn hex_cursor_addresses_large_files_without_allocation() {
        let mut view = HexView::new(30);
        view.set_cursor(u64::MAX, u64::MAX, false);
        assert_eq!(view.cursor(), u64::MAX);
        assert_eq!(view.top(), u64::MAX / 16 + 1 - 30);
        view.move_by(i64::MIN, u64::MAX, true);
        assert_eq!(view.selection(), i64::MAX as u64..u64::MAX);
        view.set_cursor(0, 0, false);
        assert_eq!(view.top(), 0);
    }
}
