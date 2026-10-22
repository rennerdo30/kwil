//! Bounded, drawing-independent side-by-side comparison. See the crate README for limits.
pub use imara_diff::Algorithm;
use std::{
    hash::{Hash, Hasher},
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// Sources must return stable bytes during comparison. Lines may include LF/CRLF;
/// both sources must use the same convention. Indexes and ranges are zero-based.
pub trait LineSource {
    fn line_count(&self) -> usize;
    fn line(&self, index: usize) -> &[u8];
}
impl<T: AsRef<[u8]>> LineSource for [T] {
    fn line_count(&self) -> usize {
        self.len()
    }
    fn line(&self, index: usize) -> &[u8] {
        self[index].as_ref()
    }
}
impl<T: LineSource + ?Sized> LineSource for &T {
    fn line_count(&self) -> usize {
        (**self).line_count()
    }
    fn line(&self, i: usize) -> &[u8] {
        (**self).line(i)
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Whitespace {
    #[default]
    Exact,
    All,
    Trailing,
    Amount,
}
pub const MAX_LINES: usize = 262_144;
pub const MAX_LINE_BYTES: usize = 1024 * 1024;
pub const MAX_SCANNED_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_WORD_LINE_BYTES: usize = 4096;
pub const MAX_WORD_HUNK_LINES: usize = 64;
#[derive(Clone, Debug)]
pub struct Options {
    pub algorithm: Algorithm,
    pub whitespace: Whitespace,
    pub ignore_line_endings: bool,
    /// May lower, but never raise, the fixed admission ceiling.
    pub max_lines: usize,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            algorithm: Algorithm::Histogram,
            whitespace: Whitespace::Exact,
            ignore_line_endings: false,
            max_lines: MAX_LINES,
        }
    }
}
#[derive(Clone, Default, Debug)]
pub struct CancellationToken(Arc<AtomicBool>);
impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    fn check(&self) -> Result<(), DiffError> {
        if self.0.load(Ordering::Relaxed) { Err(DiffError::Cancelled) } else { Ok(()) }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffError {
    TooLarge,
    Cancelled,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Equal,
    Insert,
    Delete,
    Modify,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub left: Option<usize>,
    pub right: Option<usize>,
    pub kind: Kind,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub left: Range<usize>,
    pub right: Range<usize>,
    pub rows: Range<usize>,
}
#[derive(Clone, Debug)]
struct Run {
    left: usize,
    right: usize,
    rows: Range<usize>,
    kind: Kind,
}
/// Equal regions occupy one run, never one object per line or character.
#[derive(Clone, Debug, Default)]
pub struct Comparison {
    runs: Vec<Run>,
    pub hunks: Vec<Hunk>,
    row_count: usize,
}
impl Comparison {
    pub fn row_count(&self) -> usize {
        self.row_count
    }
    pub fn row(&self, index: usize) -> Option<Row> {
        let r = self.runs.get(self.runs.partition_point(|r| r.rows.end <= index))?;
        let offset = index - r.rows.start;
        Some(Row {
            left: (r.kind != Kind::Insert).then_some(r.left + offset),
            right: (r.kind != Kind::Delete).then_some(r.right + offset),
            kind: r.kind,
        })
    }
    /// Strict navigation in uncollapsed row coordinates, without wrapping.
    pub fn next_hunk(&self, row: usize) -> Option<&Hunk> {
        self.hunks.get(self.hunks.partition_point(|h| h.rows.start <= row))
    }
    pub fn previous_hunk(&self, row: usize) -> Option<&Hunk> {
        self.hunks.partition_point(|h| h.rows.start < row).checked_sub(1).map(|i| &self.hunks[i])
    }
    fn push(&mut self, left: usize, right: usize, len: usize, kind: Kind) {
        if len != 0 {
            self.runs.push(Run { left, right, rows: self.row_count..self.row_count + len, kind });
            self.row_count += len;
        }
    }
    pub fn collapsed(&self, context: usize) -> Vec<DisplaySegment> {
        let mut out = Vec::new();
        let mut cursor = 0;
        let mut visible: Option<Range<usize>> = None;
        for h in &self.hunks {
            let range = h.rows.start.saturating_sub(context)..h.rows.end.saturating_add(context).min(self.row_count);
            if let Some(prev) = visible.as_mut() {
                if range.start <= prev.end {
                    prev.end = prev.end.max(range.end);
                    continue;
                }
                emit_segment(&mut out, &mut cursor, prev.clone());
            }
            visible = Some(range);
        }
        if let Some(range) = visible {
            emit_segment(&mut out, &mut cursor, range);
        }
        if cursor < self.row_count {
            out.push(DisplaySegment::Collapsed(cursor..self.row_count));
        }
        out
    }
    /// Refine a visible modified row on demand. Ranges address original bytes.
    /// Oversized hunks/lines and equal/one-sided rows return `None`.
    pub fn refine(
        &self,
        row: usize,
        left: &dyn LineSource,
        right: &dyn LineSource,
        cancel: &CancellationToken,
    ) -> Result<Option<WordRanges>, DiffError> {
        cancel.check()?;
        let index = self.hunks.partition_point(|h| h.rows.end <= row);
        let Some(hunk) = self.hunks.get(index).filter(|h| h.rows.contains(&row)) else {
            return Ok(None);
        };
        if hunk.left.len().max(hunk.right.len()) > MAX_WORD_HUNK_LINES {
            return Ok(None);
        }
        let Some(Row { left: Some(l), right: Some(r), kind: Kind::Modify }) = self.row(row) else {
            return Ok(None);
        };
        let (a, b) = (left.line(l), right.line(r));
        if a.len().max(b.len()) > MAX_WORD_LINE_BYTES {
            return Ok(None);
        }
        let diff = similar::TextDiff::configure().timeout(std::time::Duration::from_millis(10)).diff_words(a, b);
        let mut ranges = WordRanges::default();
        let (mut l, mut r) = (0, 0);
        for change in diff.iter_all_changes() {
            cancel.check()?;
            let len = change.value().len();
            match change.tag() {
                similar::ChangeTag::Equal => {
                    l += len;
                    r += len;
                }
                similar::ChangeTag::Delete => {
                    ranges.left.push(l..l + len);
                    l += len;
                }
                similar::ChangeTag::Insert => {
                    ranges.right.push(r..r + len);
                    r += len;
                }
            }
        }
        Ok(Some(ranges))
    }
}
fn emit_segment(out: &mut Vec<DisplaySegment>, cursor: &mut usize, range: Range<usize>) {
    if *cursor < range.start {
        out.push(DisplaySegment::Collapsed(*cursor..range.start));
    }
    *cursor = range.end;
    out.push(DisplaySegment::Rows(range));
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisplaySegment {
    Rows(Range<usize>),
    Collapsed(Range<usize>),
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WordRanges {
    pub left: Vec<Range<usize>>,
    pub right: Vec<Range<usize>>,
}
#[derive(Clone, Copy)]
struct Normalized<'a> {
    content: &'a [u8],
    ending: &'a [u8],
    whitespace: Whitespace,
}
impl Normalized<'_> {
    fn bytes(&self) -> impl Iterator<Item = u8> + '_ {
        let mut in_space = false;
        self.content
            .iter()
            .copied()
            .filter_map(move |b| {
                let space = matches!(b, b' ' | b'\t' | 0x0b | 0x0c);
                match self.whitespace {
                    Whitespace::All if space => None,
                    Whitespace::Amount if space => {
                        let emit = !in_space;
                        in_space = true;
                        emit.then_some(b' ')
                    }
                    _ => {
                        in_space = false;
                        Some(b)
                    }
                }
            })
            .chain(self.ending.iter().copied())
    }
}
impl PartialEq for Normalized<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.bytes().eq(other.bytes())
    }
}
impl Eq for Normalized<'_> {}
impl Hash for Normalized<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // All tokens in an interner have the same options and terminator convention.
        if matches!(self.whitespace, Whitespace::Exact | Whitespace::Trailing) {
            state.write(self.content);
            state.write(self.ending);
        } else {
            for byte in self.bytes() {
                state.write_u8(byte);
            }
        }
    }
}
fn normalized<'a>(bytes: &'a [u8], options: &Options) -> Normalized<'a> {
    let eol = if bytes.ends_with(b"\r\n") { 2 } else { usize::from(bytes.ends_with(b"\n")) };
    let (mut content, mut ending) = bytes.split_at(bytes.len() - eol);
    if options.ignore_line_endings && eol == 2 {
        ending = b"\n";
    }
    if options.whitespace == Whitespace::Trailing {
        let end = content.iter().rposition(|b| !matches!(b, b' ' | b'\t' | 0x0b | 0x0c)).map_or(0, |i| i + 1);
        content = &content[..end];
    }
    Normalized { content, ending, whitespace: options.whitespace }
}
/// Computes globally aligned hunks on admitted inputs, borrowing all line bytes.
pub fn compare(
    left: &dyn LineSource,
    right: &dyn LineSource,
    options: &Options,
    cancel: &CancellationToken,
) -> Result<Comparison, DiffError> {
    cancel.check()?;
    let (nl, nr) = (left.line_count(), right.line_count());
    if nl.checked_add(nr).is_none_or(|n| n > options.max_lines.min(MAX_LINES)) {
        return Err(DiffError::TooLarge);
    }
    let mut input = imara_diff::InternedInput {
        before: Vec::with_capacity(nl),
        after: Vec::with_capacity(nr),
        interner: imara_diff::Interner::new(nl + nr),
    };
    let mut bytes = 0usize;
    for (source, tokens) in [(left, &mut input.before), (right, &mut input.after)] {
        for i in 0..source.line_count() {
            cancel.check()?;
            let line = source.line(i);
            bytes = bytes.checked_add(line.len()).ok_or(DiffError::TooLarge)?;
            if line.len() > MAX_LINE_BYTES || bytes > MAX_SCANNED_BYTES {
                return Err(DiffError::TooLarge);
            }
            tokens.push(input.interner.intern(normalized(line, options)));
        }
    }
    cancel.check()?;
    let diff = imara_diff::Diff::compute(options.algorithm, &input);
    cancel.check()?;
    let mut model = Comparison::default();
    let (mut l, mut r) = (0, 0);
    for h in diff.hunks() {
        cancel.check()?;
        let (a, b) = (h.before.start as usize..h.before.end as usize, h.after.start as usize..h.after.end as usize);
        model.push(l, r, a.start - l, Kind::Equal);
        let start = model.row_count;
        let paired = a.len().min(b.len());
        model.push(a.start, b.start, paired, Kind::Modify);
        model.push(a.start + paired, b.start + paired, a.len() - paired, Kind::Delete);
        model.push(a.start + paired, b.start + paired, b.len() - paired, Kind::Insert);
        model.hunks.push(Hunk { left: a.clone(), right: b.clone(), rows: start..model.row_count });
        (l, r) = (a.end, b.end);
    }
    model.push(l, r, nl - l, Kind::Equal);
    Ok(model)
}
