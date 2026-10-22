use super::*;

impl Editor {
    /// Execute stable command ids. UI-owned or unavailable commands return an
    /// explicit status; no command path panics on an unknown id.
    pub fn execute(&mut self, id: &str, args: &CommandArgs, count: u32) -> io::Result<CommandResult> {
        self.execute_with_clipboard(id, args, count, None)
    }
    pub fn execute_with_clipboard(
        &mut self,
        id: &str,
        args: &CommandArgs,
        count: u32,
        clipboard: Option<&mut dyn Clipboard>,
    ) -> io::Result<CommandResult> {
        let count = count.max(1);
        if let Some(motion) = command_motion(id) {
            return Ok(applied(self.move_cursors(
                motion,
                count,
                args.extend || matches!(self.mode, Mode::Visual | Mode::VisualLine),
            )?));
        }
        let damage = match id {
            "edit.insert" if args.text.is_some() => self.insert_text(args.text.as_deref().unwrap(), args.at)?,
            "edit.insert" | "mode.insert" => {
                self.start_insert(None);
                Damage::default()
            }
            "edit.newline" => self.newline()?,
            "edit.backspace" => self.delete(true, false)?,
            "edit.delete" | "edit.deleteCharacter" => {
                if count == 1 {
                    self.delete(false, false)?
                } else {
                    return self.execute_vim(&invocation("operator.delete", Some(VimMotion::Right), count));
                }
            }
            "edit.deleteWordBackward" => self.delete(true, true)?,
            "edit.deleteWordForward" => self.delete(false, true)?,
            "edit.undo" => self.undo()?,
            "edit.redo" => self.redo()?,
            "edit.selectAll" => self.select_all()?,
            "edit.selectWord" => self.select_word()?,
            "edit.selectLine" => self.select_line()?,
            "edit.selectNextMatch" => return self.add_next_occurrence(1 << 20),
            "edit.addCursorAbove" => self.add_cursor_vertical(false)?,
            "edit.addCursorBelow" => self.add_cursor_vertical(true)?,
            "edit.addCursor" => {
                let Some(cursor) = args.cursor.clone() else {
                    return Ok(message("addCursor requires a cursor"));
                };
                let mut cursors = self.cursors.clone();
                cursors.push(cursor);
                let p = cursors.len() - 1;
                self.set_cursors(cursors, p)?
            }
            "edit.columnSelection" => {
                let Some((a, b, c, d)) = args.column_selection else {
                    return Ok(message("columnSelection requires row/column endpoints"));
                };
                self.column_selection(a, b, c, d)?
            }
            "edit.deleteLine" => self.delete_lines(count)?,
            "edit.duplicateLine" => self.duplicate_lines()?,
            "edit.moveLineUp" => self.move_lines(false)?,
            "edit.moveLineDown" => self.move_lines(true)?,
            "edit.indent" => self.indent(false)?,
            "edit.outdent" => self.indent(true)?,
            "edit.toggleComment" => self.toggle_comment()?,
            "edit.comment" => self.line_edits(LineEdit::Comment)?,
            "edit.uncomment" => self.line_edits(LineEdit::Uncomment)?,
            "edit.trimTrailingWhitespace" => self.trim_trailing_whitespace()?,
            "edit.copy" | "edit.cut" | "edit.cutLine" | "edit.paste" => {
                let Some(clipboard) = clipboard else {
                    return Ok(message("frontend clipboard required"));
                };
                match id {
                    "edit.copy" => {
                        self.copy(clipboard)?;
                        Damage::default()
                    }
                    "edit.cut" => self.cut(clipboard)?,
                    "edit.cutLine" => {
                        self.select_line()?;
                        self.cut(clipboard)?
                    }
                    _ => self.paste(clipboard)?,
                }
            }
            "navigation.goToLine" => {
                let Some(line) = args.line else {
                    return Ok(message("goToLine requires a one-based line"));
                };
                self.go_to_line(line, args.column.unwrap_or(1), args.extend)?
            }
            "navigation.matchBracket" => return self.match_bracket(1 << 20, args.extend),
            "edit.repeat"
            | "edit.pasteAfter"
            | "edit.pasteBefore"
            | "edit.append"
            | "edit.openAbove"
            | "edit.openBelow"
            | "edit.insertLineStart"
            | "edit.appendLineEnd"
            | "mode.normal"
            | "mode.visual"
            | "mode.visualLine" => return self.execute_vim(&invocation(id_to_static(id), None, count)),
            "file.save" => {
                return Ok(CommandResult { damage: Damage::default(), status: CommandStatus::SaveRequested });
            }
            "view.toggleWordWrap" => {
                self.settings.editor.word_wrap = !self.settings.editor.word_wrap;
                Damage { full: true, status: true, ..Damage::default() }
            }
            "view.toggleWhitespace" => {
                self.settings.editor.show_whitespace = !self.settings.editor.show_whitespace;
                Damage { full: true, status: true, ..Damage::default() }
            }
            "view.zoomIn" | "view.zoomOut" | "view.zoomReset" => {
                self.settings.font.size = match id {
                    "view.zoomIn" => (self.settings.font.size + count as f32).min(128.0),
                    "view.zoomOut" => (self.settings.font.size - count as f32).max(1.0),
                    _ => 14.0,
                };
                Damage { full: true, status: true, ..Damage::default() }
            }
            _ => {
                return Ok(CommandResult {
                    damage: Damage { status: true, ..Damage::default() },
                    status: CommandStatus::NotImplemented(format!("command not implemented: {id}")),
                });
            }
        };
        Ok(applied(damage))
    }

    fn start_insert(&mut self, prefix: Option<Invocation>) {
        self.end_burst();
        self.mode = Mode::Insert;
        self.insert_prefix = prefix;
        self.inserted.clear();
    }
    fn finish_insert(&mut self) {
        if !self.inserted.is_empty() {
            self.repeat = Some(if let Some(prefix) = self.insert_prefix.take() {
                Repeat::Vim(prefix, std::mem::take(&mut self.inserted))
            } else {
                Repeat::Insert(std::mem::take(&mut self.inserted))
            });
        }
        self.insert_prefix = None;
        self.join_insert = false;
        self.end_burst();
    }

    pub fn execute_vim(&mut self, call: &Invocation) -> io::Result<CommandResult> {
        let count = call.count.max(1);
        if let Some(motion) = VimMotion::from_id(call.id) {
            if matches!(motion, VimMotion::FirstLine | VimMotion::LastLine) {
                let row = if call.explicit_count {
                    u64::from(count)
                } else if motion == VimMotion::FirstLine {
                    1
                } else {
                    self.document.buffer.line_count()
                };
                return Ok(applied(self.go_to_line(row, 1, matches!(self.mode, Mode::Visual | Mode::VisualLine))?));
            }
            if let Some(motion) = command_motion(call.id) {
                return Ok(applied(self.move_cursors(
                    motion,
                    count,
                    matches!(self.mode, Mode::Visual | Mode::VisualLine),
                )?));
            }
            let s = self.document.snapshot();
            let cursors = self
                .cursors
                .iter()
                .map(|c| {
                    let r = vim_range(&s, c, motion, count, self.rows, self.settings.editor.tab_size)?;
                    Ok(Cursor { anchor: r.start, head: r.end, desired_column: None })
                })
                .collect::<io::Result<Vec<_>>>()?;
            return Ok(applied(self.set_cursors(cursors, self.primary)?));
        }
        match call.id {
            "operator.delete" | "operator.change" | "operator.yank" => {
                let Some(motion) = call.motion else {
                    return Ok(message("operator requires a motion"));
                };
                let s = self.document.snapshot();
                let ranges = self
                    .cursors
                    .iter()
                    .map(|c| vim_range(&s, c, motion, count, self.rows, self.settings.editor.tab_size))
                    .collect::<io::Result<Vec<_>>>()?;
                let data = ClipboardData {
                    snapshot: s,
                    ranges: ranges.clone(),
                    linewise: matches!(motion, VimMotion::Line | VimMotion::Up | VimMotion::Down),
                };
                let register = call.register.unwrap_or('"');
                if register != '_' {
                    if register.is_ascii_uppercase() {
                        let key = register.to_ascii_lowercase();
                        if let Some(previous) = self.registers.get(&key) {
                            let mut s = Snapshot::default();
                            for item in [previous, &data] {
                                let snapshot = item.joined(self.document.line_ending.bytes())?;
                                s = s.concat(&snapshot)?;
                            }
                            let len = s.len();
                            self.registers.insert(
                                key,
                                ClipboardData {
                                    snapshot: s,
                                    ranges: std::iter::once(0..len).collect(),
                                    linewise: data.linewise,
                                },
                            );
                        } else {
                            self.registers.insert(key, data.clone());
                        }
                    } else {
                        self.registers.insert(register, data.clone());
                    }
                    self.registers.insert('"', data);
                }
                if call.id == "operator.yank" {
                    self.mode = Mode::Normal;
                    self.end_burst();
                    return Ok(applied(Damage { status: true, ..Damage::default() }));
                }
                let damage = self.apply(ranges.into_iter().map(Edit::delete).collect(), None, true)?;
                self.repeat = Some(Repeat::Vim(call.clone(), Vec::new()));
                if call.id == "operator.change" {
                    self.start_insert(Some(call.clone()));
                    self.join_insert = true;
                } else {
                    self.mode = Mode::Normal;
                }
                Ok(applied(damage))
            }
            "edit.pasteAfter" | "edit.pasteBefore" => {
                let Some(data) = self.registers.get(&call.register.unwrap_or('"').to_ascii_lowercase()).cloned() else {
                    return Ok(message("register is empty"));
                };
                let damage = self.paste_data(&data, Some(call.id == "edit.pasteAfter"), count)?;
                self.repeat = Some(Repeat::Vim(call.clone(), Vec::new()));
                Ok(applied(damage))
            }
            "edit.repeat" => {
                let Some(repeat) = self.repeat.clone() else {
                    return Ok(message("no completed change to repeat"));
                };
                let mut result = applied(Damage::default());
                for _ in 0..count.min(4096) {
                    match &repeat {
                        Repeat::Insert(bytes) => result = applied(self.insert_bytes(bytes, None)?),
                        Repeat::Vim(command, bytes) => {
                            result = self.execute_vim(command)?;
                            if !bytes.is_empty() {
                                result.damage = self.insert_bytes(bytes, Some(Duration::ZERO))?;
                            }
                            self.mode = Mode::Normal;
                            self.finish_insert();
                        }
                    }
                }
                self.repeat = Some(repeat);
                Ok(result)
            }
            "mode.normal" => {
                self.finish_insert();
                self.mode = Mode::Normal;
                let cursors = self.cursors.iter().map(|c| Cursor::caret(c.head)).collect();
                Ok(applied(self.set_cursors(cursors, self.primary)?))
            }
            "mode.visual" | "mode.visualLine" => {
                self.end_burst();
                self.mode = if call.id == "mode.visual" { Mode::Visual } else { Mode::VisualLine };
                Ok(applied(Damage { status: true, ..Damage::default() }))
            }
            "edit.insert" | "mode.insert" => {
                self.start_insert(Some(call.clone()));
                Ok(applied(Damage::default()))
            }
            "edit.append" | "edit.insertLineStart" | "edit.appendLineEnd" => {
                let motion = match call.id {
                    "edit.append" => Motion::Right,
                    "edit.insertLineStart" => Motion::SmartHome,
                    _ => Motion::LineEnd,
                };
                let d = self.move_cursors(motion, 1, false)?;
                self.start_insert(Some(call.clone()));
                Ok(applied(d))
            }
            "edit.openBelow" | "edit.openAbove" => {
                let above = call.id == "edit.openAbove";
                self.move_cursors(if above { Motion::LineStart } else { Motion::LineEnd }, 1, false)?;
                let damage = self.newline()?;
                if above {
                    self.move_cursors(Motion::Up, 1, false)?;
                }
                self.start_insert(Some(call.clone()));
                self.join_insert = true;
                Ok(applied(damage))
            }
            "edit.deleteCharacter" => {
                let mut op = invocation("operator.delete", Some(VimMotion::Right), count);
                op.register = call.register;
                self.execute_vim(&op)
            }
            _ => self.execute(call.id, &CommandArgs::default(), count),
        }
    }
}

fn applied(damage: Damage) -> CommandResult {
    CommandResult { damage, status: CommandStatus::Applied }
}
fn message(message: &str) -> CommandResult {
    CommandResult {
        damage: Damage { status: true, ..Damage::default() },
        status: CommandStatus::Message(message.into()),
    }
}
fn invocation(id: &'static str, motion: Option<VimMotion>, count: u32) -> Invocation {
    Invocation {
        id,
        motion,
        count,
        explicit_count: false,
        register: None,
        argument: kwil_core_commands::vim::Argument::None,
    }
}
fn id_to_static(id: &str) -> &'static str {
    kwil_core_commands::command(id).map_or("", |c| c.id)
}
fn command_motion(id: &str) -> Option<Motion> {
    Some(match id {
        "motion.left" => Motion::Left,
        "motion.right" => Motion::Right,
        "motion.up" => Motion::Up,
        "motion.down" => Motion::Down,
        "motion.wordForward" => Motion::WordForward,
        "motion.wordBackward" => Motion::WordBackward,
        "motion.wordEnd" => Motion::WordEnd,
        "motion.lineStart" => Motion::LineStart,
        "motion.smartHome" => Motion::SmartHome,
        "motion.lineEnd" => Motion::LineEnd,
        "motion.firstLine" => Motion::DocumentStart,
        "motion.lastLine" => Motion::DocumentEnd,
        "motion.pageUp" => Motion::PageUp,
        "motion.pageDown" => Motion::PageDown,
        _ => return None,
    })
}

fn vim_range(
    s: &Snapshot,
    cursor: &Cursor,
    motion: VimMotion,
    count: u32,
    rows: usize,
    tab: u8,
) -> io::Result<Range<u64>> {
    let at = cursor.head;
    let row = text::row(s, at)?;
    match motion {
        VimMotion::Selection => Ok(cursor.selection()),
        VimMotion::Line | VimMotion::Up | VimMotion::Down => {
            let start_row = if motion == VimMotion::Up {
                row.saturating_sub(u64::from(count))
            } else {
                text::row(s, cursor.selection().start)?
            };
            let end_row = if motion == VimMotion::Line {
                let end = text::row(s, cursor.selection().end)?;
                end.max(start_row) + u64::from(count)
            } else if motion == VimMotion::Down {
                row + u64::from(count) + 1
            } else {
                row + 1
            };
            Ok(text::line_full(s, start_row)?.start
                ..s.line_start(end_row).map_err(io::Error::other)?.unwrap_or(s.len()))
        }
        VimMotion::InnerWord | VimMotion::AroundWord => text::word_range(s, at, motion == VimMotion::AroundWord),
        VimMotion::InnerQuote | VimMotion::AroundQuote | VimMotion::InnerParentheses | VimMotion::AroundParentheses => {
            let quote = matches!(motion, VimMotion::InnerQuote | VimMotion::AroundQuote);
            let around = matches!(motion, VimMotion::AroundQuote | VimMotion::AroundParentheses);
            let (open, close) = if quote { (b'"', b'"') } else { (b'(', b')') };
            let start_bound = if quote { text::line(s, row)?.start } else { at.saturating_sub(text::WINDOW) };
            let end_bound = if quote { text::line(s, row)?.end } else { (at + text::WINDOW).min(s.len()) };
            let mut p = at;
            let mut depth = 0;
            let mut start = None;
            loop {
                let b = text::byte(s, p)?;
                if !quote && b == Some(close) {
                    depth += 1;
                }
                if b == Some(open) {
                    if depth == 0 {
                        start = Some(p);
                        break;
                    }
                    depth -= 1;
                }
                if p <= start_bound {
                    break;
                }
                p -= 1;
            }
            let Some(start) = start else {
                return Err(io::Error::other("text object opening delimiter not found in bounded scan"));
            };
            let mut p = start + 1;
            let mut depth = 0;
            while p < end_bound {
                let b = text::byte(s, p)?;
                if !quote && b == Some(open) {
                    depth += 1;
                }
                if b == Some(close) {
                    if depth == 0 {
                        return Ok(if around { start..p + 1 } else { start + 1..p });
                    }
                    depth -= 1;
                }
                p += 1;
            }
            Err(io::Error::other("text object closing delimiter not found in bounded scan"))
        }
        _ => {
            let target = motion_target(s, at, command_motion(motion.id()).unwrap(), count, None, rows, tab)?;
            let end = if matches!(motion, VimMotion::WordEnd) { text::next(s, target)? } else { target };
            Ok(if motion == VimMotion::LineEnd { at..text::line(s, row)?.end } else { at.min(end)..at.max(end) })
        }
    }
}
