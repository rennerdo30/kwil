//! A document-independent Vim grammar. The editor owns ranges, registers,
//! inserted text, undo groups and the last completed change. In particular,
//! `edit.repeat` asks it to replay that transaction (including inserted text),
//! not merely the last key. No document bytes enter this parser.
use crate::{Key, KeyChord, Mode, NamedKey};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Left,
    Down,
    Up,
    Right,
    WordForward,
    WordBackward,
    WordEnd,
    LineStart,
    LineEnd,
    FirstLine,
    LastLine,
    Line,
    Selection,
    InnerWord,
    AroundWord,
    InnerQuote,
    AroundQuote,
    InnerParentheses,
    AroundParentheses,
}
impl Motion {
    pub fn from_id(id: &str) -> Option<Self> {
        [
            Self::Left,
            Self::Down,
            Self::Up,
            Self::Right,
            Self::WordForward,
            Self::WordBackward,
            Self::WordEnd,
            Self::LineStart,
            Self::LineEnd,
            Self::FirstLine,
            Self::LastLine,
            Self::Line,
            Self::Selection,
            Self::InnerWord,
            Self::AroundWord,
            Self::InnerQuote,
            Self::AroundQuote,
            Self::InnerParentheses,
            Self::AroundParentheses,
        ]
        .into_iter()
        .find(|motion| motion.id() == id)
    }
    pub fn id(self) -> &'static str {
        match self {
            Self::Left => "motion.left",
            Self::Down => "motion.down",
            Self::Up => "motion.up",
            Self::Right => "motion.right",
            Self::WordForward => "motion.wordForward",
            Self::WordBackward => "motion.wordBackward",
            Self::WordEnd => "motion.wordEnd",
            Self::LineStart => "motion.lineStart",
            Self::LineEnd => "motion.lineEnd",
            Self::FirstLine => "motion.firstLine",
            Self::LastLine => "motion.lastLine",
            Self::Line => "motion.line",
            Self::Selection => "motion.selection",
            Self::InnerWord => "motion.innerWord",
            Self::AroundWord => "motion.aroundWord",
            Self::InnerQuote => "motion.innerQuote",
            Self::AroundQuote => "motion.aroundQuote",
            Self::InnerParentheses => "motion.innerParentheses",
            Self::AroundParentheses => "motion.aroundParentheses",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Argument {
    None,
    /// Standalone gj/gk intent. The registered up/down id remains executable
    /// as logical movement when wrapping does not own the text body. While
    /// wrapping owns it, frontends dispatch this intent through the shared
    /// WrapHost visual navigation API, preserving count and selection extension.
    VisualMotion {
        extend: bool,
    },
    Search(String),
    Substitute {
        pattern: String,
        replacement: String,
        global: bool,
        whole_file: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub id: &'static str,
    /// Operator count times motion count. Always nonzero.
    pub count: u32,
    /// Distinguishes G (last line) from 1G (line 1). For a line motion the
    /// executor interprets an explicit count as an absolute line number.
    pub explicit_count: bool,
    pub motion: Option<Motion>,
    pub register: Option<char>,
    pub argument: Argument,
}

impl Invocation {
    /// Explicit visual-row intent, distinct from logical j/k and operators.
    /// Record/replay must preserve the complete invocation, including argument;
    /// reconstructing it from the command id loses this distinction.
    pub fn visual_motion(&self) -> Option<Motion> {
        if !matches!(self.argument, Argument::VisualMotion { .. }) {
            return None;
        }
        match (self.id, self.motion) {
            ("motion.down", Some(Motion::Down)) => Some(Motion::Down),
            ("motion.up", Some(Motion::Up)) => Some(Motion::Up),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VimResult {
    Pending,
    Command(Invocation),
    Unbound,
    Error(String),
}

#[derive(Clone, Copy, Debug)]
struct Operator {
    key: char,
    count: u32,
    explicit: bool,
}

#[derive(Clone, Copy, Debug)]
enum Prefix {
    G,
    Object(bool),
    Register,
}

#[derive(Clone, Debug, Default)]
pub struct Vim {
    mode: Mode,
    count: Option<u32>,
    operator: Option<Operator>,
    prefix: Option<Prefix>,
    register: Option<char>,
    command_line: String,
    search_line: bool,
    return_mode: Mode,
}

impl Vim {
    pub fn mode(&self) -> Mode {
        self.mode
    }
    pub fn operator_pending(&self) -> bool {
        self.operator.is_some()
    }
    pub fn command_line(&self) -> Option<&str> {
        (self.mode == Mode::Command).then_some(self.command_line.as_str())
    }
    /// Cancel partial grammar on focus changes or after an explicit keymap
    /// override. Does not exit Insert/Visual mode.
    pub fn cancel_pending(&mut self) {
        self.count = None;
        self.operator = None;
        self.prefix = None;
        self.register = None;
    }

    /// Cancel focus-dependent input, including an unfinished ex/search line.
    /// Insert and Visual modes survive focus changes.
    pub fn cancel_input(&mut self) {
        self.cancel_pending();
        if self.mode == Mode::Command {
            self.mode = self.return_mode;
            self.command_line.clear();
        }
    }

    /// Apply an explicit binding without losing Vim's count, operator or mode
    /// semantics. This is also the palette's entry point for modal actions.
    pub fn invoke(&mut self, id: &'static str) -> VimResult {
        if let Some(motion) = Motion::from_id(id) {
            return self.motion(motion);
        }
        let operator = match id {
            "operator.delete" => Some('d'),
            "operator.change" => Some('c'),
            "operator.yank" => Some('y'),
            _ => None,
        };
        if let Some(operator) = operator {
            return self.feed(KeyChord::new(Key::Character(operator), Default::default()));
        }
        if self.mode == Mode::Command {
            self.mode = self.return_mode;
            self.command_line.clear();
        }
        self.operator = None;
        self.prefix = None;
        match id {
            "mode.normal" => self.mode = Mode::Normal,
            "mode.insert"
            | "edit.insert"
            | "edit.append"
            | "edit.openBelow"
            | "edit.openAbove"
            | "edit.insertLineStart"
            | "edit.appendLineEnd" => self.mode = Mode::Insert,
            "mode.visual" => self.mode = Mode::Visual,
            "mode.visualLine" => self.mode = Mode::VisualLine,
            "mode.command" => {
                self.return_mode = self.mode;
                self.mode = Mode::Command;
                self.search_line = false;
            }
            _ => {}
        }
        self.simple(id)
    }

    fn emit(&mut self, id: &'static str, motion: Option<Motion>, argument: Argument) -> VimResult {
        let op = self.operator;
        let explicit = self.count.is_some() || op.is_some_and(|o| o.explicit);
        let Some(count) = self.count.unwrap_or(1).checked_mul(op.map_or(1, |o| o.count)) else {
            return self.error("count overflow");
        };
        let invocation = Invocation { id, count, explicit_count: explicit, motion, register: self.register, argument };
        self.cancel_pending();
        VimResult::Command(invocation)
    }
    fn simple(&mut self, id: &'static str) -> VimResult {
        self.emit(id, None, Argument::None)
    }
    fn error(&mut self, message: &str) -> VimResult {
        self.cancel_pending();
        VimResult::Error(message.into())
    }
    fn motion(&mut self, motion: Motion) -> VimResult {
        if let Some(operator) = self.operator {
            let id = match operator.key {
                'd' => "operator.delete",
                'c' => "operator.change",
                _ => "operator.yank",
            };
            let result = self.emit(id, Some(motion), Argument::None);
            if operator.key == 'c' && matches!(result, VimResult::Command(_)) {
                self.mode = Mode::Insert;
            }
            result
        } else {
            self.emit(motion.id(), Some(motion), Argument::None)
        }
    }

    fn visual_motion(&mut self, motion: Motion) -> VimResult {
        // Decision 0003 preserves logical operator ranges. Until visual-row
        // operator semantics are specified, refuse before emitting a command
        // or entering Insert mode; never reinterpret dgj/cgk as logical dj/ck.
        if self.operator.is_some() {
            return self.error("visual-row motions are not supported for operators; use j or k");
        }
        let extend = matches!(self.mode, Mode::Visual | Mode::VisualLine);
        self.emit(motion.id(), Some(motion), Argument::VisualMotion { extend })
    }

    pub fn feed(&mut self, key: KeyChord) -> VimResult {
        if key.key == Key::Named(NamedKey::Escape) {
            self.cancel_pending();
            self.command_line.clear();
            self.mode = Mode::Normal;
            return self.simple("mode.normal");
        }
        if self.mode == Mode::Command {
            return self.feed_command(key);
        }
        if self.mode == Mode::Insert {
            return VimResult::Unbound;
        }
        if key.modifiers.ctrl
            && !key.modifiers.alt
            && !key.modifiers.super_key
            && !key.modifiers.shift
            && key.key == Key::Character('r')
        {
            if self.operator.is_some() || self.prefix.is_some() {
                return self.error("incomplete operator");
            }
            return self.simple("edit.redo");
        }
        let Some(c) = key.character() else {
            self.cancel_pending();
            return VimResult::Unbound;
        };
        if let Some(prefix) = self.prefix.take() {
            return match prefix {
                Prefix::Register if c.is_ascii_alphanumeric() || "\"_+*-".contains(c) => {
                    self.register = Some(c);
                    VimResult::Pending
                }
                Prefix::Register => self.error("invalid register"),
                Prefix::G if c == 'g' => self.motion(Motion::FirstLine),
                Prefix::G if c == 'j' => self.visual_motion(Motion::Down),
                Prefix::G if c == 'k' => self.visual_motion(Motion::Up),
                Prefix::G => self.error("expected gg, gj or gk"),
                Prefix::Object(around) => {
                    let motion = match (around, c) {
                        (false, 'w') => Motion::InnerWord,
                        (true, 'w') => Motion::AroundWord,
                        (false, '"') => Motion::InnerQuote,
                        (true, '"') => Motion::AroundQuote,
                        (false, '(' | ')') => Motion::InnerParentheses,
                        (true, '(' | ')') => Motion::AroundParentheses,
                        _ => return self.error("unknown text object"),
                    };
                    self.motion(motion)
                }
            };
        }
        if c.is_ascii_digit() && (c != '0' || self.count.is_some()) {
            let Some(count) =
                self.count.unwrap_or(0).checked_mul(10).and_then(|n| n.checked_add(c as u32 - '0' as u32))
            else {
                return self.error("count overflow");
            };
            self.count = Some(count);
            return VimResult::Pending;
        }
        if c == '"' && self.operator.is_none() {
            self.prefix = Some(Prefix::Register);
            return VimResult::Pending;
        }
        let visual = matches!(self.mode, Mode::Visual | Mode::VisualLine);
        if matches!(c, 'i' | 'a') && (self.operator.is_some() || visual) {
            self.prefix = Some(Prefix::Object(c == 'a'));
            return VimResult::Pending;
        }
        if let Some(operator) = self.operator
            && c == operator.key
        {
            return self.motion(Motion::Line);
        }
        let motion = match c {
            'h' => Some(Motion::Left),
            'j' => Some(Motion::Down),
            'k' => Some(Motion::Up),
            'l' => Some(Motion::Right),
            'w' => Some(Motion::WordForward),
            'b' => Some(Motion::WordBackward),
            'e' => Some(Motion::WordEnd),
            '0' => Some(Motion::LineStart),
            '$' => Some(Motion::LineEnd),
            'G' => Some(Motion::LastLine),
            _ => None,
        };
        if let Some(motion) = motion {
            return self.motion(motion);
        }
        if c == 'g' {
            self.prefix = Some(Prefix::G);
            return VimResult::Pending;
        }
        if self.operator.is_some() {
            return self.error("expected motion or text object");
        }
        if matches!(c, 'd' | 'c' | 'y') {
            if visual {
                let motion = if self.mode == Mode::VisualLine { Motion::Line } else { Motion::Selection };
                self.mode = if c == 'c' { Mode::Insert } else { Mode::Normal };
                return self.emit(
                    match c {
                        'd' => "operator.delete",
                        'c' => "operator.change",
                        _ => "operator.yank",
                    },
                    Some(motion),
                    Argument::None,
                );
            }
            let explicit = self.count.is_some();
            self.operator = Some(Operator { key: c, count: self.count.take().unwrap_or(1), explicit });
            return VimResult::Pending;
        }
        let id = match c {
            'x' => "edit.deleteCharacter",
            'p' => "edit.pasteAfter",
            'P' => "edit.pasteBefore",
            'u' => "edit.undo",
            '.' => "edit.repeat",
            'n' => "search.findNext",
            'N' => "search.findPrevious",
            'i' | 'a' | 'o' | 'O' | 'I' | 'A' => {
                self.mode = Mode::Insert;
                match c {
                    'i' => "edit.insert",
                    'a' => "edit.append",
                    'o' => "edit.openBelow",
                    'O' => "edit.openAbove",
                    'I' => "edit.insertLineStart",
                    _ => "edit.appendLineEnd",
                }
            }
            'v' | 'V' => {
                let mode = if c == 'v' { Mode::Visual } else { Mode::VisualLine };
                self.mode = if self.mode == mode { Mode::Normal } else { mode };
                match self.mode {
                    Mode::Visual => "mode.visual",
                    Mode::VisualLine => "mode.visualLine",
                    _ => "mode.normal",
                }
            }
            ':' | '/' => {
                self.return_mode = self.mode;
                self.mode = Mode::Command;
                self.search_line = c == '/';
                self.command_line.clear();
                self.cancel_pending();
                return VimResult::Pending;
            }
            _ => {
                self.cancel_pending();
                return VimResult::Unbound;
            }
        };
        self.simple(id)
    }

    fn feed_command(&mut self, key: KeyChord) -> VimResult {
        match key.key {
            Key::Named(NamedKey::Backspace) => {
                if self.command_line.pop().is_none() {
                    self.mode = self.return_mode;
                }
                VimResult::Pending
            }
            Key::Named(NamedKey::Enter) => {
                self.mode = self.return_mode;
                let line = std::mem::take(&mut self.command_line);
                if self.search_line {
                    return self.emit("search.find", None, Argument::Search(line));
                }
                match line.trim() {
                    "w" => self.simple("file.save"),
                    "q" => self.simple("file.quit"),
                    "wq" => self.simple("file.saveAndQuit"),
                    ex => match parse_substitute(ex) {
                        Ok(argument) => self.emit("search.substitute", None, argument),
                        Err(error) => self.error(error),
                    },
                }
            }
            _ => {
                if let Some(c) = key.character() {
                    // Bound command input, independent of document size.
                    if self.command_line.len() + c.len_utf8() > 4096 {
                        return self.error("command line exceeds 4096 bytes");
                    }
                    self.command_line.push(c);
                    VimResult::Pending
                } else {
                    VimResult::Unbound
                }
            }
        }
    }
}

fn parse_substitute(line: &str) -> Result<Argument, &'static str> {
    let (line, whole_file) = line.strip_prefix('%').map_or((line, false), |line| (line, true));
    let Some(mut chars) = line.strip_prefix('s').map(str::chars) else {
        return Err("unknown ex command");
    };
    let delimiter = chars
        .next()
        .filter(|c| !c.is_alphanumeric() && !c.is_whitespace() && *c != '\\')
        .ok_or("invalid substitution delimiter")?;
    fn field(chars: &mut std::str::Chars<'_>, delimiter: char) -> Result<String, &'static str> {
        let mut result = String::new();
        while let Some(c) = chars.next() {
            if c == delimiter {
                return Ok(result);
            }
            if c == '\\' {
                let next = chars.next().ok_or("dangling escape")?;
                if next != delimiter {
                    result.push('\\');
                }
                result.push(next);
            } else {
                result.push(c);
            }
        }
        Err("unterminated substitution")
    }
    let pattern = field(&mut chars, delimiter)?;
    let replacement = field(&mut chars, delimiter)?;
    let global = match chars.as_str() {
        "" => false,
        "g" => true,
        _ => return Err("unsupported substitution flags"),
    };
    Ok(Argument::Substitute { pattern, replacement, global, whole_file })
}
