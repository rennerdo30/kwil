//! Bounded Similar 3.2 byte-word boundaries. UTF-8 decoding uses the same
//! maximal invalid subpart rule as bstr 1.13.1, without a decoded string.
use super::WordInput;
use crate::store::{Records, attempt, gate};
use kwil_core_text::compare_input::{
    Admission, AtomLookup, AtomSpan, CaptureKind, InputError, PayloadSpace, ReadWork, Step,
};
use std::{ops::Range, sync::atomic::AtomicBool};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Token {
    pub start: u64,
    pub end: u64,
    pub opaque: bool,
    pub encoding: u64,
    pub raw_start: u64,
    pub raw_end: u64,
}
impl Token {
    pub fn words(self) -> [u64; 8] {
        [self.start, self.end, u64::from(self.opaque), self.encoding, self.raw_start, self.raw_end, 0, 0]
    }
    pub fn decode(w: [u64; 8]) -> Result<Self, InputError> {
        if w[0] >= w[1] || w[2] > 1 || w[6] != 0 || w[7] != 0 || (w[2] == 1 && (w[1] - w[0] != 1 || w[4] >= w[5])) {
            return Err(InputError::CorruptStore);
        }
        Ok(Self { start: w[0], end: w[1], opaque: w[2] == 1, encoding: w[3], raw_start: w[4], raw_end: w[5] })
    }
}

pub(super) struct Window {
    bytes: [u8; 256],
    at: u64,
    len: usize,
    space: PayloadSpace,
    pending: Option<(PayloadSpace, u64, usize)>,
}
impl Window {
    pub fn new() -> Self {
        Self { bytes: [0; 256], at: 0, len: 0, space: PayloadSpace::Logical, pending: None }
    }
    pub fn byte(
        &mut self,
        source: &mut impl WordInput,
        space: PayloadSpace,
        at: u64,
        end: u64,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<u8> {
        if self.pending.is_none() && self.space == space && at >= self.at && at - self.at < self.len as u64 {
            gate!(work.debit(1, 0, cancel));
            return Step::Ready(self.bytes[(at - self.at) as usize]);
        }
        if at >= end {
            return Step::Failed(InputError::SourceContract);
        }
        let (s, p, n) = *self.pending.get_or_insert((space, at, (end - at).min(256) as usize));
        let n = gate!(source.read_at(s, p, &mut self.bytes[..n], work, cancel));
        if n == 0 || n > 256 {
            return Step::Failed(InputError::SourceContract);
        }
        self.pending = None;
        self.space = s;
        self.at = p;
        self.len = n;
        if s != space || at < p || at - p >= n as u64 {
            return Step::Failed(InputError::SourceContract);
        }
        gate!(work.debit(1, 0, cancel));
        Step::Ready(self.bytes[(at - p) as usize])
    }
}

/// At most four source bytes are sufficient to distinguish a valid scalar,
/// an invalid maximal subpart, and a truncated EOF prefix. Original endpoints
/// come from consumed bytes, never replacement-character UTF-8 lengths.
pub(super) fn decode(bytes: &[u8]) -> (char, usize) {
    if bytes.is_empty() {
        return ('\u{fffd}', 0);
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => {
            let c = s.chars().next().unwrap();
            (c, c.len_utf8())
        }
        Err(e) if e.valid_up_to() != 0 => {
            let c = std::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap().chars().next().unwrap();
            (c, c.len_utf8())
        }
        Err(e) => ('\u{fffd}', e.error_len().unwrap_or(bytes.len())),
    }
}

pub(super) struct Scanner {
    at: u64,
    end: u64,
    atom: Option<AtomSpan>,
    lookup: Option<AtomLookup>,
    window: Window,
    look: [u8; 4],
    filled: usize,
    decode_credit: usize,
    unit: Option<(Token, bool)>,
    token: Option<(Token, bool)>,
    pub opaque: bool,
}
impl Scanner {
    pub fn new(range: Range<u64>) -> Self {
        Self {
            at: range.start,
            end: range.end,
            atom: None,
            lookup: None,
            window: Window::new(),
            look: [0; 4],
            filled: 0,
            decode_credit: 0,
            unit: None,
            token: None,
            opaque: false,
        }
    }
    fn atom(&mut self, source: &impl WordInput, at: u64, work: &mut ReadWork, cancel: &AtomicBool) -> Step<AtomSpan> {
        if let Some(a) = self.atom
            && at >= a.start
            && at < a.end
        {
            return Step::Ready(a);
        }
        let lookup = self.lookup.get_or_insert_with(|| AtomLookup::new(at));
        let a = gate!(source.atom_span(lookup, work, cancel));
        self.lookup = None;
        let Some(a) = a else {
            return Step::Failed(InputError::SourceContract);
        };
        if a.start > at || a.end <= at || a.end > source.byte_len() {
            return Step::Failed(InputError::SourceContract);
        }
        self.atom = Some(a);
        Step::Ready(a)
    }
    fn peek(
        &mut self,
        source: &mut impl WordInput,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<Option<(Token, bool)>> {
        if self.unit.is_some() {
            return Step::Ready(self.unit);
        }
        if self.at == self.end {
            return Step::Ready(None);
        }
        let first = if self.filled == 0 { Some(gate!(self.atom(source, self.at, work, cancel))) } else { None };
        if let Some(a) = first
            && a.kind == CaptureKind::Opaque
        {
            if a.end - a.start != 1 || a.original_start >= a.original_end || a.original_end > source.original_len() {
                return Step::Failed(InputError::SourceContract);
            }
            gate!(work.debit(1, 0, cancel));
            self.opaque = true;
            self.unit = Some((
                Token {
                    start: self.at,
                    end: self.at + 1,
                    opaque: true,
                    encoding: a.encoding,
                    raw_start: a.original_start,
                    raw_end: a.original_end,
                },
                false,
            ));
            return Step::Ready(self.unit);
        }
        while self.filled < 4 && (self.filled as u64) < self.end - self.at {
            let p = self.at + self.filled as u64;
            if gate!(self.atom(source, p, work, cancel)).kind == CaptureKind::Opaque {
                break;
            }
            self.look[self.filled] = gate!(self.window.byte(source, PayloadSpace::Logical, p, self.end, work, cancel));
            self.filled += 1;
        }
        // Decode is fixed work and explicitly credited across one-byte quanta.
        while self.decode_credit < 16 {
            let n = (16 - self.decode_credit).min(work.bytes.min(16) as usize);
            if n == 0 {
                return Step::Pending;
            }
            gate!(work.debit(n as u64, 0, cancel));
            self.decode_credit += n;
        }
        let (c, n) = decode(&self.look[..self.filled]);
        if n == 0 {
            return Step::Failed(InputError::SourceContract);
        }
        self.unit = Some((
            Token { start: self.at, end: self.at + n as u64, opaque: false, encoding: 0, raw_start: 0, raw_end: 0 },
            c.is_whitespace(),
        ));
        Step::Ready(self.unit)
    }
    fn consume(&mut self, t: Token) {
        self.at = t.end;
        self.filled = 0;
        self.decode_credit = 0;
        self.unit = None;
    }
    pub fn next(
        &mut self,
        source: &mut impl WordInput,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<Option<Token>> {
        loop {
            gate!(work.debit(0, 0, cancel));
            let Some((next, white)) = gate!(self.peek(source, work, cancel)) else {
                return Step::Ready(self.token.take().map(|v| v.0));
            };
            match self.token {
                None => {
                    self.token = Some((next, white));
                    self.consume(next);
                }
                Some((mut token, w)) if w == white && !token.opaque && !next.opaque => {
                    token.end = next.end;
                    self.token = Some((token, w));
                    self.consume(next);
                }
                Some(_) => return Step::Ready(self.token.take().map(|v| v.0)),
            }
        }
    }
}

pub(super) struct Equality {
    indices: [u64; 2],
    tokens: [Option<Token>; 2],
    windows: [Window; 2],
    offset: u64,
    first: Option<u8>,
    second: Option<u8>,
}
impl Equality {
    pub fn new(a: u64, b: u64) -> Self {
        Self {
            indices: [a, b],
            tokens: [None, None],
            windows: [Window::new(), Window::new()],
            offset: 0,
            first: None,
            second: None,
        }
    }
    pub fn step<A: Admission>(
        &mut self,
        left: &mut impl WordInput,
        right: &mut impl WordInput,
        records: &mut Records<A>,
        right_base: u64,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<bool> {
        for i in 0..2 {
            if self.tokens[i].is_none() {
                let at = attempt!(
                    self.indices[i].checked_add(if i == 0 { 0 } else { right_base }).ok_or(InputError::Overflow)
                );
                self.tokens[i] =
                    Some(attempt!(Token::decode(gate!(records.read(crate::store::RUNS, at, work, cancel)))));
            }
        }
        let [a, b] = self.tokens.map(Option::unwrap);
        if a.opaque != b.opaque {
            return Step::Ready(false);
        }
        let (space, start, end) = if a.opaque {
            if a.encoding != b.encoding || a.raw_end - a.raw_start != b.raw_end - b.raw_start {
                return Step::Ready(false);
            }
            (PayloadSpace::Original, [a.raw_start, b.raw_start], [a.raw_end, b.raw_end])
        } else {
            if a.end - a.start != b.end - b.start {
                return Step::Ready(false);
            }
            (PayloadSpace::Logical, [a.start, b.start], [a.end, b.end])
        };
        while self.offset < end[0] - start[0] {
            if self.first.is_none() {
                self.first =
                    Some(gate!(self.windows[0].byte(left, space, start[0] + self.offset, end[0], work, cancel)));
            }
            if self.second.is_none() {
                self.second =
                    Some(gate!(self.windows[1].byte(right, space, start[1] + self.offset, end[1], work, cancel)));
            }
            gate!(work.debit(1, 0, cancel));
            if self.first.take().unwrap() != self.second.take().unwrap() {
                return Step::Ready(false);
            }
            self.offset += 1;
        }
        Step::Ready(true)
    }
}
