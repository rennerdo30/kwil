//! Bounded encoding detection, viewport decode and byte-preserving streaming saves.
//!
//! All positions are **original byte offsets**, including the BOM. Added pieces
//! contain UTF-8. Never use the byte-oriented `Snapshot` line metrics for UTF-16:
//! `DecodeIndex` counts LF/CR code units (and CRLF as one break), with line rank
//! and the current line start at decoder checkpoints every 64 KiB. Build it on a worker and wake
//! the frontend with `index_in_background`'s callback. No UI timer is needed.
//!
//! A viewport has an explicit 1 MiB source-byte cap, including alignment. Larger
//! work uses the streaming save/transform APIs. Index memory is sparse; neither
//! indexing nor saving retains decoded text. TextFile's mapped-read contract,
//! including its external-truncation caveat, also applies here.
//!
//! V1 supports all stateless encoding_rs encodings. ISO-2022-JP returns an
//! explicit `UnsupportedEncoding`: its shifted state needs a future checkpoint
//! representation (known limitation KW-45). UTF-32 is also explicitly unsupported. Neither case is
//! silently reinterpreted or replayed from the beginning of a file.

use crate::{IndexProgress, ReadError, Snapshot, SourceChunk, TextFile};
use encoding_rs::{CoderResult, Decoder, DecoderResult, EncoderResult};
use std::borrow::Cow;
use std::io::{self, Write};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

#[cfg(test)]
#[path = "encoding_tests.rs"]
mod tests;

#[path = "encoded_snapshot.rs"]
mod snapshot_save;
pub use snapshot_save::{
    validate_snapshot_save, validate_snapshot_save_cancellable, write_snapshot, write_snapshot_cancellable,
};

const SAMPLE: usize = 64 << 10;
const STRIDE: usize = 64 << 10;
const SCRATCH: usize = 16 << 10;
pub const MAX_VIEWPORT_BYTES: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Encoding(&'static encoding_rs::Encoding);

impl Encoding {
    pub const UTF8: Self = Self(encoding_rs::UTF_8);
    pub const UTF16_LE: Self = Self(encoding_rs::UTF_16LE);
    pub const UTF16_BE: Self = Self(encoding_rs::UTF_16BE);

    /// WHATWG labels, including Windows-1252, Shift_JIS, EUC-JP, GB18030,
    /// GBK, Big5 and KOI8-R. Stateful ISO-2022-JP requires a different checkpoint
    /// representation and is explicitly refused rather than replaying a file.
    pub fn for_label(label: &str) -> Result<Self, EncodingError> {
        let encoding = encoding_rs::Encoding::for_label_no_replacement(label.as_bytes())
            .ok_or_else(|| EncodingError::UnsupportedEncoding(label.to_owned()))?;
        Self::checked(encoding)
    }

    fn checked(encoding: &'static encoding_rs::Encoding) -> Result<Self, EncodingError> {
        if encoding == encoding_rs::ISO_2022_JP || encoding == encoding_rs::REPLACEMENT {
            Err(EncodingError::UnsupportedEncoding(encoding.name().to_owned()))
        } else {
            Ok(Self(encoding))
        }
    }

    pub fn name(self) -> &'static str {
        self.0.name()
    }

    fn utf16(self) -> bool {
        self == Self::UTF16_LE || self == Self::UTF16_BE
    }

    fn bom(self) -> &'static [u8] {
        if self == Self::UTF8 {
            b"\xef\xbb\xbf"
        } else if self == Self::UTF16_LE {
            b"\xff\xfe"
        } else if self == Self::UTF16_BE {
            b"\xfe\xff"
        } else {
            b""
        }
    }

    fn unit(self, bytes: &[u8], at: usize) -> Option<u16> {
        let pair = bytes.get(at..at + 2)?;
        Some(if self == Self::UTF16_LE {
            u16::from_le_bytes([pair[0], pair[1]])
        } else {
            u16::from_be_bytes([pair[0], pair[1]])
        })
    }
}

#[derive(Debug)]
pub enum EncodingError {
    Read(ReadError),
    Io(io::Error),
    UnsupportedEncoding(String),
    InvalidRange,
    ViewportTooLarge,
    IndexNotReady,
    WrongSource,
    SplitCharacter(u64),
    InvalidUtf8,
    MalformedSource,
    Cancelled,
    Unmappable { character: char, encoding: &'static str },
}

impl std::fmt::Display for EncodingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(e) => e.fmt(f),
            Self::Io(e) => e.fmt(f),
            Self::UnsupportedEncoding(name) if name == "ISO-2022-JP" => {
                f.write_str("ISO-2022-JP is not supported yet (known limitation, KW-45)")
            }
            Self::UnsupportedEncoding(name) => write!(f, "unsupported encoding: {name}"),
            Self::InvalidRange => f.write_str("byte range outside source"),
            Self::ViewportTooLarge => f.write_str("viewport exceeds 1 MiB; request a column range or stream"),
            Self::IndexNotReady => f.write_str("encoding index is not ready"),
            Self::WrongSource => f.write_str("snapshot contains a different original file"),
            Self::SplitCharacter(at) => write!(f, "edit splits an encoded character at byte {at}"),
            Self::InvalidUtf8 => f.write_str("added text is not valid UTF-8"),
            Self::MalformedSource => {
                f.write_str("conversion contains malformed source bytes; reinterpret or repair first")
            }
            Self::Cancelled => f.write_str("snapshot save cancelled"),
            Self::Unmappable { character, encoding } => write!(f, "{character:?} cannot be saved as {encoding}"),
        }
    }
}

impl std::error::Error for EncodingError {}
impl From<ReadError> for EncodingError {
    fn from(e: ReadError) -> Self {
        Self::Read(e)
    }
}
impl From<io::Error> for EncodingError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    CrLf,
    Cr,
}
impl LineEnding {
    pub fn text(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
            Self::Cr => "\r",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndingStyle {
    None,
    Uniform(LineEnding),
    Mixed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EndingCounts {
    pub lf: u64,
    pub crlf: u64,
    pub cr: u64,
}
impl EndingCounts {
    pub fn style(self) -> EndingStyle {
        match (self.lf > 0, self.crlf > 0, self.cr > 0) {
            (false, false, false) => EndingStyle::None,
            (true, false, false) => EndingStyle::Uniform(LineEnding::Lf),
            (false, true, false) => EndingStyle::Uniform(LineEnding::CrLf),
            (false, false, true) => EndingStyle::Uniform(LineEnding::Cr),
            _ => EndingStyle::Mixed,
        }
    }

    fn record(&mut self, ending: LineEnding) {
        match ending {
            LineEnding::Lf => self.lf += 1,
            LineEnding::CrLf => self.crlf += 1,
            LineEnding::Cr => self.cr += 1,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Detection {
    pub encoding: Encoding,
    pub bom_len: usize,
    pub endings: EndingCounts,
    /// This describes the samples only, never validation of the whole file.
    pub sample_is_utf8: bool,
    pub sampled_bytes: usize,
}

/// Only first/last <=64 KiB are examined. Detection is a heuristic; reinterpret
/// is the authoritative user override. Incomplete UTF-8 at artificial sample
/// edges is excluded from validation, not mistaken for a legacy encoding.
pub fn detect(file: &TextFile) -> Result<Detection, EncodingError> {
    detect_bytes(file.bytes()?, None)
}

fn samples(bytes: &[u8]) -> [Range<usize>; 2] {
    let end = bytes.len().min(SAMPLE);
    [0..end, end.max(bytes.len().saturating_sub(SAMPLE))..bytes.len()]
}

fn sample_utf8(bytes: &[u8], range: Range<usize>) -> bool {
    let mut start = range.start;
    let mut end = range.end;
    if start > 0 {
        while start < end && start < range.start + 3 && bytes[start] & 0xc0 == 0x80 {
            start += 1;
        }
    }
    if end < bytes.len() {
        let tail = &bytes[start..end];
        if let Err(e) = std::str::from_utf8(tail)
            && e.error_len().is_none()
        {
            end = start + e.valid_up_to();
        }
    }
    simdutf8::basic::from_utf8(&bytes[start..end]).is_ok()
}

fn detect_bytes(bytes: &[u8], chosen: Option<Encoding>) -> Result<Detection, EncodingError> {
    if chosen.is_none() && (bytes.starts_with(b"\xff\xfe\0\0") || bytes.starts_with(b"\0\0\xfe\xff")) {
        return Err(EncodingError::UnsupportedEncoding("UTF-32".to_owned()));
    }
    let ranges = samples(bytes);
    let sample_is_utf8 = ranges.iter().all(|r| sample_utf8(bytes, r.clone()));
    let bom = encoding_rs::Encoding::for_bom(bytes);
    let encoding = if let Some(chosen) = chosen {
        chosen
    } else if let Some((encoding, _)) = bom {
        Encoding::checked(encoding)?
    } else if looks_utf16(bytes, &ranges, true) {
        Encoding::UTF16_LE
    } else if looks_utf16(bytes, &ranges, false) {
        Encoding::UTF16_BE
    } else if sample_is_utf8 && !ranges.iter().any(|range| memchr::memchr(0x1b, &bytes[range.clone()]).is_some()) {
        Encoding::UTF8
    } else {
        let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
        // Samples are discontiguous: do not let a pending multibyte lead byte
        // in the first sample consume an unrelated byte in the last sample.
        for range in &ranges {
            detector.feed(&bytes[range.clone()], false);
            if range.end < bytes.len() {
                detector.feed(b"\n", false);
            }
        }
        detector.feed(b"", true);
        Encoding::checked(detector.guess(
            None,
            if sample_is_utf8 { chardetng::Utf8Detection::Allow } else { chardetng::Utf8Detection::Deny },
        ))?
    };
    let bom_len = bom.filter(|(e, _)| *e == encoding.0).map_or(0, |(_, n)| n);
    let mut endings = EndingCounts::default();
    for range in &ranges {
        let width = if encoding.utf16() { 2 } else { 1 };
        let mut start = range.start.max(bom_len);
        if encoding.utf16() {
            start += start % 2;
        }
        let mut end = range.end;
        if encoding.utf16() {
            end -= end % 2;
        }
        if start >= end {
            continue;
        }
        // Artificial sample edges are not real line endings. Do not peek
        // outside either bounded sample to resolve a clipped CRLF.
        let unit = |at| if encoding.utf16() { encoding.unit(bytes, at) } else { bytes.get(at).map(|&b| b as u16) };
        if range.start > 0 && unit(start) == Some(10) {
            start += width;
        }
        if range.end < bytes.len() && unit(end - width) == Some(13) {
            end -= width;
        }
        if start < end {
            let sample = &bytes[start..end];
            for_breaks(sample, 0, sample.len(), encoding, |_, ending| endings.record(ending));
        }
    }
    Ok(Detection { encoding, bom_len, endings, sample_is_utf8, sampled_bytes: ranges.iter().map(|r| r.len()).sum() })
}

// Same alternating-NUL/printability criteria as TextFile::kind. Recognition
// lives here too because FileKind intentionally does not expose an encoding.
fn looks_utf16(bytes: &[u8], ranges: &[Range<usize>; 2], le: bool) -> bool {
    if !bytes.len().is_multiple_of(2) {
        return false;
    }
    let (mut total, mut high_nuls, mut low_nuls, mut printable) = (0, 0, 0, 0);
    for range in ranges {
        let start = range.start + range.start % 2;
        for pair in bytes[start..range.end.max(start)].as_chunks::<2>().0 {
            let (low, high) = if le { (pair[0], pair[1]) } else { (pair[1], pair[0]) };
            total += 1;
            high_nuls += usize::from(high == 0);
            low_nuls += usize::from(low == 0);
            let unit = u16::from_le_bytes([low, high]);
            printable += usize::from(char::from_u32(unit as u32).is_some_and(|c| !c.is_control() || c.is_whitespace()));
        }
    }
    total >= 4 && high_nuls * 2 >= total && low_nuls * 10 <= total && printable * 10 >= total * 9
}

/// Reports breaks whose *first* code unit is inside start..end. CRLF may end
/// just outside the range. LF belonging to an earlier CR is not counted twice.
fn for_breaks(bytes: &[u8], start: usize, end: usize, encoding: Encoding, mut emit: impl FnMut(usize, LineEnding)) {
    if start >= end {
        return;
    }
    let width = if encoding.utf16() { 2 } else { 1 };
    let unit = |at: usize| if encoding.utf16() { encoding.unit(bytes, at) } else { bytes.get(at).map(|&b| b as u16) };
    let visit = |at: usize, emit: &mut dyn FnMut(usize, LineEnding)| match unit(at) {
        Some(13) => {
            if unit(at + width) == Some(10) {
                emit(at + width * 2, LineEnding::CrLf);
            } else {
                emit(at + width, LineEnding::Cr);
            }
        }
        Some(10) if at < width || unit(at - width) != Some(13) => emit(at + width, LineEnding::Lf),
        _ => {}
    };
    if encoding.utf16() {
        for at in ((start + start % 2)..end).step_by(2) {
            visit(at, &mut emit);
        }
    } else {
        for at in memchr::memchr2_iter(b'\r', b'\n', &bytes[start..end]) {
            visit(start + at, &mut emit);
        }
    }
}

/// UTF-8 text is borrowed directly. Invalid runs retain their raw bytes;
/// frontends render each byte as an escape marker (for example `\\xFF`).
#[derive(Debug, PartialEq, Eq)]
pub enum DisplaySegment<'a> {
    Text(Cow<'a, str>),
    Invalid(&'a [u8]),
}

pub fn utf8_segments<'a>(mut bytes: &'a [u8], mut emit: impl FnMut(DisplaySegment<'a>)) {
    while !bytes.is_empty() {
        match simdutf8::compat::from_utf8(bytes) {
            Ok(text) => {
                emit(DisplaySegment::Text(Cow::Borrowed(text)));
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid > 0 {
                    emit(DisplaySegment::Text(Cow::Borrowed(std::str::from_utf8(&bytes[..valid]).unwrap())));
                }
                let invalid = error.error_len().unwrap_or(bytes.len() - valid);
                emit(DisplaySegment::Invalid(&bytes[valid..valid + invalid]));
                bytes = &bytes[valid + invalid..];
            }
        }
    }
}

#[derive(Debug)]
pub struct DecodedRange<'a> {
    /// Expanded by at most a character on either edge; never splits a scalar.
    pub source: Range<u64>,
    pub segments: Vec<DisplaySegment<'a>>,
    pub had_errors: bool,
}

#[derive(Debug)]
pub struct DecodeIndex {
    /// Known neutral decoder states at original source byte offsets.
    checkpoints: Vec<Checkpoint>,
    line_count: u64,
}

#[derive(Debug)]
struct Checkpoint {
    offset: u64,
    line: u64,
    line_start: u64,
}

impl DecodeIndex {
    pub fn checkpoint_count(&self) -> usize {
        self.checkpoints.len()
    }
    pub fn line_count(&self) -> u64 {
        self.line_count
    }
    pub fn allocated_bytes(&self) -> usize {
        self.checkpoints.capacity() * std::mem::size_of::<Checkpoint>()
    }
    fn before(&self, at: u64) -> usize {
        self.checkpoints[self.checkpoints.partition_point(|p| p.offset <= at).saturating_sub(1)].offset as usize
    }
}

struct SharedIndex {
    ready: OnceLock<DecodeIndex>,
    scanned: AtomicU64,
}

#[derive(Clone)]
pub struct EncodedFile {
    file: TextFile,
    detection: Detection,
    index: Arc<SharedIndex>,
}

impl EncodedFile {
    pub fn new(file: TextFile) -> Result<Self, EncodingError> {
        let detection = detect(&file)?;
        Ok(Self::with_detection(file, detection))
    }
    /// Explicit interpretation, including recovery when automatic detection
    /// refuses an ambiguous BOM or an unsupported detected encoding.
    pub fn new_with_encoding(file: TextFile, encoding: Encoding) -> Result<Self, EncodingError> {
        let detection = detect_bytes(file.bytes()?, Some(encoding))?;
        Ok(Self::with_detection(file, detection))
    }
    fn with_detection(file: TextFile, detection: Detection) -> Self {
        Self { file, detection, index: Arc::new(SharedIndex { ready: OnceLock::new(), scanned: AtomicU64::new(0) }) }
    }
    pub fn detection(&self) -> Detection {
        self.detection
    }
    pub fn file(&self) -> &TextFile {
        &self.file
    }
    /// Reinterpret original bytes, without editing them. Indexes are tied to
    /// the interpretation; previous clones keep their own interpretation.
    pub fn reinterpret(&self, encoding: Encoding) -> Result<Self, EncodingError> {
        Ok(Self::with_detection(self.file.clone(), detect_bytes(self.file.bytes()?, Some(encoding))?))
    }
    pub fn index(&self) -> Option<&DecodeIndex> {
        self.index.ready.get()
    }
    pub fn progress(&self) -> IndexProgress {
        IndexProgress { scanned: self.index.scanned.load(Ordering::Relaxed), total: self.file.len() }
    }

    pub fn index_in_background(
        &self,
        done: impl FnOnce(Result<(), EncodingError>) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let this = self.clone();
        std::thread::spawn(move || done(this.build_index().map(|_| ())))
    }

    /// Worker entry point. The scratch buffer is 16 KiB, independent of input.
    pub fn build_index(&self) -> Result<&DecodeIndex, EncodingError> {
        let bytes = self.file.bytes()?;
        if let Some(index) = self.index() {
            return Ok(index);
        }
        let encoding = self.detection.encoding;
        let mut decoder = encoding.0.new_decoder_without_bom_handling();
        let mut checkpoints = vec![Checkpoint {
            offset: self.detection.bom_len as u64,
            line: 0,
            line_start: self.detection.bom_len as u64,
        }];
        let mut line_count = 1;
        let mut line_start = self.detection.bom_len as u64;
        let mut at = self.detection.bom_len;
        while at < bytes.len() {
            // Recheck once per region before touching mapping pages.
            let bytes = self.file.bytes()?;
            let mut end = (at + STRIDE).min(bytes.len());
            if encoding.utf16() {
                end = utf16_boundary(bytes, end, encoding);
            } else {
                discard(&mut decoder, &bytes[at..end]);
                // At most 3 continuation bytes for supported encodings. EOF
                // pending/malformed bytes need no following checkpoint.
                while end < bytes.len() && decoder.latin1_byte_compatible_up_to(b"").is_none() {
                    discard(&mut decoder, &bytes[end..end + 1]);
                    end += 1;
                }
            }
            // Keep CRLF together in both the line and byte checkpoints.
            let width = if encoding.utf16() { 2 } else { 1 };
            let cr = if encoding.utf16() {
                encoding.unit(bytes, end.saturating_sub(width)) == Some(13)
            } else {
                end > 0 && bytes[end - 1] == b'\r'
            };
            if cr && end + width <= bytes.len() {
                let lf = if encoding.utf16() { encoding.unit(bytes, end) == Some(10) } else { bytes[end] == b'\n' };
                if lf {
                    if !encoding.utf16() {
                        discard(&mut decoder, &bytes[end..end + width]);
                    }
                    end += width;
                }
            }
            for_breaks(bytes, at, end, encoding, |offset, _| {
                line_count += 1;
                line_start = offset as u64;
            });
            at = end;
            if at < bytes.len() {
                checkpoints.push(Checkpoint { offset: at as u64, line: line_count - 1, line_start });
            }
            self.index.scanned.store(at as u64, Ordering::Relaxed);
        }
        self.index.scanned.store(self.file.len(), Ordering::Relaxed);
        let _ = self.index.ready.set(DecodeIndex { checkpoints, line_count });
        Ok(self.index().unwrap())
    }

    pub fn line_start(&self, line: u64) -> Result<Option<u64>, EncodingError> {
        let bytes = self.file.bytes()?;
        let index = self.index().ok_or(EncodingError::IndexNotReady)?;
        if line >= index.line_count {
            return Ok(None);
        }
        let i = index.checkpoints.partition_point(|p| p.line <= line) - 1;
        let checkpoint = &index.checkpoints[i];
        let mut current = checkpoint.line;
        if current == line {
            return Ok(Some(checkpoint.line_start));
        }
        let mut found = None;
        // Rank checkpoints bound lookup to one 64 KiB region even when a
        // preceding line is gigabytes long;
        // no strings or per-line allocations even for a giant line.
        let mut start = checkpoint.offset as usize;
        while start < bytes.len() && found.is_none() {
            let bytes = self.file.bytes()?;
            let end = (start + STRIDE).min(bytes.len());
            for_breaks(bytes, start, end, self.detection.encoding, |at, _| {
                current += 1;
                if current == line {
                    found = Some(at as u64);
                }
            });
            start = end;
        }
        Ok(found)
    }

    /// Computes a capped viewport range from encoding-aware line starts.
    /// For giant lines use decode_range with the visible column byte range.
    pub fn decode_lines(&self, line: u64, count: usize) -> Result<Option<DecodedRange<'_>>, EncodingError> {
        let Some(start) = self.line_start(line)? else { return Ok(None) };
        if count == 0 {
            return self.decode_range(start..start).map(Some);
        }
        let end_line = line.checked_add(count as u64).ok_or(EncodingError::InvalidRange)?;
        let end = self.line_start(end_line)?.unwrap_or(self.file.len());
        self.decode_range(start..end).map(Some)
    }

    pub fn decode_range(&self, range: Range<u64>) -> Result<DecodedRange<'_>, EncodingError> {
        let bytes = self.file.bytes()?;
        self.check_range(&range)?;
        if range.len_u64() > MAX_VIEWPORT_BYTES as u64 {
            return Err(EncodingError::ViewportTooLarge);
        }
        let start = (range.start as usize).max(self.detection.bom_len);
        let end = (range.end as usize).max(start);
        let aligned = self.aligned(bytes, start..end)?;
        if aligned.len() > MAX_VIEWPORT_BYTES {
            return Err(EncodingError::ViewportTooLarge);
        }
        let source = aligned.start as u64..aligned.end as u64;
        let raw = &bytes[aligned];
        let mut segments = Vec::new();
        let had_errors;
        if self.detection.encoding == Encoding::UTF8 {
            had_errors = simdutf8::basic::from_utf8(raw).is_err();
            utf8_segments(raw, |segment| segments.push(segment));
        } else {
            let (text, errors) = self.detection.encoding.0.decode_without_bom_handling(raw);
            had_errors = errors;
            if !text.is_empty() {
                segments.push(DisplaySegment::Text(text));
            }
        }
        Ok(DecodedRange { source, segments, had_errors })
    }

    fn check_range(&self, range: &Range<u64>) -> Result<(), EncodingError> {
        if range.start > range.end || range.end > self.file.len() { Err(EncodingError::InvalidRange) } else { Ok(()) }
    }

    fn aligned(&self, bytes: &[u8], range: Range<usize>) -> Result<Range<usize>, EncodingError> {
        if range.is_empty() {
            return Ok(range);
        }
        let encoding = self.detection.encoding;
        if encoding.utf16() {
            let mut start = range.start & !1;
            if matches!(encoding.unit(bytes, start), Some(0xdc00..=0xdfff))
                && start >= 2
                && matches!(encoding.unit(bytes, start - 2), Some(0xd800..=0xdbff))
            {
                start -= 2;
            }
            return Ok(start..utf16_boundary(bytes, range.end, encoding));
        }
        if encoding.0.is_single_byte() {
            return Ok(range);
        }
        let base = if range.start <= STRIDE + self.detection.bom_len {
            self.detection.bom_len
        } else {
            self.index().ok_or(EncodingError::IndexNotReady)?.before(range.start.saturating_sub(8) as u64)
        };
        let mut decoder = encoding.0.new_decoder_without_bom_handling();
        let prefix_end = base.max(range.start.saturating_sub(8));
        discard(&mut decoder, &bytes[base..prefix_end]);
        let mut last = base;
        if decoder.latin1_byte_compatible_up_to(b"").is_some() {
            last = prefix_end;
        }
        for at in prefix_end..range.start {
            discard(&mut decoder, &bytes[at..at + 1]);
            if decoder.latin1_byte_compatible_up_to(b"").is_some() {
                last = at + 1;
            }
        }
        discard(&mut decoder, &bytes[range.start..range.end]);
        let mut end = range.end;
        while end < bytes.len() && decoder.latin1_byte_compatible_up_to(b"").is_none() {
            discard(&mut decoder, &bytes[end..end + 1]);
            end += 1;
        }
        Ok(last..end)
    }
}

trait RangeLength {
    fn len_u64(&self) -> u64;
}
impl RangeLength for Range<u64> {
    fn len_u64(&self) -> u64 {
        self.end - self.start
    }
}

fn utf16_boundary(bytes: &[u8], end: usize, encoding: Encoding) -> usize {
    let mut end = (end + end % 2).min(bytes.len());
    if end >= 2
        && matches!(encoding.unit(bytes, end - 2), Some(0xd800..=0xdbff))
        && matches!(encoding.unit(bytes, end), Some(0xdc00..=0xdfff))
    {
        end += 2;
    }
    end
}

fn discard(decoder: &mut Decoder, mut bytes: &[u8]) {
    let mut scratch = [0u8; SCRATCH];
    loop {
        let (result, read, _, _) = decoder.decode_to_utf8(bytes, &mut scratch, false);
        bytes = &bytes[read..];
        if result == CoderResult::InputEmpty {
            break;
        }
    }
}

/// A lazy transformation plan. Keep it in the editor's undo transaction when
/// changing line endings; no file-sized edit list or decoded copy is created.
/// `None` preserves original line endings, including mixed styles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SavePlan {
    pub encoding: Encoding,
    pub bom: bool,
    pub line_ending: Option<LineEnding>,
}

impl SavePlan {
    pub fn convert_to(mut self, encoding: Encoding, bom: bool) -> Self {
        self.encoding = encoding;
        self.bom = bom;
        self
    }
    pub fn change_line_endings(mut self, ending: LineEnding) -> Self {
        self.line_ending = Some(ending);
        self
    }
}

/// One raw document-byte replacement. Replacement text is UTF-8 added text,
/// regardless of the original encoding. Offsets refer to the supplied snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineEndingEdit {
    pub range: Range<u64>,
    pub replacement: &'static str,
}

struct EndingPlanner<F> {
    target: LineEnding,
    pending_cr: Option<Range<u64>>,
    emit: F,
}

impl<F: FnMut(LineEndingEdit) -> Result<(), EncodingError>> EndingPlanner<F> {
    fn emit(&mut self, range: Range<u64>, current: LineEnding) -> Result<(), EncodingError> {
        if self.target != current {
            (self.emit)(LineEndingEdit { range, replacement: self.target.text() })?;
        }
        Ok(())
    }
    fn flush_cr(&mut self) -> Result<(), EncodingError> {
        if let Some(range) = self.pending_cr.take() {
            self.emit(range, LineEnding::Cr)?;
        }
        Ok(())
    }
    fn newline(&mut self, range: Range<u64>, unit: u16) -> Result<(), EncodingError> {
        if let Some(cr) = self.pending_cr.take() {
            if unit == 10 && cr.end == range.start {
                return self.emit(cr.start..range.end, LineEnding::CrLf);
            }
            self.emit(cr, LineEnding::Cr)?;
        }
        if unit == 13 {
            self.pending_cr = Some(range);
        } else {
            self.emit(range, LineEnding::Lf)?;
        }
        Ok(())
    }
}

impl EncodedFile {
    /// Stream a line-ending transformation without retaining an edit list.
    /// Apply ascending edits with an accumulated byte delta inside one Buffer
    /// transaction, or spool and apply in reverse. Never change original bytes
    /// in place. The snapshot may contain original and UTF-8 added pieces.
    pub fn plan_line_ending_edits(
        &self,
        snapshot: &Snapshot,
        target: LineEnding,
        emit: impl FnMut(LineEndingEdit) -> Result<(), EncodingError>,
    ) -> Result<(), EncodingError> {
        let original = self.file.bytes()?;
        let mut planner = EndingPlanner { target, pending_cr: None, emit };
        let mut offset = 0;
        for chunk in snapshot.source_chunks(0..snapshot.len())? {
            let (bytes, encoding) = match chunk? {
                SourceChunk::Original { file, range, bytes } => {
                    if !std::ptr::eq(file.bytes()?.as_ptr(), original.as_ptr()) || file.len() != self.file.len() {
                        return Err(EncodingError::WrongSource);
                    }
                    self.require_boundary(original, range.start)?;
                    self.require_boundary(original, range.end)?;
                    (bytes, self.detection.encoding)
                }
                SourceChunk::Added(bytes) => (bytes, Encoding::UTF8),
                SourceChunk::Decoded { .. } | SourceChunk::Opaque { .. } => return Err(EncodingError::WrongSource),
            };
            if encoding.utf16() {
                for at in (0..bytes.len().saturating_sub(1)).step_by(2) {
                    let unit = encoding.unit(bytes, at).unwrap();
                    if unit == 10 || unit == 13 {
                        planner.newline(offset + at as u64..offset + at as u64 + 2, unit)?;
                    }
                }
            } else {
                for at in memchr::memchr2_iter(b'\r', b'\n', bytes) {
                    planner.newline(offset + at as u64..offset + at as u64 + 1, bytes[at] as u16)?;
                }
            }
            offset += bytes.len() as u64;
            if planner.pending_cr.as_ref().is_some_and(|range| range.end < offset) {
                planner.flush_cr()?;
            }
        }
        planner.flush_cr()
    }

    pub fn save_plan(&self) -> SavePlan {
        SavePlan { encoding: self.detection.encoding, bom: self.detection.bom_len != 0, line_ending: None }
    }

    /// Preflight without touching a destination. UI surfaces unmappable chars
    /// and malformed conversion input before its atomic-save layer replaces a
    /// file. The same immutable snapshot and plan must be used for writing.
    pub fn validate_save(&self, snapshot: &Snapshot, plan: SavePlan) -> Result<(), EncodingError> {
        self.stream_save(snapshot, plan, &mut io::sink())
    }

    /// Validates first, then streams. A character error leaves `writer`
    /// untouched. Atomic replacement, permissions and fsync belong to KW-19.
    pub fn write_to(&self, snapshot: &Snapshot, plan: SavePlan, writer: &mut impl Write) -> Result<(), EncodingError> {
        self.validate_save(snapshot, plan)?;
        self.stream_save(snapshot, plan, writer)
    }

    fn stream_save(&self, snapshot: &Snapshot, plan: SavePlan, writer: &mut impl Write) -> Result<(), EncodingError> {
        if plan.bom && plan.encoding.bom().is_empty() {
            return Err(EncodingError::UnsupportedEncoding(format!("BOM for {}", plan.encoding.name())));
        }
        let original = self.file.bytes()?;
        let preserve = plan.encoding == self.detection.encoding && plan.line_ending.is_none();
        if plan.bom {
            writer.write_all(plan.encoding.bom())?;
        }
        let mut output = TextOutput::new(plan, writer);
        let mut added = Utf8Input::default();
        let mut source_decoder = None;
        for chunk in snapshot.source_chunks(0..snapshot.len())? {
            match chunk? {
                SourceChunk::Original { file, range, bytes } => {
                    // Original references are immutable handles; pointer
                    // equality identifies this source without comparing data.
                    if !std::ptr::eq(file.bytes()?.as_ptr(), original.as_ptr()) || file.len() != self.file.len() {
                        return Err(EncodingError::WrongSource);
                    }
                    let start = range.start.max(self.detection.bom_len as u64);
                    if start >= range.end {
                        continue;
                    }
                    added.finish()?;
                    if plan.encoding == self.detection.encoding && self.detection.encoding != Encoding::UTF8 {
                        self.require_boundary(original, start)?;
                        self.require_boundary(original, range.end)?;
                    }
                    let bytes = &bytes[(start - range.start) as usize..];
                    if preserve {
                        output.writer.write_all(bytes)?;
                    } else if plan.encoding == self.detection.encoding {
                        output.copy_raw(bytes, self.detection.encoding)?;
                    } else {
                        let decoder = source_decoder
                            .get_or_insert_with(|| self.detection.encoding.0.new_decoder_without_bom_handling());
                        transcode(decoder, bytes, false, &mut output)?;
                    }
                }
                SourceChunk::Added(bytes) => {
                    if bytes.is_empty() {
                        continue;
                    }
                    finish_source(&mut source_decoder, &mut output)?;
                    added.feed(bytes, |text| output.feed(text))?;
                }
                SourceChunk::Decoded { .. } | SourceChunk::Opaque { .. } => return Err(EncodingError::WrongSource),
            }
        }
        added.finish()?;
        finish_source(&mut source_decoder, &mut output)?;
        output.finish()
    }

    fn require_boundary(&self, bytes: &[u8], at: u64) -> Result<(), EncodingError> {
        if at == self.detection.bom_len as u64 || at == self.file.len() {
            return Ok(());
        }
        let aligned = self.aligned(bytes, at as usize..at as usize + 1)?;
        if aligned.start != at as usize {
            return Err(EncodingError::SplitCharacter(at));
        }
        Ok(())
    }
}

#[derive(Default)]
struct Utf8Input {
    pending: Vec<u8>,
}
impl Utf8Input {
    fn feed(
        &mut self,
        mut bytes: &[u8],
        mut emit: impl FnMut(&str) -> Result<(), EncodingError>,
    ) -> Result<(), EncodingError> {
        if !self.pending.is_empty() {
            while !bytes.is_empty() && self.pending.len() < 4 {
                self.pending.push(bytes[0]);
                bytes = &bytes[1..];
                match std::str::from_utf8(&self.pending) {
                    Ok(text) => {
                        emit(text)?;
                        self.pending.clear();
                        break;
                    }
                    Err(e) if e.error_len().is_some() => return Err(EncodingError::InvalidUtf8),
                    _ => {}
                }
            }
            if !self.pending.is_empty() {
                return Ok(());
            }
        }
        match std::str::from_utf8(bytes) {
            Ok(text) => emit(text),
            Err(e) => {
                emit(std::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap())?;
                if e.error_len().is_some() {
                    return Err(EncodingError::InvalidUtf8);
                }
                self.pending.extend_from_slice(&bytes[e.valid_up_to()..]);
                Ok(())
            }
        }
    }
    fn finish(&self) -> Result<(), EncodingError> {
        if self.pending.is_empty() { Ok(()) } else { Err(EncodingError::InvalidUtf8) }
    }
}

fn finish_source(decoder: &mut Option<Decoder>, output: &mut TextOutput<'_, impl Write>) -> Result<(), EncodingError> {
    if let Some(mut decoder) = decoder.take() {
        transcode(&mut decoder, b"", true, output)?;
    }
    Ok(())
}

fn transcode(
    decoder: &mut Decoder,
    bytes: &[u8],
    last: bool,
    output: &mut TextOutput<'_, impl Write>,
) -> Result<(), EncodingError> {
    let mut scratch = [0u8; SCRATCH];
    let mut at = 0;
    loop {
        let (result, read, written) = decoder.decode_to_utf8_without_replacement(&bytes[at..], &mut scratch, last);
        output.feed(std::str::from_utf8(&scratch[..written]).unwrap())?;
        at += read;
        match result {
            DecoderResult::InputEmpty => return Ok(()),
            DecoderResult::Malformed(_, _) => return Err(EncodingError::MalformedSource),
            DecoderResult::OutputFull => {}
        }
    }
}

struct TextOutput<'a, W> {
    plan: SavePlan,
    encoder: encoding_rs::Encoder,
    writer: &'a mut W,
    pending_cr: bool,
}

impl<'a, W: Write> TextOutput<'a, W> {
    fn new(plan: SavePlan, writer: &'a mut W) -> Self {
        Self { plan, encoder: plan.encoding.0.new_encoder(), writer, pending_cr: false }
    }
    fn feed(&mut self, text: &str) -> Result<(), EncodingError> {
        let Some(ending) = self.plan.line_ending else {
            return self.encode(text);
        };
        let mut start = 0;
        for (at, ch) in text.char_indices() {
            if self.pending_cr {
                self.encode(ending.text())?;
                self.pending_cr = false;
                if ch == '\n' {
                    start = at + 1;
                    continue;
                }
            }
            if ch == '\r' || ch == '\n' {
                self.encode(&text[start..at])?;
                if ch == '\r' {
                    self.pending_cr = true;
                } else {
                    self.encode(ending.text())?;
                }
                start = at + 1;
            }
        }
        self.encode(&text[start..])
    }

    // Same-encoding ending changes do not decode/re-encode non-newline bytes.
    // This retains malformed sequences and legacy aliases byte for byte.
    fn copy_raw(&mut self, bytes: &[u8], encoding: Encoding) -> Result<(), EncodingError> {
        let ending = self.plan.line_ending.unwrap();
        let width = if encoding.utf16() { 2 } else { 1 };
        let mut start = 0;
        let mut visit = |at: usize, unit: u16| -> Result<(), EncodingError> {
            if unit != 10 && unit != 13 {
                return Ok(());
            }
            if at > start {
                if self.pending_cr {
                    self.encode(ending.text())?;
                    self.pending_cr = false;
                }
                self.writer.write_all(&bytes[start..at])?;
            }
            if self.pending_cr {
                self.encode(ending.text())?;
                self.pending_cr = false;
                if unit == 10 {
                    start = at + width;
                    return Ok(());
                }
            }
            if unit == 13 {
                self.pending_cr = true;
            } else {
                self.encode(ending.text())?;
            }
            start = at + width;
            Ok(())
        };
        if encoding.utf16() {
            for at in (0..bytes.len().saturating_sub(1)).step_by(2) {
                visit(at, encoding.unit(bytes, at).unwrap())?;
            }
        } else {
            for at in memchr::memchr2_iter(b'\r', b'\n', bytes) {
                visit(at, bytes[at] as u16)?;
            }
        }
        if start < bytes.len() {
            if self.pending_cr {
                self.encode(ending.text())?;
                self.pending_cr = false;
            }
            self.writer.write_all(&bytes[start..])?;
        }
        Ok(())
    }
    fn encode(&mut self, mut text: &str) -> Result<(), EncodingError> {
        let mut scratch = [0u8; SCRATCH];
        if self.plan.encoding.utf16() {
            let mut used = 0;
            for unit in text.encode_utf16() {
                let bytes =
                    if self.plan.encoding == Encoding::UTF16_LE { unit.to_le_bytes() } else { unit.to_be_bytes() };
                scratch[used..used + 2].copy_from_slice(&bytes);
                used += 2;
                if used == scratch.len() {
                    self.writer.write_all(&scratch)?;
                    used = 0;
                }
            }
            self.writer.write_all(&scratch[..used])?;
            return Ok(());
        }
        loop {
            let (result, read, written) = self.encoder.encode_from_utf8_without_replacement(text, &mut scratch, false);
            self.writer.write_all(&scratch[..written])?;
            text = &text[read..];
            match result {
                EncoderResult::InputEmpty => return Ok(()),
                EncoderResult::Unmappable(character) => {
                    return Err(EncodingError::Unmappable { character, encoding: self.plan.encoding.name() });
                }
                EncoderResult::OutputFull => {}
            }
        }
    }
    fn finish(&mut self) -> Result<(), EncodingError> {
        if self.pending_cr {
            self.encode(self.plan.line_ending.unwrap().text())?;
            self.pending_cr = false;
        }
        Ok(()) // Supported encoders have no final shift sequence.
    }
}
