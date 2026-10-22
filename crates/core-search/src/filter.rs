use crate::search::{copy_range, invalid};
use crate::{Cancellation, Haystack, Pattern, SearchMode, search};
use std::io;
use std::ops::Range;
use std::sync::atomic::Ordering;

/// Indices refer to the patterns supplied to LineFilter::new.
#[derive(Clone, Debug)]
pub enum FilterExpr {
    Pattern(usize),
    And(Vec<FilterExpr>),
    Or(Vec<FilterExpr>),
    Not(Box<FilterExpr>),
}

impl FilterExpr {
    fn valid(&self, n: usize) -> bool {
        match self {
            Self::Pattern(i) => *i < n,
            Self::And(v) | Self::Or(v) => !v.is_empty() && v.iter().all(|e| e.valid(n)),
            Self::Not(e) => e.valid(n),
        }
    }
    fn eval(&self, values: &[bool]) -> bool {
        match self {
            Self::Pattern(i) => values[*i],
            Self::And(v) => v.iter().all(|e| e.eval(values)),
            Self::Or(v) => v.iter().any(|e| e.eval(values)),
            Self::Not(e) => !e.eval(values),
        }
    }
}

/// One-based line numbers in a half-open run. No line text is retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineRun {
    pub start: u64,
    pub end: u64,
}

/// Optional bounded collector. Streaming consumers can spool runs instead.
#[derive(Debug)]
pub struct LineRuns {
    runs: Vec<LineRun>,
    max_runs: usize,
}
impl LineRuns {
    pub fn new(max_runs: usize) -> Self {
        Self { runs: Vec::new(), max_runs }
    }
    pub fn runs(&self) -> &[LineRun] {
        &self.runs
    }
    pub fn push(&mut self, run: LineRun) -> io::Result<()> {
        if run.start == 0 || run.start >= run.end || self.runs.last().is_some_and(|r| r.end > run.start) {
            return Err(invalid("invalid line run order"));
        }
        if let Some(last) = self.runs.last_mut().filter(|last| last.end == run.start) {
            last.end = run.end;
            return Ok(());
        }
        if self.runs.len() == self.max_runs {
            return Err(io::Error::other("line run capacity reached; spool results"));
        }
        self.runs.push(run);
        Ok(())
    }
}

/// Resume only at a line start. A final partial line is rescanned on append;
/// complete preceding lines are never scanned again. Rotation/truncation must
/// reset this cursor (the watcher owns source identity).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FilterResume {
    pub offset: u64,
    pub line: u64,
}
impl Default for FilterResume {
    fn default() -> Self {
        Self { offset: 0, line: 1 }
    }
}

#[derive(Debug)]
pub struct FilterSummary {
    pub resume: FilterResume,
    pub matching_lines: u64,
    pub cancelled: bool,
}

pub struct LineFilter {
    patterns: Vec<Pattern>,
    expression: FilterExpr,
}
impl LineFilter {
    pub fn new(patterns: Vec<Pattern>, expression: FilterExpr) -> io::Result<Self> {
        if !expression.valid(patterns.len()) {
            return Err(invalid("invalid filter expression or pattern index"));
        }
        Ok(Self { patterns, expression })
    }

    /// Streams compact runs. For follow mode set include_unterminated=false:
    /// the resume offset then stays at the start of the incomplete final line.
    /// For a static view true includes the final line, but the caller must
    /// replace that provisional line's result if further bytes are appended.
    /// Regex match bounds are the same as Pattern::regex; lines themselves may
    /// be arbitrarily long and are never assembled into a line-sized buffer.
    pub fn scan<H: Haystack + ?Sized>(
        &self,
        source: &H,
        start: FilterResume,
        include_unterminated: bool,
        cancel: &Cancellation,
        mut emit: impl FnMut(LineRun) -> io::Result<()>,
    ) -> io::Result<FilterSummary> {
        if start.offset > source.len() || start.line == 0 {
            return Err(invalid("invalid filter resume cursor"));
        }
        if start.offset > 0 {
            let mut previous = Vec::with_capacity(1);
            copy_range(source, start.offset - 1..start.offset, &mut previous)?;
            if previous != b"\n" {
                return Err(invalid("filter must resume at a line start"));
            }
        }
        let mut cursor = start;
        let mut pending: Option<LineRun> = None;
        let mut matching_lines = 0;
        let mut values = vec![false; self.patterns.len()];
        let mut process = |end: u64, next: u64| -> io::Result<bool> {
            if next == end && cursor.offset == end {
                return Ok(true);
            }
            if cancel.load(Ordering::Relaxed) {
                return Ok(false);
            }
            let mut line_end = end;
            if end > cursor.offset {
                let mut last = Vec::with_capacity(1);
                copy_range(source, end - 1..end, &mut last)?;
                if last == b"\r" {
                    line_end -= 1;
                }
            }
            let line = LineSource { source, range: cursor.offset..line_end };
            for (i, pattern) in self.patterns.iter().enumerate() {
                values[i] =
                    search(&line, pattern, 0..line.len(), SearchMode::Count, cancel, |_| {}, |_| {})?.count != 0;
                if cancel.load(Ordering::Relaxed) {
                    return Ok(false);
                }
            }
            if self.expression.eval(&values) {
                matching_lines += 1;
                match &mut pending {
                    Some(run) if run.end == cursor.line => run.end += 1,
                    _ => {
                        if let Some(run) = pending.take() {
                            emit(run)?;
                        }
                        pending = Some(LineRun { start: cursor.line, end: cursor.line + 1 });
                    }
                }
            } else if let Some(run) = pending.take() {
                emit(run)?;
            }
            // An unterminated static line is provisional: keep the cursor at
            // its start so the watcher can replace that line's result on append.
            if next != end {
                cursor = FilterResume { offset: next, line: cursor.line + 1 };
            }
            Ok(true)
        };
        let mut expected = start.offset;
        'chunks: for (at, bytes) in source.chunks(start.offset..source.len()) {
            if at != expected || bytes.len() as u64 > source.len() - expected {
                return Err(invalid("invalid haystack chunks"));
            }
            expected += bytes.len() as u64;
            for n in memchr::memchr_iter(b'\n', bytes) {
                if !process(at + n as u64, at + n as u64 + 1)? {
                    break 'chunks;
                }
            }
            if cancel.load(Ordering::Relaxed) {
                break;
            }
        }
        if !cancel.load(Ordering::Relaxed) {
            if expected != source.len() {
                return Err(invalid("incomplete haystack chunks"));
            }
            if include_unterminated {
                process(source.len(), source.len())?;
            }
        }
        if let Some(run) = pending {
            emit(run)?;
        }
        Ok(FilterSummary { resume: cursor, matching_lines, cancelled: cancel.load(Ordering::Relaxed) })
    }
}

struct LineSource<'a, H: ?Sized> {
    source: &'a H,
    range: Range<u64>,
}
impl<H: Haystack + ?Sized> Haystack for LineSource<'_, H> {
    fn len(&self) -> u64 {
        self.range.end - self.range.start
    }
    fn chunks(&self, range: Range<u64>) -> impl Iterator<Item = (u64, &[u8])> {
        self.source
            .chunks(self.range.start + range.start..self.range.start + range.end)
            .map(|(at, b)| (at - self.range.start, b))
    }
}
