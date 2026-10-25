//! Exact external-memory Myers traversal shared by line and token pairs.
//!
//! Pair callbacks and sinks serialize through one Records owner. Events remain
//! held until the sink acknowledges its final completed write. No allocation.
use crate::store::{self, Records, attempt, gate};
use kwil_core_text::compare_input::{Admission, InputError, ReadWork, Step};
use std::{mem::size_of, sync::atomic::AtomicBool};

/// Coordinates remain immutable. Pending equality resumes the same owned
/// state before any other operation on the pair or Records. Cancellation does
/// not reset pending reads: each caller explicitly resets only its own reads.
pub(crate) trait SequencePair {
    type Equality;
    fn lengths(&self) -> [u64; 2];
    fn begin_equal(&self, a: u64, b: u64) -> Self::Equality;
    fn equal<A: Admission>(
        &mut self,
        state: &mut Self::Equality,
        records: &mut Records<A>,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<bool>;
}

/// Record offsets in the existing FORWARD/REVERSE/STACK banks. Callers reserve
/// disjoint extents from descriptors/output retained in the same banks.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Layout {
    pub(crate) forward: u64,
    pub(crate) reverse: u64,
    pub(crate) stack: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Rect {
    pub(crate) a: u64,
    pub(crate) b: u64,
    pub(crate) c: u64,
    pub(crate) d: u64,
}
impl Rect {
    pub(crate) fn n(self) -> u64 {
        self.b - self.a
    }
    pub(crate) fn m(self) -> u64 {
        self.d - self.c
    }
    fn words(self, equal: bool) -> [u64; 8] {
        [self.a, self.b, self.c, self.d, u64::from(equal), 0, 0, 0]
    }
    fn decode(w: [u64; 8]) -> Result<(Self, bool), InputError> {
        if w[0] > w[1] || w[2] > w[3] || w[4] > 1 {
            return Err(InputError::CorruptStore);
        }
        Ok((Self { a: w[0], b: w[1], c: w[2], d: w[3] }, w[4] == 1))
    }
}

/// One depth uses opposite-parity slots from the preceding depth. No file
/// clearing pass and no stale frontier reads: depth/parity bounds gate access.
struct Bisect<E> {
    rect: Rect,
    limit: u64,
    depth: u64,
    k: i128,
    reverse: bool,
    stage: u8,
    candidates: [Option<u64>; 2],
    x: u64,
    y: u64,
    equality: Option<E>,
}
impl<E> Bisect<E> {
    fn new(rect: Rect) -> Result<Self, InputError> {
        let total = rect.n().checked_add(rect.m()).ok_or(InputError::Overflow)?;
        Ok(Self {
            rect,
            limit: total / 2 + total % 2,
            depth: 0,
            k: 0,
            reverse: false,
            stage: 0,
            candidates: [None; 2],
            x: 0,
            y: 0,
            equality: None,
        })
    }
    fn index(&self, k: i128, reverse: bool, layout: Layout) -> Result<u64, InputError> {
        let index = u64::try_from(k + i128::from(self.limit)).map_err(|_| InputError::Overflow)?;
        let base = if reverse { layout.reverse } else { layout.forward };
        base.checked_add(index).ok_or(InputError::Overflow)
    }
    fn valid_k(k: i128, depth: u64) -> bool {
        k.abs() <= i128::from(depth) && (k + i128::from(depth)) % 2 == 0
    }
    fn candidate(&self, x: u64, k: i128) -> Option<u64> {
        let y = i128::from(x) - k;
        (x <= self.rect.n() && y >= 0 && y <= i128::from(self.rect.m())).then_some(x)
    }
    fn step<P: SequencePair<Equality = E>, A: Admission>(
        &mut self,
        records: &mut Records<A>,
        pair: &mut P,
        layout: Layout,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<(u64, u64)> {
        loop {
            gate!(work.debit(0, 0, cancel));
            let file = if self.reverse { store::REVERSE } else { store::FORWARD };
            match self.stage {
                0 => {
                    self.candidates = [None; 2];
                    if self.depth == 0 {
                        self.x = 0;
                        self.y = 0;
                        self.stage = 3;
                        continue;
                    }
                    self.stage = 1;
                }
                1 => {
                    if self.k < i128::from(self.depth) {
                        let w = gate!(records.read(
                            file,
                            attempt!(self.index(self.k + 1, self.reverse, layout)),
                            work,
                            cancel
                        ));
                        if w[1] == 1 {
                            self.candidates[0] = self.candidate(w[0], self.k);
                        }
                    }
                    self.stage = 2;
                }
                2 => {
                    if self.k > -i128::from(self.depth) {
                        let w = gate!(records.read(
                            file,
                            attempt!(self.index(self.k - 1, self.reverse, layout)),
                            work,
                            cancel
                        ));
                        if w[1] == 1 {
                            self.candidates[1] = w[0].checked_add(1).and_then(|x| self.candidate(x, self.k));
                        }
                    }
                    if let Some(x) = self.candidates.into_iter().flatten().max() {
                        self.x = x;
                        self.y = (i128::from(x) - self.k) as u64;
                        self.stage = 3;
                    } else {
                        self.stage = 6;
                    }
                }
                3 => {
                    if self.x == self.rect.n() || self.y == self.rect.m() {
                        self.stage = 4;
                        continue;
                    }
                    let (a, b) = if self.reverse {
                        (self.rect.b - self.x - 1, self.rect.d - self.y - 1)
                    } else {
                        (self.rect.a + self.x, self.rect.c + self.y)
                    };
                    let equality = self.equality.get_or_insert_with(|| pair.begin_equal(a, b));
                    let equal = gate!(pair.equal(equality, records, work, cancel));
                    self.equality = None;
                    if equal {
                        self.x += 1;
                        self.y += 1;
                    } else {
                        self.stage = 4;
                    }
                }
                4 => {
                    gate!(records.write(
                        file,
                        attempt!(self.index(self.k, self.reverse, layout)),
                        [self.x, 1, 0, 0, 0, 0, 0, 0],
                        work,
                        cancel
                    ));
                    self.stage = 5;
                }
                5 => {
                    let delta = i128::from(self.rect.n()) - i128::from(self.rect.m());
                    let odd = delta % 2 != 0;
                    let other_depth = if !self.reverse && odd {
                        self.depth.checked_sub(1)
                    } else if self.reverse && !odd {
                        Some(self.depth)
                    } else {
                        None
                    };
                    if let Some(depth) = other_depth {
                        let other_k = delta - self.k;
                        if Self::valid_k(other_k, depth) {
                            let opposite = if self.reverse { store::FORWARD } else { store::REVERSE };
                            let w = gate!(records.read(
                                opposite,
                                attempt!(self.index(other_k, !self.reverse, layout)),
                                work,
                                cancel
                            ));
                            if w[1] == 1 && w[0] > self.rect.n() {
                                return Step::Failed(InputError::CorruptStore);
                            }
                            if w[1] == 1 && self.x >= self.rect.n() - w[0] {
                                let x = if self.reverse { w[0] } else { self.x };
                                let k = if self.reverse { other_k } else { self.k };
                                let y = (i128::from(x) - k) as u64;
                                if (x == 0 && y == 0) || (x == self.rect.n() && y == self.rect.m()) {
                                    return Step::Failed(InputError::CorruptStore);
                                }
                                return Step::Ready((self.rect.a + x, self.rect.c + y));
                            }
                        }
                    }
                    self.stage = 7;
                }
                6 => {
                    gate!(records.write(
                        file,
                        attempt!(self.index(self.k, self.reverse, layout)),
                        [0; 8],
                        work,
                        cancel
                    ));
                    self.stage = 7;
                }
                _ => {
                    gate!(work.debit(1, 0, cancel));
                    if self.k < i128::from(self.depth) {
                        self.k += 2;
                    } else if !self.reverse {
                        self.reverse = true;
                        self.k = -i128::from(self.depth);
                    } else {
                        if self.depth == self.limit {
                            return Step::Failed(InputError::CorruptStore);
                        }
                        self.depth += 1;
                        self.reverse = false;
                        self.k = -i128::from(self.depth);
                    }
                    self.stage = 0;
                }
            }
        }
    }
}

/// Exact traversal order. Changed always has one empty axis; the sink may
/// accumulate adjacent changes before pairing them into output rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Equal(Rect),
    Changed(Rect),
}

#[derive(Clone, Copy)]
enum Phase {
    Seed,
    Pop,
    Prefix,
    PrefixEmit,
    Suffix,
    QueueSuffix,
    Residual,
    Bisect,
    QueueRight,
    QueueLeft,
    Finish,
}

/// Fixed by-value state, including the concrete pair's equality state in
/// both possible locations. The caller prepays this retained size.
pub(crate) struct Sequence<E> {
    lengths: [u64; 2],
    layout: Layout,
    phase: Phase,
    stack: u64,
    rect: Rect,
    untrimmed: Rect,
    prefix: u64,
    suffix: u64,
    equal_task: bool,
    equality: Option<E>,
    bisect: Option<Bisect<E>>,
    split: (u64, u64),
    held: Option<Event>,
}

/// Add to the caller's retained charge in the same aggregate before construction.
/// Covers solver constructor/return overlaps, bisect construction, equality
/// callback construction and event delivery, all with actual generic sizes.
pub(crate) fn transient_charge<E>() -> Option<usize> {
    size_of::<Sequence<E>>()
        .checked_mul(2)?
        .checked_add(size_of::<Bisect<E>>().checked_mul(2)?)?
        .checked_add(size_of::<E>().checked_mul(2)?)?
        .checked_add(size_of::<Event>().checked_mul(3)?)
}

impl<E> Sequence<E> {
    pub(crate) fn new(lengths: [u64; 2], layout: Layout) -> Self {
        let rect = Rect { a: 0, b: lengths[0], c: 0, d: lengths[1] };
        Self {
            lengths,
            layout,
            phase: Phase::Seed,
            stack: 0,
            rect,
            untrimmed: rect,
            prefix: 0,
            suffix: 0,
            equal_task: false,
            equality: None,
            bisect: None,
            split: (0, 0),
            held: None,
        }
    }

    /// Only acknowledge after the sink's final operation returns Ready.
    /// A mismatch preserves the held event and all traversal state unchanged.
    pub(crate) fn acknowledge(&mut self, event: Event) -> Result<(), InputError> {
        if self.held != Some(event) {
            return Err(InputError::QueryChanged);
        }
        self.held = None;
        Ok(())
    }

    fn equal<P: SequencePair<Equality = E>, A: Admission>(
        &mut self,
        pair: &mut P,
        records: &mut Records<A>,
        a: u64,
        b: u64,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<bool> {
        let state = self.equality.get_or_insert_with(|| pair.begin_equal(a, b));
        let result = pair.equal(state, records, work, cancel);
        if matches!(result, Step::Ready(_)) {
            self.equality = None;
        }
        result
    }

    fn push_task<A: Admission>(
        &mut self,
        records: &mut Records<A>,
        rect: Rect,
        equal: bool,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<()> {
        let next = attempt!(self.stack.checked_add(1).ok_or(InputError::Overflow));
        let index = attempt!(self.layout.stack.checked_add(self.stack).ok_or(InputError::Overflow));
        gate!(records.write(store::STACK, index, rect.words(equal), work, cancel));
        self.stack = next;
        Step::Ready(())
    }
    /// Records must already be open. A held event repeats without accessing
    /// the pair or Records, even while the sink has a suspended transfer.
    pub(crate) fn next<P: SequencePair<Equality = E>, A: Admission>(
        &mut self,
        pair: &mut P,
        records: &mut Records<A>,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<Option<Event>> {
        loop {
            gate!(work.debit(0, 0, cancel));
            if let Some(event) = self.held {
                return Step::Ready(Some(event));
            }
            if self.lengths != pair.lengths() {
                return Step::Failed(InputError::SourceChanged);
            }
            match self.phase {
                Phase::Seed => {
                    gate!(self.push_task(records, self.rect, false, work, cancel));
                    self.phase = Phase::Pop;
                }
                Phase::Pop => {
                    if self.stack == 0 {
                        self.phase = Phase::Finish;
                        continue;
                    }
                    let w = gate!(records.read(
                        store::STACK,
                        attempt!(self.layout.stack.checked_add(self.stack - 1).ok_or(InputError::Overflow)),
                        work,
                        cancel
                    ));
                    let (rect, equal) = attempt!(Rect::decode(w));
                    if rect.b > self.lengths[0] || rect.d > self.lengths[1] || equal && rect.n() != rect.m() {
                        return Step::Failed(InputError::CorruptStore);
                    }
                    self.stack -= 1;
                    self.rect = rect;
                    self.untrimmed = rect;
                    self.equal_task = equal;
                    self.prefix = 0;
                    self.suffix = 0;
                    self.phase = if equal { Phase::PrefixEmit } else { Phase::Prefix };
                }
                Phase::Prefix => {
                    if self.rect.a == self.rect.b || self.rect.c == self.rect.d {
                        self.phase = Phase::PrefixEmit;
                    } else if gate!(self.equal(pair, records, self.rect.a, self.rect.c, work, cancel)) {
                        self.rect.a += 1;
                        self.rect.c += 1;
                        self.prefix += 1;
                    } else {
                        self.phase = Phase::PrefixEmit;
                    }
                }
                Phase::PrefixEmit => {
                    let length = if self.equal_task { self.rect.n() } else { self.prefix };
                    if length > 0 {
                        self.held = Some(Event::Equal(Rect {
                            a: self.untrimmed.a,
                            b: self.untrimmed.a + length,
                            c: self.untrimmed.c,
                            d: self.untrimmed.c + length,
                        }));
                    }
                    self.phase = if self.equal_task { Phase::Pop } else { Phase::Suffix };
                }
                Phase::Suffix => {
                    if self.rect.a == self.rect.b || self.rect.c == self.rect.d {
                        self.phase = Phase::QueueSuffix;
                    } else if gate!(self.equal(pair, records, self.rect.b - 1, self.rect.d - 1, work, cancel)) {
                        self.rect.b -= 1;
                        self.rect.d -= 1;
                        self.suffix += 1;
                    } else {
                        self.phase = Phase::QueueSuffix;
                    }
                }
                Phase::QueueSuffix => {
                    if self.suffix > 0 {
                        gate!(self.push_task(
                            records,
                            Rect { a: self.rect.b, b: self.untrimmed.b, c: self.rect.d, d: self.untrimmed.d },
                            true,
                            work,
                            cancel
                        ));
                    }
                    self.phase = Phase::Residual;
                }
                Phase::Residual => {
                    if self.rect.n() == 0 || self.rect.m() == 0 {
                        gate!(work.debit(1, 0, cancel));
                        if self.rect.n() > 0 || self.rect.m() > 0 {
                            self.held = Some(Event::Changed(self.rect));
                        }
                        self.phase = Phase::Pop;
                    } else {
                        self.bisect = Some(attempt!(Bisect::new(self.rect)));
                        self.phase = Phase::Bisect;
                    }
                }
                Phase::Bisect => {
                    self.split = gate!(self.bisect.as_mut().unwrap().step(records, pair, self.layout, work, cancel));
                    self.bisect = None;
                    self.phase = Phase::QueueRight;
                }
                Phase::QueueRight => {
                    gate!(self.push_task(
                        records,
                        Rect { a: self.split.0, b: self.rect.b, c: self.split.1, d: self.rect.d },
                        false,
                        work,
                        cancel
                    ));
                    self.phase = Phase::QueueLeft;
                }
                Phase::QueueLeft => {
                    gate!(self.push_task(
                        records,
                        Rect { a: self.rect.a, b: self.split.0, c: self.rect.c, d: self.split.1 },
                        false,
                        work,
                        cancel
                    ));
                    self.phase = Phase::Pop;
                }
                Phase::Finish => return Step::Ready(None),
            }
        }
    }
}

#[cfg(test)]
#[path = "sequence_tests.rs"]
mod tests;
