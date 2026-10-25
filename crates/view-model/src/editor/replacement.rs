//! Worker-only adapter for immutable search recipes. The real editor (including
//! its journal observer and undo history) is loaned, never cloned or reseeded.
use super::*;
use kwil_core_search::worker::{REPLACE_BYTES, REPLACE_EDITS, ReplaceRecipe, ReplacementEdits};

impl Editor {
    /// Metadata-only admission; do not retry a pending observer on transfer.
    pub(crate) fn admit_worker_loan(&self, revision: u64) -> io::Result<()> {
        if self.document.revision() != revision {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Search revision is stale; search again"));
        }
        if self.saving
            || self.compound.is_some()
            || self.has_pending_edit_receipt()
            || self.has_pending_save_completion()
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Finish pending edit/save/recovery work before replacing",
            ));
        }
        Ok(())
    }

    /// Runs exclusively on the fixed document worker. Validation is streaming;
    /// history retains compact inline shapes or the immutable spool iterator.
    pub(crate) fn apply_search_replacement(&mut self, recipe: ReplaceRecipe) -> io::Result<Damage> {
        self.admit_worker_loan(recipe.key.revision)?;
        // Inline plans have a fixed producer bound. Preserve only compact
        // shapes in undo/receipts; retaining their byte iterator would pin up
        // to8MiB outside the add budget under Recipe::Bulk's metadata charge.
        if let ReplacementEdits::Inline(edits) = &recipe.edits
            && (edits.len() > REPLACE_EDITS
                || edits
                    .iter()
                    .try_fold(0_usize, |bytes, edit| bytes.checked_add(edit.new_bytes.len()))
                    .is_none_or(|bytes| bytes > REPLACE_BYTES))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Inline replacement exceeds its bounded search plan",
            ));
        }
        let length = self.document.snapshot().len();
        let mut previous: Option<(u64, u64)> = None;
        let mut effective = false;
        for edit in recipe.edits.iter() {
            let edit = edit?;
            let end = edit.offset.checked_add(edit.len).filter(|end| *end <= length).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "Replacement range exceeds searched snapshot")
            })?;
            if edit.len == 0 && edit.new_bytes.is_empty() {
                continue;
            }
            if previous.is_some_and(|(start, end)| edit.offset < end || edit.offset == start) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Replacement ranges overlap or are out of order",
                ));
            }
            previous = Some((edit.offset, end));
            effective = true;
        }
        if !effective {
            return Ok(Damage::default());
        }
        let prepared = if let ReplacementEdits::Inline(edits) = &recipe.edits {
            Some(Recipe::Explicit(Arc::new(
                edits
                    .iter()
                    .filter(|edit| edit.len != 0 || !edit.new_bytes.is_empty())
                    .map(|edit| receipts::Shape {
                        range: edit.offset..edit.offset + edit.len, // validated above
                        inserted: edit.new_bytes.len() as u64,
                    })
                    .collect(),
            )))
        } else {
            None
        };
        self.apply_descending_with_recipe(
            recipe.edits.iter().rev().map(|edit| {
                let edit = edit?;
                let end = edit
                    .offset
                    .checked_add(edit.len)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Replacement range overflow"))?;
                Ok(Some(Edit { range: edit.offset..end, insert: Insert::Bytes(Arc::from(edit.new_bytes)) }))
            }),
            prepared,
            // Inline edits always use the explicit recipe above. The only
            // retained bulk seed is a SpoolEdits cursor: inline counters plus
            // a shared spool pin, covered by the existing source/spool budget.
            Some(0),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kwil_core_search::worker::SearchKey;

    fn recipe(editor: &Editor, edits: Vec<kwil_core_search::Edit>) -> ReplaceRecipe {
        ReplaceRecipe {
            key: SearchKey { document_id: 1, revision: editor.document().revision(), generation: 1 },
            edits: ReplacementEdits::Inline(Arc::new(edits)),
        }
    }

    fn stored_recipe(
        editor: &Editor,
        edits: Vec<kwil_core_search::Edit>,
        spool: bool,
        owner: &mut kwil_core_search::worker_spool::SpoolOwner,
    ) -> ReplaceRecipe {
        let mut result = recipe(editor, Vec::new());
        result.edits = if spool {
            let slot = owner.begin().unwrap();
            for edit in edits {
                owner.append(slot, edit).unwrap();
            }
            ReplacementEdits::Spool(owner.finish(slot).unwrap())
        } else {
            ReplacementEdits::Inline(Arc::new(edits))
        };
        result
    }

    #[test]
    fn replacement_noop_inline_and_spool_preserve_revision_sequence_and_redo() {
        for spool in [false, true] {
            let mut owner = kwil_core_search::worker_spool::SpoolOwner::new(8 << 20).unwrap();
            let mut editor = Editor::new(Document::new());
            editor.insert_text("base", Duration::ZERO).unwrap();
            editor.insert_text("!", Duration::from_secs(1)).unwrap();
            editor.undo().unwrap();
            let (sender, receiver) = std::sync::mpsc::sync_channel(3);
            editor
                .set_edit_observer(Some(Box::new(move |receipt| {
                    sender.try_send(receipt).map_err(|error| match error {
                        std::sync::mpsc::TrySendError::Full(receipt)
                        | std::sync::mpsc::TrySendError::Disconnected(receipt) => receipt,
                    })
                })))
                .unwrap();
            let revision = editor.document().revision();
            let next_revision = editor.document.next_revision;
            let sequence = editor.next_edit_sequence();
            assert_eq!(editor.document.buffer.history_depth(), (1, 1));
            for edits in [Vec::new(), vec![kwil_core_search::Edit { offset: 0, len: 0, new_bytes: Vec::new() }]] {
                let recipe = stored_recipe(&editor, edits, spool, &mut owner);
                let damage = editor.apply_search_replacement(recipe).unwrap();
                assert!(!damage.full && !damage.status && damage.rows.is_empty());
                assert_eq!(editor.document().revision(), revision);
                assert_eq!(editor.document.next_revision, next_revision);
                assert_eq!(editor.next_edit_sequence(), sequence);
                assert_eq!(editor.document.buffer.history_depth(), (1, 1));
                assert_eq!((editor.undo.len(), editor.redo.len()), (1, 1));
                assert!(receiver.try_recv().is_err(), "an identity recipe must not publish a receipt");
            }
            // The generic streamed entry also preserves both histories when
            // the producer supplied Some(empty-to-empty), rather than None.
            editor.apply_descending((0..1).map(|_| Ok(Some(Edit::bytes(0..0, b""))))).unwrap();
            assert_eq!(editor.document().revision(), revision);
            assert_eq!(editor.next_edit_sequence(), sequence);
            assert_eq!((editor.undo.len(), editor.redo.len()), (1, 1));
            assert_eq!(editor.document.buffer.history_depth(), (1, 1));
            editor.redo().unwrap();
            editor.undo().unwrap();
            editor.undo().unwrap();
            assert!(editor.document().snapshot().is_empty());
            assert_eq!(editor.document().revision(), 0, "undo must restore the real older insertion's model state");
            for expected in [crate::EditKind::Redo, crate::EditKind::Undo, crate::EditKind::Undo] {
                assert_eq!(receiver.recv_timeout(Duration::from_secs(1)).unwrap().kind, expected);
            }
            drop((editor, receiver));
            owner.retire().unwrap();
            assert_eq!(owner.live(), 0);
        }
    }

    #[test]
    fn replacement_mixed_noops_inline_and_spool_count_only_effective_shapes() {
        for spool in [false, true] {
            let mut owner = kwil_core_search::worker_spool::SpoolOwner::new(8 << 20).unwrap();
            let mut editor = Editor::new(Document::new());
            editor.insert_text("a\nb\n", Duration::ZERO).unwrap();
            let (sender, receiver) = std::sync::mpsc::sync_channel(3);
            editor
                .set_edit_observer(Some(Box::new(move |receipt| {
                    sender.try_send(receipt).map_err(|error| match error {
                        std::sync::mpsc::TrySendError::Full(receipt)
                        | std::sync::mpsc::TrySendError::Disconnected(receipt) => receipt,
                    })
                })))
                .unwrap();
            let recipe = stored_recipe(
                &editor,
                vec![
                    kwil_core_search::Edit { offset: 0, len: 0, new_bytes: Vec::new() },
                    kwil_core_search::Edit { offset: 2, len: 1, new_bytes: b"XX".to_vec() },
                    kwil_core_search::Edit { offset: 4, len: 0, new_bytes: Vec::new() },
                ],
                spool,
                &mut owner,
            );
            editor.apply_search_replacement(recipe).unwrap();
            assert_eq!(editor.document.buffer.history_depth(), (2, 0));
            editor.undo().unwrap();
            let mut bytes = Vec::new();
            editor.document().snapshot().write_to(&mut bytes).unwrap();
            assert_eq!(bytes, b"a\nb\n");
            editor.redo().unwrap();
            bytes.clear();
            editor.document().snapshot().write_to(&mut bytes).unwrap();
            assert_eq!(bytes, b"a\nXX\n");
            for _ in 0..3 {
                let receipt = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
                assert_eq!(receipt.replacement_count(), 1, "identity records cannot inflate batch counts");
                let mut actual = 0;
                receipt
                    .for_each_replacement(&mut |replacement| {
                        assert!(!replacement.range.is_empty() || !replacement.insert.is_empty());
                        actual += 1;
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(actual, 1);
            }
            drop((editor, receiver));
            owner.retire().unwrap();
            assert_eq!(owner.live(), 0);
        }
    }

    #[test]
    fn replacement_validation_rejects_overlaps_order_overflow_and_compound_without_changes() {
        let mut editor = Editor::new(Document::new());
        editor.insert_text("abc", Duration::ZERO).unwrap();
        let revision = editor.document().revision();
        let edit = |offset, len| kwil_core_search::Edit { offset, len, new_bytes: b"x".to_vec() };
        for edits in [
            vec![edit(0, 2), edit(1, 1)],
            vec![edit(2, 1), edit(0, 1)],
            vec![edit(u64::MAX, 1)],
            vec![edit(1, 0), edit(1, 0)],
        ] {
            let recipe = recipe(&editor, edits);
            assert_eq!(editor.apply_search_replacement(recipe).unwrap_err().kind(), io::ErrorKind::InvalidData);
            assert_eq!(editor.document().revision(), revision);
            let mut bytes = Vec::new();
            editor.document().snapshot().write_to(&mut bytes).unwrap();
            assert_eq!(bytes, b"abc");
        }
        editor.compound = Some(CompoundEdit::default());
        assert_eq!(editor.admit_worker_loan(revision).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        editor.compound = None;
        editor.undo().unwrap();
        assert!(editor.document().snapshot().is_empty(), "rejected recipes never add undo history");
    }

    #[test]
    fn replacement_stream_read_error_rolls_back_and_keeps_prior_history() {
        let mut editor = Editor::new(Document::new());
        editor.insert_text("abcdef", Duration::ZERO).unwrap();
        let revision = editor.document().revision();
        // The high edit succeeds before the lower record fails. A read error
        // must cancel the open transaction, without an undo or journal receipt.
        let edits = (0..2).rev().map(|index| {
            if index == 0 {
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated recipe"))
            } else {
                Ok(Some(Edit::bytes(4..6, b"changed")))
            }
        });
        assert_eq!(editor.apply_descending(edits).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(editor.document().revision(), revision);
        let mut bytes = Vec::new();
        editor.document().snapshot().write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"abcdef");
        editor.undo().unwrap();
        assert!(editor.document().snapshot().is_empty());
    }

    #[test]
    fn replacement_inline_history_releases_payload_under_spill_only_undo_cap() {
        let mut document = Document::new();
        document.buffer = Buffer::with_add_memory_limit(0);
        let mut editor = Editor::new(document);
        editor.set_undo_memory_limit(512 << 10);
        editor.insert_text(&"a\n".repeat(16), Duration::ZERO).unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(3);
        editor
            .set_edit_observer(Some(Box::new(move |receipt| {
                sender.try_send(receipt).map_err(|error| match error {
                    std::sync::mpsc::TrySendError::Full(receipt)
                    | std::sync::mpsc::TrySendError::Disconnected(receipt) => receipt,
                })
            })))
            .unwrap();
        let edits = Arc::new(
            (0..16)
                .map(|index| kwil_core_search::Edit { offset: index * 2, len: 1, new_bytes: vec![b'x'; 64 << 10] })
                .collect(),
        );
        let payload = Arc::downgrade(&edits);
        let revision = editor.document().revision();
        editor
            .apply_search_replacement(ReplaceRecipe {
                key: SearchKey { document_id: 1, revision, generation: 1 },
                edits: ReplacementEdits::Inline(edits),
            })
            .unwrap();
        let receipt = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(payload.upgrade().is_none(), "undo and observer receipts must not retain the inline payload vector");
        assert_eq!(editor.document().buffer.add_memory_bytes(), 0);
        assert!(editor.document().buffer.undo_memory_bytes() <= 512 << 10);
        assert_eq!(receipt.replacement_count(), 16);
        let mut shapes = 0;
        receipt
            .for_each_replacement(&mut |replacement| {
                assert_eq!(replacement.range.end - replacement.range.start, 1);
                assert_eq!(replacement.insert.len(), 64 << 10);
                shapes += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(shapes, 16);
        assert_eq!(editor.document().snapshot().len(), 16 * ((64 << 10) + 1));
        editor.undo().unwrap();
        let mut bytes = Vec::new();
        editor.document().snapshot().write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"a\n".repeat(16));
        editor.redo().unwrap();
        assert_eq!(editor.document().snapshot().len(), 16 * ((64 << 10) + 1));
        assert!(payload.upgrade().is_none());
    }
}
