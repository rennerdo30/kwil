//! Worker-only capsule parsing. The sink owns a staged recovery revision;
//! errors poison that revision, never publish a partial fragment or origin ID.
//! Outer journal authentication/atomic batch publication and backing retirement
//! remain caller responsibilities. This parser does not commit a revision.

use super::{CAPSULE_VERSION, MAX_READ_BYTES};
use kwil_core_text::{decoded_backing::Interpretation, encoding::Encoding};
use std::{
    io::{self, Read},
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

/// Explicit caller limits; no product settings or implicit disk quota.
pub struct DecodeLimits {
    pub max_physical_bytes: u64,
    pub max_logical_bytes: u64,
    pub max_records: u64,
}

/// All operations affect an uncommitted fragment/revision. Origin definitions
/// must reject duplicate IDs and references must resolve only accepted or
/// current staged definitions. `abort` must make subsequent commit impossible.
pub trait CapsuleSink {
    fn begin_fragment(&mut self) -> io::Result<()>;
    fn define_origin(
        &mut self,
        id: u64,
        interpretation: Interpretation,
        raw_len: u64,
        input: &mut impl Read,
    ) -> io::Result<()>;
    fn append_origin(&mut self, id: u64, logical: Range<u64>) -> io::Result<()>;
    fn append_bytes(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn append_opaque(
        &mut self,
        encoding: Encoding,
        raw: &[u8],
        endpoints_le: &[u8],
        logical_count: u64,
    ) -> io::Result<()>;
    fn end_fragment(&mut self, logical_len: u64) -> io::Result<()>;
    fn abort(&mut self);
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn poll(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) { Err(io::Error::from(io::ErrorKind::Interrupted)) } else { Ok(()) }
}
fn exact(input: &mut impl Read, mut bytes: &mut [u8], cancel: &AtomicBool) -> io::Result<()> {
    while !bytes.is_empty() {
        poll(cancel)?;
        let result = input.read(bytes);
        let n = result?;
        // A concrete I/O failure wins over a coincident cancellation signal.
        poll(cancel)?;
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        bytes = &mut bytes[n..];
    }
    Ok(())
}
fn number<const N: usize>(input: &mut impl Read, cancel: &AtomicBool) -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    exact(input, &mut bytes, cancel)?;
    Ok(bytes)
}
fn u64_(input: &mut impl Read, cancel: &AtomicBool) -> io::Result<u64> {
    Ok(u64::from_le_bytes(number(input, cancel)?))
}
fn label(input: &mut impl Read, len: u16, cancel: &AtomicBool) -> io::Result<Encoding> {
    // Every supported canonical encoding name fits; reject oversized labels
    // before allocation, including otherwise-valid unbounded alias spelling.
    let mut bytes = [0; 64];
    let len = usize::from(len);
    if len == 0 || len > bytes.len() {
        return Err(invalid("invalid encoding label length"));
    }
    exact(input, &mut bytes[..len], cancel)?;
    let name = std::str::from_utf8(&bytes[..len]).map_err(|_| invalid("invalid encoding label"))?;
    Encoding::for_label(name).map_err(|_| invalid("unsupported encoding label"))
}
fn contribution(total: &mut u64, len: u64, expected: u64) -> io::Result<()> {
    *total =
        total.checked_add(len).filter(|n| *n <= expected).ok_or_else(|| invalid("capsule logical length overflow"))?;
    Ok(())
}
fn bounded_bytes(input: &mut impl Read, len: usize, cancel: &AtomicBool) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len).map_err(|_| io::Error::other("capsule scratch allocation failed"))?;
    bytes.resize(len, 0);
    exact(input, &mut bytes, cancel)?;
    Ok(bytes)
}

// A callback may use read_exact, which retries Interrupted. Return a
// non-retryable adapter error to the callback while retaining the ORIGINAL
// error for the decoder. Even a callback that ignores errors cannot erase it.
struct OriginInput<'a, R: Read> {
    input: io::Take<&'a mut R>,
    cancel: &'a AtomicBool,
    failure: Option<io::Error>,
}
impl<R: Read> OriginInput<'_, R> {
    fn fail(&mut self, error: io::Error) -> io::Result<usize> {
        if self.failure.is_none() {
            self.failure = Some(error);
        }
        Err(io::Error::other("origin payload read failed; decoder retains cause"))
    }
}
impl<R: Read> Read for OriginInput<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.failure.is_some() {
            return Err(io::Error::other("origin payload previously failed"));
        }
        if let Err(error) = poll(self.cancel) {
            return self.fail(error);
        }
        let len = bytes.len().min(MAX_READ_BYTES);
        let n = match self.input.read(&mut bytes[..len]) {
            Ok(n) => n,
            Err(error) => return self.fail(error),
        };
        if let Err(error) = poll(self.cancel) {
            return self.fail(error);
        }
        match n {
            0 if len != 0 && self.input.limit() != 0 => self.fail(io::Error::from(io::ErrorKind::UnexpectedEof)),
            n => Ok(n),
        }
    }
}

/// Decode exactly one authenticated physical payload. End must be its final
/// byte. Neither short payloads nor suffix bytes are silently accepted. The
/// reader may contain a following frame, which is never consumed.
pub fn decode_capsule(
    input: &mut impl Read,
    physical_len: u64,
    expected_logical_len: u64,
    limits: &DecodeLimits,
    cancel: &AtomicBool,
    sink: &mut impl CapsuleSink,
) -> io::Result<()> {
    let result = decode_inner(input, physical_len, expected_logical_len, limits, cancel, sink);
    if result.is_err() {
        sink.abort();
    }
    result
}

fn decode_inner(
    input: &mut impl Read,
    physical_len: u64,
    expected: u64,
    limits: &DecodeLimits,
    cancel: &AtomicBool,
    sink: &mut impl CapsuleSink,
) -> io::Result<()> {
    poll(cancel)?;
    if physical_len > limits.max_physical_bytes || expected > limits.max_logical_bytes {
        return Err(invalid("capsule exceeds recovery limit"));
    }
    let mut input = input.take(physical_len);
    let header = number::<16>(&mut input, cancel)?;
    if &header[..4] != b"KWCP"
        || u16::from_le_bytes(header[4..6].try_into().unwrap()) != CAPSULE_VERSION
        || header[6..8] != [0, 0]
        || u64::from_le_bytes(header[8..].try_into().unwrap()) != expected
    {
        return Err(invalid("unsupported capsule header or logical length"));
    }
    sink.begin_fragment()?;
    let mut records = 0u64;
    let mut total = 0u64;
    let mut scratch = [0; MAX_READ_BYTES];
    loop {
        poll(cancel)?;
        let tag = number::<1>(&mut input, cancel)?[0];
        if tag == 0 {
            if input.limit() != 0 || total != expected {
                return Err(invalid("capsule end or logical length mismatch"));
            }
            poll(cancel)?;
            sink.end_fragment(total)?;
            return poll(cancel);
        }
        records = records
            .checked_add(1)
            .filter(|n| *n <= limits.max_records)
            .ok_or_else(|| invalid("capsule record limit exceeded"))?;
        match tag {
            1 => {
                let id = u64_(&mut input, cancel)?;
                if id == 0 {
                    return Err(invalid("zero origin ID"));
                }
                let label_len = u16::from_le_bytes(number(&mut input, cancel)?);
                let bom_len = number::<1>(&mut input, cancel)?[0];
                let raw_len = u64_(&mut input, cancel)?;
                let encoding = label(&mut input, label_len, cancel)?;
                let interpretation = Interpretation::new(encoding, usize::from(bom_len))
                    .map_err(|_| invalid("invalid origin interpretation"))?;
                if raw_len > input.limit() {
                    return Err(invalid("origin exceeds capsule payload"));
                }
                let mut raw = OriginInput { input: (&mut input).take(raw_len), cancel, failure: None };
                let result = sink.define_origin(id, interpretation, raw_len, &mut raw);
                if let Some(error) = raw.failure.take() {
                    return Err(error);
                }
                result?;
                if raw.input.limit() != 0 {
                    return Err(invalid("origin sink did not consume raw payload"));
                }
            }
            2 => {
                let id = u64_(&mut input, cancel)?;
                let start = u64_(&mut input, cancel)?;
                let end = u64_(&mut input, cancel)?;
                if id == 0 || end < start {
                    return Err(invalid("invalid origin reference"));
                }
                contribution(&mut total, end - start, expected)?;
                sink.append_origin(id, start..end)?;
            }
            3 => {
                let len = u64_(&mut input, cancel)?;
                contribution(&mut total, len, expected)?;
                if len > input.limit() {
                    return Err(invalid("logical bytes exceed capsule payload"));
                }
                let mut remaining = len;
                while remaining != 0 {
                    let n = remaining.min(MAX_READ_BYTES as u64) as usize;
                    exact(&mut input, &mut scratch[..n], cancel)?;
                    sink.append_bytes(&scratch[..n])?;
                    remaining -= n as u64;
                }
            }
            4 => {
                let label_len = u16::from_le_bytes(number(&mut input, cancel)?);
                let count = u64_(&mut input, cancel)?;
                let raw_len = u64_(&mut input, cancel)?;
                let table_len = u64_(&mut input, cancel)?;
                if count == 0
                    || raw_len == 0
                    || raw_len > MAX_READ_BYTES as u64
                    || table_len > MAX_READ_BYTES as u64
                    || count.checked_add(1).and_then(|n| n.checked_mul(4)) != Some(table_len)
                {
                    return Err(invalid("invalid opaque block bounds"));
                }
                contribution(&mut total, count, expected)?;
                let encoding = label(&mut input, label_len, cancel)?;
                let raw = bounded_bytes(&mut input, raw_len as usize, cancel)?;
                let table = bounded_bytes(&mut input, table_len as usize, cancel)?;
                let mut previous = 0u32;
                for (index, bytes) in table.as_chunks::<4>().0.iter().enumerate() {
                    let endpoint = u32::from_le_bytes(*bytes);
                    if (index == 0 && endpoint != 0)
                        || (index != 0 && endpoint <= previous)
                        || u64::from(endpoint) > raw_len
                    {
                        return Err(invalid("invalid opaque endpoint table"));
                    }
                    previous = endpoint;
                }
                if u64::from(previous) != raw_len {
                    return Err(invalid("opaque final endpoint mismatch"));
                }
                sink.append_opaque(encoding, &raw, &table, count)?;
            }
            _ => return Err(invalid("unknown capsule record")),
        }
        poll(cancel)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Sink {
        origins: BTreeMap<u64, Vec<u8>>,
        text: Vec<u8>,
        refs: Vec<(u64, Range<u64>)>,
        opaque: Vec<(Vec<u8>, Vec<u8>, u64)>,
        ended: Option<u64>,
        aborted: bool,
        fail_bytes: bool,
        swallow_origin_error: bool,
    }
    impl CapsuleSink for Sink {
        fn begin_fragment(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn define_origin(&mut self, id: u64, _: Interpretation, raw_len: u64, input: &mut impl Read) -> io::Result<()> {
            if self.origins.contains_key(&id) {
                return Err(invalid("duplicate origin"));
            }
            let mut raw = vec![0; raw_len as usize];
            let result = input.read_exact(&mut raw);
            if result.is_err() && self.swallow_origin_error {
                let _ = input.read(&mut [0; 1]);
                return Ok(());
            }
            result?;
            self.origins.insert(id, raw);
            Ok(())
        }
        fn append_origin(&mut self, id: u64, logical: Range<u64>) -> io::Result<()> {
            if !self.origins.contains_key(&id) {
                return Err(invalid("unknown origin"));
            }
            self.refs.push((id, logical));
            Ok(())
        }
        fn append_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
            if self.fail_bytes {
                return Err(io::Error::from(io::ErrorKind::StorageFull));
            }
            self.text.extend_from_slice(bytes);
            Ok(())
        }
        fn append_opaque(&mut self, _: Encoding, raw: &[u8], table: &[u8], count: u64) -> io::Result<()> {
            self.opaque.push((raw.to_vec(), table.to_vec(), count));
            Ok(())
        }
        fn end_fragment(&mut self, len: u64) -> io::Result<()> {
            self.ended = Some(len);
            Ok(())
        }
        fn abort(&mut self) {
            self.aborted = true;
            self.origins.clear();
            self.text.clear();
            self.refs.clear();
            self.opaque.clear();
            self.ended = None;
        }
    }
    fn limits() -> DecodeLimits {
        DecodeLimits { max_physical_bytes: 1 << 20, max_logical_bytes: 1 << 20, max_records: 64 }
    }
    fn header(logical: u64) -> Vec<u8> {
        let mut out = b"KWCP".to_vec();
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&logical.to_le_bytes());
        out
    }
    fn definition(out: &mut Vec<u8>, id: u64, encoding: Encoding, bom: u8, raw: &[u8]) {
        out.push(1);
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&(encoding.name().len() as u16).to_le_bytes());
        out.push(bom);
        out.extend_from_slice(&(raw.len() as u64).to_le_bytes());
        out.extend_from_slice(encoding.name().as_bytes());
        out.extend_from_slice(raw);
    }
    fn reference(out: &mut Vec<u8>, id: u64, start: u64, end: u64) {
        out.push(2);
        for n in [id, start, end] {
            out.extend_from_slice(&n.to_le_bytes());
        }
    }
    fn text(out: &mut Vec<u8>, bytes: &[u8]) {
        out.push(3);
        out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        out.extend_from_slice(bytes);
    }
    fn opaque(out: &mut Vec<u8>, raw: &[u8], offsets: &[u32]) {
        let encoding = Encoding::UTF16_LE;
        out.push(4);
        out.extend_from_slice(&(encoding.name().len() as u16).to_le_bytes());
        for n in [(offsets.len() - 1) as u64, raw.len() as u64, (offsets.len() * 4) as u64] {
            out.extend_from_slice(&n.to_le_bytes());
        }
        out.extend_from_slice(encoding.name().as_bytes());
        out.extend_from_slice(raw);
        for n in offsets {
            out.extend_from_slice(&n.to_le_bytes());
        }
    }
    fn run(bytes: &[u8], logical: u64, sink: &mut Sink) -> io::Result<()> {
        decode_capsule(&mut &bytes[..], bytes.len() as u64, logical, &limits(), &AtomicBool::new(false), sink)
    }

    #[test]
    fn decode_all_records_preserves_raw_and_stops_at_payload_end() {
        let mut bytes = header(6);
        definition(&mut bytes, 1, Encoding::UTF16_LE, 2, b"\xff\xfeA\0\0\xd8");
        reference(&mut bytes, 1, 0, 2);
        text(&mut bytes, b"\xc3\xa9\xff");
        opaque(&mut bytes, b"\0\xd8", &[0, 2]);
        bytes.push(0);
        let len = bytes.len();
        bytes.extend_from_slice(b"NEXT");
        let mut input = io::Cursor::new(bytes);
        let mut sink = Sink::default();
        decode_capsule(&mut input, len as u64, 6, &limits(), &AtomicBool::new(false), &mut sink).unwrap();
        assert_eq!(input.position(), len as u64);
        assert_eq!(sink.origins[&1], b"\xff\xfeA\0\0\xd8");
        assert_eq!(sink.refs, vec![(1, 0..2)]);
        assert_eq!(sink.text, b"\xc3\xa9\xff");
        assert_eq!(sink.opaque, vec![(b"\0\xd8".to_vec(), [0u32.to_le_bytes(), 2u32.to_le_bytes()].concat(), 1)]);
        assert_eq!(sink.ended, Some(6));
        assert!(!sink.aborted);
    }

    #[test]
    fn decode_writer_capsules_with_arbitrary_byte_segmentation() {
        use crate::document_worker::provenance::OriginRegistry;
        use kwil_core_text::Buffer;
        use std::sync::Arc;
        let bytes = vec![b'A'; (64 << 10) + 7];
        let mut buffer = Buffer::empty();
        buffer.insert(0, &bytes).unwrap();
        let registry = OriginRegistry::new(4, 4096);
        let plan = registry.plan(buffer.snapshot(), 0..buffer.len(), Arc::new(AtomicBool::new(false))).unwrap();
        let len = plan.physical_len();
        let mut writer = registry.start(plan).unwrap();
        let mut capsule = Vec::new();
        writer.read_to_end(&mut capsule).unwrap();
        let _unadmitted = writer.finish().unwrap();
        struct Small<'a> {
            bytes: &'a [u8],
            largest: usize,
        }
        impl Read for Small<'_> {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                self.largest = self.largest.max(out.len());
                let n = out.len().min(3).min(self.bytes.len());
                out[..n].copy_from_slice(&self.bytes[..n]);
                self.bytes = &self.bytes[n..];
                Ok(n)
            }
        }
        let mut input = Small { bytes: &capsule, largest: 0 };
        let mut sink = Sink::default();
        decode_capsule(&mut input, len, bytes.len() as u64, &limits(), &AtomicBool::new(false), &mut sink).unwrap();
        assert_eq!(sink.text, bytes);
        assert!(input.largest <= MAX_READ_BYTES);
        assert!(input.bytes.is_empty());
    }

    #[test]
    fn decode_rejects_truncation_suffix_headers_counts_and_limits_atomically() {
        let mut valid = header(1);
        text(&mut valid, b"A");
        valid.push(0);
        for end in 0..valid.len() {
            let mut sink = Sink::default();
            assert!(run(&valid[..end], 1, &mut sink).is_err());
            assert!(sink.aborted);
            assert!(sink.text.is_empty());
        }
        let mut bad = Vec::new();
        let mut suffix = valid.clone();
        suffix.push(0);
        bad.push(suffix);
        for at in [0, 4, 6, 8] {
            let mut bytes = valid.clone();
            bytes[at] ^= 1;
            bad.push(bytes);
        }
        let mut unknown = header(0);
        unknown.extend_from_slice(&[99, 0]);
        bad.push(unknown);
        let mut overflow = header(1);
        overflow.push(3);
        overflow.extend_from_slice(&u64::MAX.to_le_bytes());
        overflow.push(0);
        bad.push(overflow);
        for bytes in bad {
            let mut sink = Sink::default();
            assert!(run(&bytes, 1, &mut sink).is_err());
            assert!(sink.aborted);
        }
        let mut sink = Sink::default();
        let mut cap = limits();
        cap.max_records = 0;
        assert!(
            decode_capsule(&mut &valid[..], valid.len() as u64, 1, &cap, &AtomicBool::new(false), &mut sink).is_err()
        );
        assert!(sink.aborted);
        cap = limits();
        cap.max_physical_bytes = 1;
        assert!(
            decode_capsule(&mut &valid[..], valid.len() as u64, 1, &cap, &AtomicBool::new(false), &mut sink).is_err()
        );
    }

    #[test]
    fn decode_empty_seed_identity_and_sink_failures_never_publish() {
        let mut bytes = header(0);
        definition(&mut bytes, 7, Encoding::UTF8, 3, b"\xef\xbb\xbf");
        reference(&mut bytes, 7, 0, 0);
        bytes.push(0);
        let mut sink = Sink::default();
        run(&bytes, 0, &mut sink).unwrap();
        assert_eq!(sink.refs, vec![(7, 0..0)]);
        let mut unknown = header(0);
        reference(&mut unknown, 8, 0, 0);
        unknown.push(0);
        assert!(run(&unknown, 0, &mut sink).is_err());
        assert!(sink.aborted);
        assert!(sink.origins.is_empty());
        let mut duplicate = header(0);
        definition(&mut duplicate, 1, Encoding::UTF8, 0, b"");
        definition(&mut duplicate, 1, Encoding::UTF8, 0, b"");
        duplicate.push(0);
        assert!(run(&duplicate, 0, &mut Sink::default()).is_err());
        let mut bytes = header(1);
        text(&mut bytes, b"A");
        bytes.push(0);
        let mut sink = Sink { fail_bytes: true, ..Sink::default() };
        assert_eq!(run(&bytes, 1, &mut sink).unwrap_err().kind(), io::ErrorKind::StorageFull);
        assert!(sink.aborted);
    }

    #[test]
    fn decode_rejects_opaque_endpoint_and_allocation_bounds() {
        for table in [&[1, 2][..], &[0, 0][..], &[0, 3][..], &[0, 1][..]] {
            let mut bytes = header(1);
            opaque(&mut bytes, b"\0\xd8", table);
            bytes.push(0);
            let mut sink = Sink::default();
            assert!(run(&bytes, 1, &mut sink).is_err());
            assert!(sink.aborted);
        }
        let mut bytes = header(1);
        bytes.push(4);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        for n in [1u64, u64::MAX, 8] {
            bytes.extend_from_slice(&n.to_le_bytes());
        }
        bytes.push(0);
        assert!(run(&bytes, 1, &mut Sink::default()).is_err());
    }

    #[test]
    fn decode_interrupted_and_cancelled_reads_abort_without_retry() {
        struct Stops<'a> {
            cancel: Option<&'a AtomicBool>,
            calls: usize,
        }
        impl Read for Stops<'_> {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                self.calls += 1;
                assert_eq!(self.calls, 1);
                if let Some(cancel) = self.cancel {
                    cancel.store(true, Ordering::Relaxed);
                }
                Err(io::Error::from(io::ErrorKind::Interrupted))
            }
        }
        let cancel = AtomicBool::new(false);
        let mut reader = Stops { cancel: Some(&cancel), calls: 0 };
        let mut sink = Sink::default();
        assert_eq!(
            decode_capsule(&mut reader, 17, 0, &limits(), &cancel, &mut sink).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert!(sink.aborted);
        assert_eq!(reader.calls, 1);
        cancel.store(false, Ordering::Relaxed);
        let mut reader = Stops { cancel: None, calls: 0 };
        let mut sink = Sink::default();
        assert!(decode_capsule(&mut reader, 17, 0, &limits(), &cancel, &mut sink).is_err());
        assert!(sink.aborted);
        assert_eq!(reader.calls, 1);
    }

    struct OriginReads<'a> {
        input: io::Cursor<Vec<u8>>,
        raw_start: u64,
        largest: usize,
        raw_calls: usize,
        interrupt_once: bool,
        cancel: Option<&'a AtomicBool>,
    }
    impl Read for OriginReads<'_> {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.largest = self.largest.max(bytes.len());
            if self.input.position() >= self.raw_start {
                self.raw_calls += 1;
                if self.interrupt_once {
                    self.interrupt_once = false;
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
                if let Some(cancel) = self.cancel {
                    let n = bytes.len().min(3);
                    let result = self.input.read(&mut bytes[..n]);
                    cancel.store(true, Ordering::Relaxed);
                    return result;
                }
            }
            self.input.read(bytes)
        }
    }
    fn origin_input<'a>(raw: &[u8], cancel: Option<&'a AtomicBool>, interrupt_once: bool) -> OriginReads<'a> {
        let mut bytes = header(0);
        definition(&mut bytes, 1, Encoding::UTF8, 0, raw);
        let raw_start = (bytes.len() - raw.len()) as u64;
        bytes.push(0);
        OriginReads { input: io::Cursor::new(bytes), raw_start, largest: 0, raw_calls: 0, interrupt_once, cancel }
    }

    #[test]
    fn decode_origin_callback_requests_are_capped_before_underlying_read() {
        let raw = vec![b'A'; MAX_READ_BYTES + 1];
        let mut input = origin_input(&raw, None, false);
        let len = input.input.get_ref().len() as u64;
        let mut sink = Sink::default();
        decode_capsule(&mut input, len, 0, &limits(), &AtomicBool::new(false), &mut sink).unwrap();
        assert_eq!(sink.origins[&1], raw);
        assert!(input.largest <= MAX_READ_BYTES, "delegated read exceeded bound: {}", input.largest);
    }

    #[test]
    fn decode_origin_cancellation_stops_inside_read_exact_and_aborts() {
        let cancel = AtomicBool::new(false);
        let mut input = origin_input(b"payload", Some(&cancel), false);
        let len = input.input.get_ref().len() as u64;
        let mut sink = Sink::default();
        assert_eq!(
            decode_capsule(&mut input, len, 0, &limits(), &cancel, &mut sink).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(input.raw_calls, 1, "sink must stop before a second origin read");
        assert!(sink.aborted);
        assert!(sink.origins.is_empty());
    }

    #[test]
    fn decode_origin_interrupted_failure_survives_callback_retry_or_swallow() {
        for swallow_origin_error in [false, true] {
            let mut input = origin_input(b"payload", None, true);
            let len = input.input.get_ref().len() as u64;
            let mut sink = Sink { swallow_origin_error, ..Sink::default() };
            assert_eq!(
                decode_capsule(&mut input, len, 0, &limits(), &AtomicBool::new(false), &mut sink).unwrap_err().kind(),
                io::ErrorKind::Interrupted
            );
            assert_eq!(input.raw_calls, 1, "failed origin reader must remain poisoned");
            assert!(sink.aborted);
            assert!(sink.origins.is_empty());
        }
    }

    #[test]
    fn decode_io_error_wins_over_simultaneous_cancellation_in_header_and_origin() {
        struct Fails<'a> {
            input: io::Cursor<Vec<u8>>,
            fail_at: u64,
            cancel: &'a AtomicBool,
        }
        impl Read for Fails<'_> {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                if self.input.position() >= self.fail_at {
                    self.cancel.store(true, Ordering::Relaxed);
                    return Err(io::Error::new(io::ErrorKind::StorageFull, "injected read failure"));
                }
                self.input.read(bytes)
            }
        }
        for header in [true, false] {
            let cancel = AtomicBool::new(false);
            let input = origin_input(b"payload", None, false);
            let len = input.input.get_ref().len() as u64;
            let fail_at = if header { 0 } else { input.raw_start };
            let mut input = Fails { input: input.input, fail_at, cancel: &cancel };
            let mut sink = Sink::default();
            let error = decode_capsule(&mut input, len, 0, &limits(), &cancel, &mut sink).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::StorageFull, "original failure must survive cancellation");
            assert!(error.to_string().contains("injected read failure"));
            assert!(sink.aborted);
            assert!(sink.origins.is_empty());
        }
    }
}
