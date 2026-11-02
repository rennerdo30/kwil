//! Bounded physical-byte queries, run only by the document worker.
use kwil_core_text::Snapshot;
use std::{io, ops::Range};

pub const MAX_PATTERN: usize = 256;
pub const MAX_COPY: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Query {
    Find { bytes: Vec<u8>, start: u64 },
    Copy(Range<u64>),
}

#[derive(Debug)]
pub enum Answer {
    Found(Option<Range<u64>>),
    Copied(String),
}

pub fn validate(query: &Query, len: u64) -> io::Result<()> {
    let valid = match query {
        Query::Find { bytes, start } => !bytes.is_empty() && bytes.len() <= MAX_PATTERN && *start <= len,
        Query::Copy(range) => range.start < range.end && range.end <= len && range.end - range.start <= MAX_COPY,
    };
    if valid {
        Ok(())
    } else {
        Err(io::Error::other("Find allows 1-256 bytes; copy requires a selection of at most 64 KiB"))
    }
}

pub fn run(snapshot: &Snapshot, query: &Query, mut cancelled: impl FnMut() -> bool) -> io::Result<Answer> {
    validate(query, snapshot.len())?;
    let check = |cancelled: &mut dyn FnMut() -> bool| {
        if cancelled() { Err(io::Error::new(io::ErrorKind::Interrupted, "Hex query cancelled")) } else { Ok(()) }
    };
    check(&mut cancelled)?;
    match query {
        Query::Copy(range) => {
            use std::fmt::Write;
            let mut out = String::with_capacity((range.end - range.start) as usize * 3);
            for chunk in snapshot.chunks(range.clone())? {
                check(&mut cancelled)?;
                for byte in chunk.map_err(io::Error::other)?.iter() {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    write!(out, "{byte:02X}").expect("String write");
                }
            }
            Ok(Answer::Copied(out))
        }
        Query::Find { bytes, start } => {
            // KMP carries only a pattern-sized prefix table across bounded
            // reads and piece boundaries. Linear time even for repeated bytes.
            let mut prefixes = vec![0; bytes.len()];
            let mut matched = 0;
            for i in 1..bytes.len() {
                while matched > 0 && bytes[i] != bytes[matched] {
                    matched = prefixes[matched - 1];
                }
                if bytes[i] == bytes[matched] {
                    matched += 1;
                }
                prefixes[i] = matched;
            }
            // Wrap once. Extend the prefix scan enough to include a match
            // beginning before the starting cursor and ending after it.
            for range in [*start..snapshot.len(), 0..start.saturating_add(bytes.len() as u64 - 1).min(snapshot.len())] {
                let mut at = range.start;
                matched = 0;
                while at < range.end {
                    check(&mut cancelled)?;
                    let end = at.saturating_add(64 * 1024).min(range.end);
                    for chunk in snapshot.chunks(at..end)? {
                        for &byte in chunk.map_err(io::Error::other)?.iter() {
                            while matched > 0 && byte != bytes[matched] {
                                matched = prefixes[matched - 1];
                            }
                            if byte == bytes[matched] {
                                matched += 1;
                            }
                            at += 1;
                            if matched == bytes.len() {
                                return Ok(Answer::Found(Some(at - bytes.len() as u64..at)));
                            }
                        }
                    }
                }
            }
            Ok(Answer::Found(None))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kwil_core_text::Buffer;

    #[test]
    fn query_streams_across_read_and_piece_boundaries_and_wraps() {
        let mut buffer = Buffer::empty();
        buffer.insert(0, &vec![0xff; 65535]).unwrap();
        buffer.insert(65535, &[0, 0xfe, b'A']).unwrap();
        let snapshot = buffer.snapshot();
        for start in [0, 65536] {
            let answer = run(&snapshot, &Query::Find { bytes: vec![0xff, 0, 0xfe], start }, || false).unwrap();
            assert!(matches!(answer, Answer::Found(Some(range)) if range == (65534..65537)));
        }
        assert!(
            matches!(run(&snapshot, &Query::Copy(65534..65538), || false).unwrap(), Answer::Copied(text) if text == "FF 00 FE 41")
        );
        assert!(run(&snapshot, &Query::Copy(0..65537), || false).is_err());
        assert!(run(&snapshot, &Query::Find { bytes: vec![0], start: 0 }, || true).is_err());
        assert!(matches!(
            run(&snapshot, &Query::Find { bytes: vec![42], start: 0 }, || false).unwrap(),
            Answer::Found(None)
        ));
    }
}
