use super::*;
use crate::{DocumentEncoding, DocumentLineEnding};
use kwil_core_text::encoding::Encoding;

impl Editor {
    pub(crate) fn reset_file_policy_observer(&mut self) {
        self.receipts.reset_for_raw_reopen();
    }
    pub(crate) fn select_save_encoding(&mut self, encoding: Encoding) {
        self.receipts.reset_for_raw_reopen();
        self.document.encoding =
            if encoding == Encoding::UTF8 { DocumentEncoding::Utf8 } else { DocumentEncoding::Encoded(encoding) };
        self.document.bom &= !encoding.bom_bytes().is_empty();
        self.document.mark_unsaved();
    }

    /// The existing bulk transaction retains a tiny iterator, not a per-line
    /// edit list. Only terminators are replaced; opaque source pieces survive.
    pub(crate) fn convert_line_endings(&mut self, ending: DocumentLineEnding) -> io::Result<Damage> {
        let source = self.document.snapshot();
        let last = source.line_count().saturating_sub(1);
        let edits = (0..last).map(move |row| {
            let end = source.line_start(row + 1).map_err(io::Error::other)?.unwrap();
            let crlf = end >= 2 && text::byte(&source, end - 2)? == Some(b'\r');
            if crlf == (ending == DocumentLineEnding::CrLf) {
                return Ok(None);
            }
            Ok(Some(Edit::bytes(end - if crlf { 2 } else { 1 }..end, ending.bytes())))
        });
        let damage = self.apply_descending_with_recipe(edits.rev(), None, Some(0))?;
        self.document.line_ending = ending;
        Ok(damage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_commands_convert_both_directions_and_undo_preserves_untouched_bytes() {
        let mut editor = Editor::new(Document::new());
        editor.replace_byte_range(0..0, b"a\n\xff\r\nb\n").unwrap();
        editor.convert_line_endings(DocumentLineEnding::CrLf).unwrap();
        let mut bytes = Vec::new();
        editor.document.snapshot().write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"a\r\n\xff\r\nb\r\n");
        editor.convert_line_endings(DocumentLineEnding::Lf).unwrap();
        bytes.clear();
        editor.document.snapshot().write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"a\n\xff\nb\n");
        editor.undo().unwrap();
        bytes.clear();
        editor.document.snapshot().write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"a\r\n\xff\r\nb\r\n");
    }
}
