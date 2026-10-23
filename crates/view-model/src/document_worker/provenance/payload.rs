//! Worker-only bridge from journal byte events to a capsule reader. Small
//! payloads remain in bounded memory; larger payloads use one private spool,
//! never a file per event. Explicit retirement retains paths on cleanup failure.

use super::MAX_READ_BYTES;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Cursor, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

#[must_use = "retire private payload storage explicitly on the journal worker"]
pub struct CapsulePayload {
    parent: PathBuf,
    directory: Option<PathBuf>,
    file: Option<File>,
    memory: Vec<u8>,
    expected: u64,
    written: u64,
    poisoned: bool,
    retired: bool,
}

impl CapsulePayload {
    /// `max_physical` is the caller's explicit spool/disk admission limit.
    /// Parent must already be app-private; no external file is ever opened.
    pub fn new(parent: &Path, expected: u64, max_physical: u64) -> io::Result<Self> {
        if expected > max_physical {
            return Err(invalid("capsule payload exceeds storage limit"));
        }
        let mut memory = Vec::new();
        memory.try_reserve_exact(expected.min(MAX_READ_BYTES as u64) as usize).map_err(io::Error::other)?;
        Ok(Self {
            parent: parent.to_owned(),
            directory: None,
            file: None,
            memory,
            expected,
            written: 0,
            poisoned: false,
            retired: false,
        })
    }

    /// Journal events are already <=64KiB. Overflow/error poisons this payload;
    /// a partial spool can never become an apparently complete capsule.
    pub fn push(&mut self, bytes: &[u8], cancel: &AtomicBool) -> io::Result<()> {
        let result = (|| {
            self.ready()?;
            poll(cancel)?;
            if bytes.len() > MAX_READ_BYTES {
                return Err(invalid("capsule event exceeds 64KiB"));
            }
            let next = self
                .written
                .checked_add(bytes.len() as u64)
                .filter(|n| *n <= self.expected)
                .ok_or_else(|| invalid("capsule payload overflow"))?;
            if self.file.is_none() && next <= MAX_READ_BYTES as u64 {
                self.memory.extend_from_slice(bytes);
            } else {
                if self.file.is_none() {
                    self.spill(cancel)?;
                }
                write_polled(self.file.as_mut().unwrap(), bytes, cancel)?;
            }
            poll(cancel)?;
            self.written = next;
            Ok(())
        })();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    fn spill(&mut self, cancel: &AtomicBool) -> io::Result<()> {
        poll(cancel)?;
        let directory = loop {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = self.parent.join(format!("kwil-capsule-{}-{id}", std::process::id()));
            let builder = fs::DirBuilder::new();
            #[cfg(unix)]
            let mut builder = builder;
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => break path,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };
        self.directory = Some(directory);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        self.file = Some(options.open(self.directory.as_ref().unwrap().join("payload"))?);
        write_polled(self.file.as_mut().unwrap(), &self.memory, cancel)?;
        self.memory = Vec::new();
        Ok(())
    }

    /// Borrowing prevents append/retirement while the decoder is reading. A
    /// second reader starts from zero; dropping a partial reader publishes nothing.
    pub fn reader(&mut self, cancel: &AtomicBool) -> io::Result<PayloadReader<'_>> {
        self.ready()?;
        poll(cancel)?;
        if self.written != self.expected {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "incomplete capsule payload"));
        }
        if let Some(file) = &mut self.file {
            file.flush()?;
            poll(cancel)?;
            file.seek(SeekFrom::Start(0))?;
            poll(cancel)?;
            Ok(PayloadReader::File(file))
        } else {
            Ok(PayloadReader::Memory(Cursor::new(&self.memory)))
        }
    }

    /// Worker only. Closes the spool before unlinking. On failure exact paths
    /// remain owned for a retry; implicit Drop closes handles but never unlinks.
    pub fn retire(&mut self) -> io::Result<()> {
        self.file.take();
        self.memory = Vec::new();
        self.retired = true;
        if let Some(directory) = &self.directory {
            let path = directory.join("payload");
            if let Err(error) = fs::remove_file(&path)
                && error.kind() != io::ErrorKind::NotFound
            {
                return Err(io::Error::new(error.kind(), format!("payload cleanup {}: {error}", path.display())));
            }
            if let Err(error) = fs::remove_dir(directory)
                && error.kind() != io::ErrorKind::NotFound
            {
                return Err(io::Error::new(error.kind(), format!("payload cleanup {}: {error}", directory.display())));
            }
        }
        self.directory = None;
        Ok(())
    }
    fn ready(&self) -> io::Result<()> {
        if self.poisoned || self.retired { Err(invalid("capsule payload is poisoned or retired")) } else { Ok(()) }
    }
}

pub enum PayloadReader<'a> {
    Memory(Cursor<&'a [u8]>),
    File(&'a mut File),
}
impl Read for PayloadReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Memory(input) => input.read(bytes),
            Self::File(input) => input.read(bytes),
        }
    }
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn poll(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) { Err(io::Error::from(io::ErrorKind::Interrupted)) } else { Ok(()) }
}
fn write_polled(writer: &mut impl Write, mut bytes: &[u8], cancel: &AtomicBool) -> io::Result<()> {
    while !bytes.is_empty() {
        poll(cancel)?;
        let n = writer.write(bytes)?;
        poll(cancel)?;
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::WriteZero));
        }
        bytes = &bytes[n..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("kwil-capsule-test-{}-{id}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn count(&self) -> usize {
            fs::read_dir(&self.0).unwrap().count()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // Clean only this unique fixture's exact known scratch names after
            // an intentional fault assertion unwinds. No recursive deletion.
            if let Ok(entries) = fs::read_dir(&self.0) {
                let prefix = format!("kwil-capsule-{}-", std::process::id());
                for entry in entries.flatten() {
                    if entry.file_name().to_string_lossy().starts_with(&prefix)
                        && entry.file_type().is_ok_and(|kind| kind.is_dir())
                    {
                        let _ = fs::remove_file(entry.path().join("payload"));
                        let _ = fs::remove_file(entry.path().join("blocker"));
                        let _ = fs::remove_dir(entry.path());
                    }
                }
            }
            let _ = fs::remove_dir(&self.0);
        }
    }
    #[test]
    fn small_payload_stays_in_memory_without_private_files() {
        let fixture = Fixture::new();
        let cancel = AtomicBool::new(false);
        let mut payload = CapsulePayload::new(&fixture.0, 7, 7).unwrap();
        payload.push(b"abc", &cancel).unwrap();
        payload.push(b"defg", &cancel).unwrap();
        assert!(payload.file.is_none());
        assert_eq!(fixture.count(), 0);
        let mut result = Vec::new();
        payload.reader(&cancel).unwrap().read_to_end(&mut result).unwrap();
        assert_eq!(result, b"abcdefg");
        payload.retire().unwrap();
        assert_eq!(fixture.count(), 0);
    }
    #[test]
    fn large_payload_spills_once_and_reads_exactly_then_retires() {
        let fixture = Fixture::new();
        let cancel = AtomicBool::new(false);
        let mut payload =
            CapsulePayload::new(&fixture.0, (MAX_READ_BYTES + 7) as u64, (MAX_READ_BYTES + 7) as u64).unwrap();
        payload.push(&vec![b'A'; MAX_READ_BYTES], &cancel).unwrap();
        assert!(payload.file.is_none());
        payload.push(b"1234567", &cancel).unwrap();
        assert_eq!(fixture.count(), 1);
        assert_eq!(payload.memory.capacity(), 0);
        let mut result = Vec::new();
        payload.reader(&cancel).unwrap().read_to_end(&mut result).unwrap();
        assert_eq!(&result[..MAX_READ_BYTES], &vec![b'A'; MAX_READ_BYTES]);
        assert_eq!(&result[MAX_READ_BYTES..], b"1234567");
        payload.retire().unwrap();
        payload.retire().unwrap();
        assert_eq!(fixture.count(), 0);
    }
    #[test]
    fn payload_limits_incomplete_and_cancelled_states_never_read_as_complete() {
        let fixture = Fixture::new();
        let cancel = AtomicBool::new(false);
        assert!(CapsulePayload::new(&fixture.0, 8, 7).is_err());
        let mut payload = CapsulePayload::new(&fixture.0, 2, 2).unwrap();
        payload.push(b"a", &cancel).unwrap();
        assert!(payload.reader(&cancel).is_err());
        assert!(payload.push(b"bc", &cancel).is_err());
        assert!(payload.reader(&cancel).is_err());
        payload.retire().unwrap();
        let mut payload = CapsulePayload::new(&fixture.0, 2, 2).unwrap();
        payload.push(b"ab", &cancel).unwrap();
        assert!(payload.push(b"c", &cancel).is_err());
        assert!(payload.reader(&cancel).is_err(), "a failed append must poison even a previously complete prefix");
        payload.retire().unwrap();
        let mut payload = CapsulePayload::new(&fixture.0, 1, 1).unwrap();
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(payload.push(b"a", &cancel).unwrap_err().kind(), io::ErrorKind::Interrupted);
        cancel.store(false, Ordering::Relaxed);
        assert!(payload.reader(&cancel).is_err());
        payload.retire().unwrap();
        assert_eq!(fixture.count(), 0);
    }
    #[test]
    fn payload_write_failures_preserve_error_and_poll_between_calls() {
        struct Fails<'a> {
            cancel: &'a AtomicBool,
            kind: Option<io::ErrorKind>,
            calls: usize,
        }
        impl Write for Fails<'_> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.calls += 1;
                self.cancel.store(true, Ordering::Relaxed);
                match self.kind {
                    Some(kind) => Err(io::Error::from(kind)),
                    None => Ok(bytes.len().min(1)),
                }
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        for kind in [Some(io::ErrorKind::StorageFull), Some(io::ErrorKind::Interrupted), None] {
            let cancel = AtomicBool::new(false);
            let mut writer = Fails { cancel: &cancel, kind, calls: 0 };
            let error = write_polled(&mut writer, b"abc", &cancel).unwrap_err();
            assert_eq!(error.kind(), kind.unwrap_or(io::ErrorKind::Interrupted));
            assert_eq!(writer.calls, 1);
        }
    }

    #[test]
    fn payload_cleanup_failure_retains_exact_path_for_retry() {
        let fixture = Fixture::new();
        let cancel = AtomicBool::new(false);
        let mut payload =
            CapsulePayload::new(&fixture.0, (MAX_READ_BYTES + 1) as u64, (MAX_READ_BYTES + 1) as u64).unwrap();
        payload.push(&vec![b'A'; MAX_READ_BYTES], &cancel).unwrap();
        payload.push(b"B", &cancel).unwrap();
        let directory = payload.directory.clone().unwrap();
        fs::write(directory.join("blocker"), b"test-owned").unwrap();
        assert!(payload.retire().is_err());
        assert_eq!(payload.directory.as_ref(), Some(&directory));
        assert!(!directory.join("payload").exists());
        fs::remove_file(directory.join("blocker")).unwrap();
        payload.retire().unwrap();
        assert_eq!(fixture.count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn payload_spool_permissions_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let cancel = AtomicBool::new(false);
        let mut payload =
            CapsulePayload::new(&fixture.0, (MAX_READ_BYTES + 1) as u64, (MAX_READ_BYTES + 1) as u64).unwrap();
        payload.push(&vec![b'A'; MAX_READ_BYTES], &cancel).unwrap();
        payload.push(b"B", &cancel).unwrap();
        let directory = payload.directory.as_ref().unwrap();
        assert_eq!(fs::metadata(directory).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(directory.join("payload")).unwrap().permissions().mode() & 0o777, 0o600);
        payload.retire().unwrap();
    }
}
