//! A compound command admits once and commits one root/history/receipt.
use super::*;
use receipts::Shape;

#[derive(Default)]
pub(super) struct CompoundEdit {
    shapes: Vec<Shape>,
}
impl CompoundEdit {
    /// Fold intermediate-coordinate replacements into original-coordinate net
    /// spans. Payloads remain ranges in the final snapshot, never copied text.
    pub fn add(&mut self, edits: &[Edit]) -> io::Result<()> {
        for edit in edits.iter().rev() {
            let mut shift = 0_i128;
            let mut affected = None;
            let mut last = 0;
            let mut old_start = 0;
            let mut old_end = 0;
            let mut envelope_start = edit.range.start;
            let mut envelope_end = edit.range.end;
            let mut insert_at = self.shapes.len();
            let mut start_shift = 0;
            for (i, shape) in self.shapes.iter().enumerate() {
                let start = (shape.range.start as i128 + shift) as u64;
                let end = start + shape.inserted;
                let after_shift = shift + shape.inserted as i128 - (shape.range.end - shape.range.start) as i128;
                if end < edit.range.start {
                    shift = after_shift;
                    continue;
                }
                if start > edit.range.end {
                    insert_at = i;
                    break;
                }
                if affected.is_none() {
                    affected = Some(i);
                    start_shift = shift;
                    envelope_start = envelope_start.min(start);
                    old_start = shape.range.start;
                }
                last = i + 1;
                envelope_end = envelope_end.max(end);
                old_end = shape.range.end;
                shift = after_shift;
            }
            let range = if let Some(first) = affected {
                if edit.range.start == envelope_start {
                    old_start = old_start.min((edit.range.start as i128 - start_shift) as u64);
                }
                if edit.range.end == envelope_end {
                    old_end = old_end.max((edit.range.end as i128 - shift) as u64);
                }
                let inserted = envelope_end - envelope_start - (edit.range.end - edit.range.start) + edit.insert.len();
                self.shapes.splice(first..last, [Shape { range: old_start..old_end, inserted }]);
                continue;
            } else {
                (edit.range.start as i128 - shift) as u64..(edit.range.end as i128 - shift) as u64
            };
            if self.shapes.len() >= MAX_CURSORS {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "compound command exceeds 4096 disjoint replacement spans",
                ));
            }
            self.shapes.insert(insert_at, Shape { range, inserted: edit.insert.len() });
        }
        Ok(())
    }
}

impl Editor {
    pub(super) fn compound_edit(
        &mut self,
        edit: impl FnOnce(&mut Self) -> io::Result<CommandResult>,
    ) -> io::Result<CommandResult> {
        if self.compound.is_some() {
            return Err(io::Error::other("nested compound repeat"));
        }
        self.admit_edit()?;
        let before = self.state();
        let snapshot = self.document.snapshot();
        let top = self.top;
        let mode = self.mode;
        let registers = self.registers.clone();
        let repeat = self.repeat.clone();
        let prefix = self.insert_prefix.clone();
        let inserted = self.inserted.clone();
        let join_insert = self.join_insert;
        let burst = self.burst.clone();
        self.end_burst();
        self.document.buffer.begin();
        self.compound = Some(CompoundEdit::default());
        let result = edit(self);
        let compound = self.compound.take().unwrap();
        let result = result.and_then(|result| {
            let first = compound.shapes.first().map(|s| text::row(&snapshot, s.range.start)).transpose()?;
            let last = compound.shapes.last().map(|s| text::row(&snapshot, s.range.end)).transpose()?;
            Ok((result, first.zip(last)))
        });
        let (result, rows) = match result {
            Ok(result) => result,
            Err(error) => {
                self.document.buffer.cancel();
                self.restore(before);
                self.top = top;
                self.mode = mode;
                self.registers = registers;
                self.repeat = repeat;
                self.insert_prefix = prefix;
                self.inserted = inserted;
                self.join_insert = join_insert;
                self.burst = burst;
                return Err(error);
            }
        };
        self.document.buffer.commit();
        if let Some((first, last)) = rows {
            self.document.changed();
            let transition = Transition {
                before: snapshot.clone(),
                after: self.document.snapshot(),
                recipe: Recipe::Explicit(Arc::new(compound.shapes)),
            };
            self.redo.clear();
            self.undo.push_back(HistoryEntry {
                before: before.clone(),
                after: self.state(),
                edited: first..last + 1,
                structural: snapshot.line_count() != self.document.buffer.line_count(),
                transition: transition.clone(),
            });
            self.charge_model_history();
            self.publish(transition, before.revision, self.document.revision, EditKind::Edit);
        }
        self.end_burst();
        self.join_insert = false;
        Ok(result)
    }

    /// Compose damage expressed in successive viewport coordinates. Earlier
    /// dirty rows follow the incoming scroll; structural damage is retained.
    pub(super) fn merge_damage(&self, damage: &mut Damage, next: Damage) {
        damage.status |= next.status;
        let scroll = damage.scroll.saturating_add(next.scroll);
        if damage.full || next.full || scroll.unsigned_abs() >= self.rows.max(1) as u64 {
            damage.full = true;
            damage.scroll = 0;
            damage.rows.clear();
            return;
        }
        let mut rows = vec![false; self.rows];
        for row in &damage.rows {
            let at = *row as i128 - next.scroll as i128;
            if at >= 0
                && let Some(row) = rows.get_mut(at as usize)
            {
                *row = true;
            }
        }
        for at in next.rows {
            if let Some(row) = rows.get_mut(at) {
                *row = true;
            }
        }
        damage.scroll = scroll;
        damage.rows = rows.into_iter().enumerate().filter_map(|(i, changed)| changed.then_some(i)).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bytes(editor: &Editor) -> Vec<u8> {
        let mut bytes = Vec::new();
        editor.document.snapshot().write_to(&mut bytes).unwrap();
        bytes
    }
    #[test]
    fn composition_oracle_replays_mixed_intermediate_replacements() {
        let mut editor = Editor::new(Document::new());
        editor.insert_text("abcdefghijklmnopqrstuvwxyz", Duration::ZERO).unwrap();
        let original = bytes(&editor);
        let mut expected = original.clone();
        let mut seed = 17_u64;
        editor
            .compound_edit(|editor| {
                for step in 0..300 {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let start = seed as usize % (expected.len() + 1);
                    let end = (start + (seed >> 32) as usize % 5).min(expected.len());
                    let inserted = vec![b'A' + step as u8 % 26; step % 4];
                    expected.splice(start..end, inserted.clone());
                    editor.apply(vec![Edit::bytes(start as u64..end as u64, &inserted)], None, true)?;
                    let mut replay = original.clone();
                    let mut shift = expected.len() as i128 - original.len() as i128;
                    for shape in editor.compound.as_ref().unwrap().shapes.iter().rev() {
                        shift -= shape.inserted as i128 - (shape.range.end - shape.range.start) as i128;
                        let start = (shape.range.start as i128 + shift) as usize;
                        replay.splice(
                            shape.range.start as usize..shape.range.end as usize,
                            expected[start..start + shape.inserted as usize].iter().copied(),
                        );
                    }
                    assert_eq!(replay, expected, "composition at intermediate step {step}");
                }
                Ok(CommandResult { damage: Damage::default(), status: CommandStatus::Applied })
            })
            .unwrap();
        assert_eq!(bytes(&editor), expected);
        let transition = editor.undo.back().unwrap().transition.clone();
        let mut replay = original.clone();
        let Recipe::Explicit(shapes) = transition.recipe else {
            panic!("net recipe");
        };
        let mut shift = expected.len() as i128 - original.len() as i128;
        for shape in shapes.iter().rev() {
            shift -= shape.inserted as i128 - (shape.range.end - shape.range.start) as i128;
            let start = (shape.range.start as i128 + shift) as usize;
            replay.splice(
                shape.range.start as usize..shape.range.end as usize,
                expected[start..start + shape.inserted as usize].iter().copied(),
            );
        }
        assert_eq!(replay, expected);
        editor.undo().unwrap();
        assert_eq!(bytes(&editor), original);
        editor.redo().unwrap();
        assert_eq!(bytes(&editor), expected);
    }

    #[test]
    fn failed_compound_restores_document_modes_cursors_registers_and_history() {
        let mut editor = Editor::new(Document::new());
        editor.insert_text("original", Duration::ZERO).unwrap();
        let receipts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = receipts.clone();
        editor
            .set_edit_observer(Some(Box::new(move |receipt| {
                sink.lock().unwrap().push(receipt);
                Ok(())
            })))
            .unwrap();
        let before = editor.state();
        let depth = editor.document.buffer.history_depth();
        let revision = editor.document.next_revision;
        let error = editor
            .compound_edit(|editor| {
                editor.insert_text("changed", Duration::ZERO)?;
                editor.mode = Mode::Insert;
                editor.registers.insert('a', ClipboardData::text(b"lost")?);
                editor.repeat = Some(Repeat::Insert(b"lost".to_vec()));
                Err(io::Error::other("injected second operation failure"))
            })
            .unwrap_err();
        assert!(error.to_string().contains("injected"));
        assert_eq!(bytes(&editor), b"original");
        assert_eq!(editor.mode, Mode::Normal);
        assert_eq!(editor.cursors(), before.cursors);
        assert_eq!(editor.document.revision, before.revision);
        assert_eq!(editor.document.next_revision, revision);
        assert_eq!(editor.document.buffer.history_depth(), depth);
        assert!(editor.registers.is_empty());
        assert!(receipts.lock().unwrap().is_empty());
        assert!(!editor.has_pending_edit_receipt());
        let Some(Repeat::Insert(previous)) = editor.repeat else {
            panic!("repeat restored");
        };
        assert_eq!(previous, b"original");
    }

    #[test]
    fn disjoint_span_overflow_refuses_and_rolls_back_entire_command() {
        let mut editor = Editor::new(Document::new());
        let original = "ab".repeat(MAX_CURSORS + 1);
        editor.insert_text(&original, Duration::ZERO).unwrap();
        let before = editor.state();
        let depth = editor.document.buffer.history_depth();
        let receipts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = receipts.clone();
        editor
            .set_edit_observer(Some(Box::new(move |receipt| {
                sink.lock().unwrap().push(receipt);
                Ok(())
            })))
            .unwrap();
        let error = editor
            .compound_edit(|editor| {
                let edits = (0..MAX_CURSORS).map(|i| Edit::bytes(i as u64 * 2..i as u64 * 2, b"X")).collect();
                editor.apply(edits, None, false)?;
                assert_eq!(editor.compound.as_ref().unwrap().shapes.len(), MAX_CURSORS);
                editor.mode = Mode::Insert;
                editor.registers.insert('a', ClipboardData::text(b"lost")?);
                editor.repeat = Some(Repeat::Insert(b"lost".to_vec()));
                let end = editor.document.buffer.len();
                editor.apply(vec![Edit::bytes(end..end, b"overflow")], None, true)?;
                Ok(CommandResult { damage: Damage::default(), status: CommandStatus::Applied })
            })
            .unwrap_err();
        assert!(error.to_string().contains("4096 disjoint"));
        assert_eq!(bytes(&editor), original.as_bytes());
        assert_eq!(editor.cursors(), before.cursors);
        assert_eq!(editor.mode, Mode::Normal);
        assert!(editor.registers.is_empty());
        assert_eq!(editor.document.revision, before.revision);
        assert_eq!(editor.document.buffer.history_depth(), depth);
        assert!(receipts.lock().unwrap().is_empty());
        assert!(!editor.has_pending_edit_receipt());
        let Some(Repeat::Insert(previous)) = editor.repeat else {
            panic!("repeat restored");
        };
        assert_eq!(previous, original.as_bytes());
    }

    #[test]
    fn contiguous_repeat_accepts_4096_count_and_refuses_larger_without_truncation() {
        let mut editor = Editor::new(Document::new());
        editor.insert_text("x", Duration::ZERO).unwrap();
        let call = Invocation {
            id: "edit.repeat",
            motion: None,
            count: 4096,
            explicit_count: true,
            register: None,
            argument: kwil_core_commands::vim::Argument::None,
        };
        editor.execute_vim(&call).unwrap();
        assert_eq!(editor.document.buffer.len(), 4097);
        let Recipe::Explicit(shapes) = &editor.undo.back().unwrap().transition.recipe else {
            panic!("net recipe");
        };
        assert_eq!(shapes.len(), 1);
        assert_eq!(shapes[0].inserted, 4096);
        editor.undo().unwrap();
        assert_eq!(bytes(&editor), b"x");
        let mut large = call;
        large.count = 4097;
        assert_eq!(editor.execute_vim(&large).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(bytes(&editor), b"x");
    }
}
