use crate::search::invalid;
use crate::{Cancellation, Options};
use globset::{Glob, GlobSet, GlobSetBuilder};
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::{WalkBuilder, WalkState};
use std::io::{self, BufRead, BufReader, Read};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::Duration;

/// Leave at least half of the logical CPUs for editing, with at most 4 workers.
pub fn worker_count() -> usize {
    std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).clamp(1, 4))
}

#[derive(Clone, Debug)]
pub struct FileSearch {
    pub root: PathBuf,
    pub expression: String,
    pub literal: bool,
    pub options: Options,
    /// Root-relative glob syntax; exclusions win, and includes never override
    /// .gitignore. Hidden files retain ignore's default exclusion behavior.
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub workers: usize,
    pub max_line_bytes: usize,
    pub max_match_ranges: usize,
    /// grep-searcher's per-worker line buffer limit. Oversized lines produce an
    /// Error event, not a file-sized allocation. Affects find-in-files only.
    pub heap_limit: usize,
}
impl FileSearch {
    pub fn new(root: PathBuf, expression: String) -> Self {
        Self {
            root,
            expression,
            literal: false,
            options: Options::default(),
            include: Vec::new(),
            exclude: Vec::new(),
            workers: worker_count(),
            max_line_bytes: 512,
            max_match_ranges: 256,
            heap_limit: 2 << 20,
        }
    }
}

#[derive(Debug)]
pub struct FileHit {
    pub path: PathBuf,
    pub line_number: u64,
    pub line_offset: u64,
    /// Preview without LF or a CR belonging to CRLF, truncated to the byte cap.
    /// UTF-16 BOM files are decoded to UTF-8 here; other files retain raw bytes.
    pub line: Vec<u8>,
    pub line_truncated: bool,
    /// Original-file byte ranges relative to line_offset, including CR in CRLF
    /// and hits beyond the preview. LF separates records and cannot be matched.
    /// For UTF-16, a byte-mode regex splitting a decoded scalar covers that
    /// scalar's original code units; preview indices and these ranges differ.
    pub match_ranges: Vec<Range<usize>>,
    pub ranges_truncated: bool,
}
#[derive(Debug)]
pub enum FileEvent {
    Hit(FileHit),
    Error { path: PathBuf, message: String },
}

#[derive(Debug, Default)]
pub struct FileSearchSummary {
    pub files: u64,
    pub matching_lines: u64,
    pub errors: u64,
    pub cancelled: bool,
    pub workers: usize,
}

/// Run on a frontend-owned worker, consuming a bounded sync_channel. Results
/// preserve order within each file; different files may interleave. Dropping
/// the receiver stops all workers. Cancellation also stops a blocked producer
/// within a 5 ms backpressure retry, without a UI polling timer.
pub fn find_in_files(
    options: &FileSearch,
    cancel: &Cancellation,
    output: SyncSender<FileEvent>,
) -> io::Result<FileSearchSummary> {
    if options.workers == 0
        || options.max_line_bytes > 1 << 20
        || options.max_match_ranges == 0
        || options.max_match_ranges > 65536
        || !(1 << 16..=8 << 20).contains(&options.heap_limit)
    {
        return Err(invalid("invalid file-search worker or memory limits"));
    }
    let mut builder = RegexMatcherBuilder::new();
    builder
        .case_insensitive(options.options.case_insensitive)
        .word(options.options.whole_word)
        .fixed_strings(options.literal)
        .crlf(true)
        .line_terminator(Some(b'\n'));
    let matcher = builder.build(&options.expression).map_err(invalid)?;
    let include = globs(&options.include)?;
    let exclude = globs(&options.exclude)?;
    let workers = options.workers.min(worker_count());
    let files = AtomicU64::new(0);
    let hits = AtomicU64::new(0);
    let errors = AtomicU64::new(0);
    let disconnected = AtomicBool::new(false);
    let mut walk = WalkBuilder::new(&options.root);
    // Useful outside a repository too; .gitignore remains effective in temp
    // trees, while parent/global excludes still follow ignore's normal policy.
    walk.require_git(false).threads(workers).follow_links(false);
    walk.build_parallel().run(|| {
        let (files, hits, errors, disconnected, output, include, exclude) =
            (&files, &hits, &errors, &disconnected, &output, &include, &exclude);
        let matcher = matcher.clone();
        let mut searcher = SearcherBuilder::new()
            .line_number(true)
            .binary_detection(BinaryDetection::quit(b'\0'))
            .heap_limit(Some(options.heap_limit))
            .encoding(None)
            .bom_sniffing(false)
            .build();
        Box::new(move |entry| {
            if cancel.load(Ordering::Relaxed) || disconnected.load(Ordering::Relaxed) {
                return WalkState::Quit;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    errors.fetch_add(1, Ordering::Relaxed);
                    send(
                        output,
                        FileEvent::Error { path: options.root.clone(), message: e.to_string() },
                        cancel,
                        disconnected,
                    );
                    return WalkState::Continue;
                }
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            let relative = entry.path().strip_prefix(&options.root).unwrap_or(entry.path());
            if (!options.include.is_empty() && !include.is_match(relative)) || exclude.is_match(relative) {
                return WalkState::Continue;
            }
            files.fetch_add(1, Ordering::Relaxed);
            let mut sink =
                FileSink { matcher: &matcher, path: entry.path(), options, output, cancel, disconnected, hits };
            let result = std::fs::File::open(entry.path()).and_then(|file| {
                let mut reader = BufReader::new(CancelReader { file, cancel, disconnected });
                let prefix = reader.fill_buf()?;
                let little = match prefix.get(..2) {
                    Some([0xff, 0xfe]) => Some(true),
                    Some([0xfe, 0xff]) => Some(false),
                    _ => None,
                };
                if let Some(little) = little {
                    reader.consume(2);
                    search_utf16(reader, little, &mut sink)
                } else {
                    searcher.search_reader(&matcher, reader, &mut sink)
                }
            });
            if let Err(e) = result {
                if cancel.load(Ordering::Relaxed) || disconnected.load(Ordering::Relaxed) {
                    return WalkState::Quit;
                }
                errors.fetch_add(1, Ordering::Relaxed);
                send(
                    output,
                    FileEvent::Error { path: entry.path().to_owned(), message: e.to_string() },
                    cancel,
                    disconnected,
                );
            }
            WalkState::Continue
        })
    });
    Ok(FileSearchSummary {
        files: files.load(Ordering::Relaxed),
        matching_lines: hits.load(Ordering::Relaxed),
        errors: errors.load(Ordering::Relaxed),
        cancelled: cancel.load(Ordering::Relaxed) || disconnected.load(Ordering::Relaxed),
        workers,
    })
}

struct CancelReader<'a> {
    file: std::fs::File,
    cancel: &'a Cancellation,
    disconnected: &'a AtomicBool,
}
impl Read for CancelReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) || self.disconnected.load(Ordering::Relaxed) {
            return Err(io::Error::other("search cancelled"));
        }
        self.file.read(buf)
    }
}

fn globs(patterns: &[String]) -> io::Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).map_err(invalid)?);
    }
    builder.build().map_err(invalid)
}

fn send(
    output: &SyncSender<FileEvent>,
    mut event: FileEvent,
    cancel: &Cancellation,
    disconnected: &AtomicBool,
) -> bool {
    loop {
        if cancel.load(Ordering::Relaxed) || disconnected.load(Ordering::Relaxed) {
            return false;
        }
        match output.try_send(event) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => {
                disconnected.store(true, Ordering::Relaxed);
                return false;
            }
            Err(TrySendError::Full(e)) => {
                event = e;
                std::thread::park_timeout(Duration::from_millis(5));
            }
        }
    }
}

struct FileSink<'a> {
    matcher: &'a RegexMatcher,
    path: &'a Path,
    options: &'a FileSearch,
    output: &'a SyncSender<FileEvent>,
    cancel: &'a Cancellation,
    disconnected: &'a AtomicBool,
    hits: &'a AtomicU64,
}
impl FileSink<'_> {
    fn line(&mut self, bytes: &[u8], line_number: u64, line_offset: u64, utf16: bool) -> io::Result<bool> {
        // Match against the original record, preserving its CR. Only the preview
        // loses CRLF. A standalone CR in an unterminated record stays visible.
        let preview = bytes.strip_suffix(b"\n").map_or(bytes, |line| line.strip_suffix(b"\r").unwrap_or(line));
        let mut ranges = Vec::new();
        let mut ranges_truncated = false;
        let text = if utf16 { std::str::from_utf8(bytes).map_err(invalid)? } else { "" };
        let mut offsets = Utf16Offsets { text, decoded: 0, original: 0 };
        self.matcher
            .find_iter(bytes, |m| {
                if ranges.len() == self.options.max_match_ranges {
                    ranges_truncated = true;
                    return false;
                }
                let range =
                    if utf16 { offsets.at(m.start(), false)..offsets.at(m.end(), true) } else { m.start()..m.end() };
                ranges.push(range);
                true
            })
            .map_err(invalid)?;
        if ranges.is_empty() {
            return Ok(true);
        }
        let hit = FileHit {
            path: self.path.to_owned(),
            line_number,
            line_offset,
            line: preview[..preview.len().min(self.options.max_line_bytes)].to_vec(),
            line_truncated: preview.len() > self.options.max_line_bytes,
            match_ranges: ranges,
            ranges_truncated,
        };
        if send(self.output, FileEvent::Hit(hit), self.cancel, self.disconnected) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

// Walk each decoded line once while translating ordered match boundaries back
// to original UTF-16 code units. No file-sized offset map is retained.
struct Utf16Offsets<'a> {
    text: &'a str,
    decoded: usize,
    original: usize,
}
impl Utf16Offsets<'_> {
    fn at(&mut self, offset: usize, round_up: bool) -> usize {
        while self.decoded < offset {
            // UTF-16 decoding below always produces valid UTF-8, including a
            // replacement scalar for each unpaired surrogate.
            let c = self.text[self.decoded..].chars().next().unwrap();
            let next = self.decoded + c.len_utf8();
            if next > offset {
                return self.original + if round_up { c.len_utf16() * 2 } else { 0 };
            }
            self.decoded = next;
            self.original += c.len_utf16() * 2;
        }
        self.original
    }
}

fn search_utf16(mut reader: impl Read, little: bool, sink: &mut FileSink<'_>) -> io::Result<()> {
    // Keep raw units and decoded bytes under the same per-worker heap budget.
    // Worst case is 2 bytes/unit plus 3 UTF-8 bytes/unit. Fixed-capacity buffers
    // avoid geometric Vec growth beyond that budget. A long line is an error.
    let cap = sink.options.heap_limit / 8;
    let mut units = Vec::with_capacity(cap);
    let mut decoded = String::with_capacity(cap * 3);
    let mut offset = 2;
    let mut line_number = 1;
    loop {
        if sink.cancel.load(Ordering::Relaxed) || sink.disconnected.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut raw = [0; 2];
        let n = reader.read(&mut raw[..1])?;
        if n == 0 && units.is_empty() {
            return Ok(());
        }
        let eof = n == 0;
        if !eof {
            reader.read_exact(&mut raw[1..])?;
            if units.len() == cap {
                return Err(io::Error::other("UTF-16 line exceeds file-search heap limit"));
            }
            units.push(if little { u16::from_le_bytes(raw) } else { u16::from_be_bytes(raw) });
        }
        if eof || units.last() == Some(&10) {
            decoded.clear();
            for scalar in char::decode_utf16(units.iter().copied()) {
                decoded.push(scalar.unwrap_or(char::REPLACEMENT_CHARACTER));
            }
            if !sink.line(decoded.as_bytes(), line_number, offset, true)? {
                return Ok(());
            }
            offset += units.len() as u64 * 2;
            line_number += 1;
            units.clear();
            if eof {
                return Ok(());
            }
        }
    }
}

impl Sink for FileSink<'_> {
    type Error = io::Error;
    fn matched(&mut self, _: &Searcher, matched: &SinkMatch<'_>) -> io::Result<bool> {
        if self.cancel.load(Ordering::Relaxed) || self.disconnected.load(Ordering::Relaxed) {
            return Ok(false);
        }
        self.line(matched.bytes(), matched.line_number().unwrap_or(1), matched.absolute_byte_offset(), false)
    }
    fn context(&mut self, _: &Searcher, _: &grep_searcher::SinkContext<'_>) -> io::Result<bool> {
        Ok(!self.cancel.load(Ordering::Relaxed) && !self.disconnected.load(Ordering::Relaxed))
    }
}
