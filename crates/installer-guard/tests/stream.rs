//! File-backed regressions for the SAME bounded stream adapter used by MSI.
//! Authored, NOT RUN by the source-only implementation job.
use crate::{
    Deadline, Refusal, Result,
    pe::{self, ReadAt},
    require,
    stream::{CHUNK, Sequential, Window},
};
use std::{
    cell::Cell,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    directory: PathBuf,
    path: PathBuf,
}
impl Fixture {
    fn new(size: u64) -> Self {
        // Large fixtures stay in the repository's ignored fixtures directory.
        // This bounded generator writes only metadata/name records and sets the
        // file length; it never constructs the 16 MiB image in memory.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().join("fixtures");
        std::fs::create_dir_all(&root).unwrap();
        let serial = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let directory = root.join(format!("installer-guard-stream-{}-{stamp}-{serial}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("guard.bin");
        let file = OpenOptions::new().write(true).create_new(true).open(&path).unwrap();
        file.set_len(size).unwrap();
        Self { directory, path }
    }
    fn write_at(&self, offset: u64, bytes: &[u8]) {
        let mut file = OpenOptions::new().write(true).open(&self.path).unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(bytes).unwrap();
    }
    fn truncate(&self, size: u64) {
        OpenOptions::new().write(true).open(&self.path).unwrap().set_len(size).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Only this create-new file/directory, never recursive owner cleanup.
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

struct CountingFile {
    path: PathBuf,
    file: File,
    size: u64,
    bytes: u64,
    reads: usize,
    rewinds: usize,
    max_read: usize,
    byte_limit: u64,
    expired: Cell<bool>,
    deadline: Deadline,
}
impl CountingFile {
    fn open(path: &Path) -> Self {
        let file = File::open(path).unwrap();
        let size = file.metadata().unwrap().len();
        Self {
            path: path.to_owned(),
            file,
            size,
            bytes: 0,
            reads: 0,
            rewinds: 0,
            max_read: 0,
            byte_limit: size + 4 * CHUNK as u64,
            expired: Cell::new(false),
            deadline: Deadline::new(),
        }
    }
}
impl Sequential for CountingFile {
    fn len(&self) -> u64 {
        self.size
    }
    fn check(&self) -> Result<()> {
        require(!self.expired.get(), "deadline", "injected fixture deadline")?;
        self.deadline.check()
    }
    fn read(&mut self, bytes: &mut [u8]) -> Result<usize> {
        self.check()?;
        // Test-only oracle: a regressed rewind loop fails promptly instead of
        // turning this focused regression into gigabytes of local I/O.
        require(self.bytes + bytes.len() as u64 <= self.byte_limit, "fixture-work", "linear stream read budget")?;
        self.max_read = self.max_read.max(bytes.len());
        self.reads += 1;
        let count = self.file.read(bytes).map_err(|_| Refusal::new("stream-fixture", "file read failed"))?;
        self.bytes += count as u64;
        Ok(count)
    }
    fn rewind(&mut self) -> Result<()> {
        self.check()?;
        self.rewinds += 1;
        let file = File::open(&self.path).map_err(|_| Refusal::new("stream-fixture", "same-path reopen failed"))?;
        let size = file.metadata().map_err(|_| Refusal::new("stream-fixture", "same-path metadata failed"))?.len();
        require(size == self.size, "unstable-evidence", "same-path source resized")?;
        self.file = file;
        Ok(())
    }
}

fn put16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}
fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
fn rva(raw: u64) -> u32 {
    u32::try_from(raw - 0x200 + 0x1000).unwrap()
}

fn far_name_pe() -> Fixture {
    const IMAGE: u64 = 16_777_216;
    let fixture = Fixture::new(IMAGE);
    let mut header = [0u8; 4096];
    header[..2].copy_from_slice(b"MZ");
    put32(&mut header, 60, 128);
    header[128..132].copy_from_slice(b"PE\0\0");
    put16(&mut header, 132, 0x8664);
    put16(&mut header, 134, 1);
    put16(&mut header, 148, 240);
    put16(&mut header, 150, 0x2000);
    put16(&mut header, 152, 0x20b);
    put32(&mut header, 260, 16);
    put32(&mut header, 400, (IMAGE - 0x200) as u32);
    put32(&mut header, 404, 0x1000);
    put32(&mut header, 408, (IMAGE - 0x200) as u32);
    put32(&mut header, 412, 0x200);
    put32(&mut header, 264, 0x1200);
    put32(&mut header, 268, 0x100);
    put32(&mut header, 0x414, 1);
    put32(&mut header, 0x418, 1);
    put32(&mut header, 0x41c, 0x1280);
    put32(&mut header, 0x420, 0x1284);
    put32(&mut header, 0x424, 0x1288);
    put32(&mut header, 0x480, 0x1300);
    put32(&mut header, 0x484, 0x12a0);
    header[0x4a0..0x4b0].copy_from_slice(b"KwilPreflightV1\0");
    header[0x500] = 0xc3;
    put32(&mut header, 272, rva(CHUNK as u64));
    put32(&mut header, 276, 65 * 20);
    put32(&mut header, 368, rva(2 * CHUNK as u64));
    put32(&mut header, 372, 65 * 32);
    fixture.write_at(0, &header);
    let mut normal = [0u8; 65 * 20];
    let mut delay = [0u8; 65 * 32];
    for index in 0..64 {
        // Descending, interleaved references defeat descriptor/name alternation
        // and defeat a forward cursor without the offset-ordered name phase.
        let n = 127 - 2 * index;
        let d = 126 - 2 * index;
        put32(&mut normal, index * 20 + 12, rva(8_388_608 + n as u64 * CHUNK as u64 + 4093));
        put32(&mut delay, index * 32, 1);
        put32(&mut delay, index * 32 + 4, rva(8_388_608 + d as u64 * CHUNK as u64 + 4093));
    }
    fixture.write_at(CHUNK as u64, &normal);
    fixture.write_at(2 * CHUNK as u64, &delay);
    for index in 0..128 {
        let name = format!("os{index:03}.dll\0");
        fixture.write_at(8_388_608 + index * CHUNK as u64 + 4093, name.as_bytes());
    }
    fixture
}

#[test]
fn far_separated_names_bound_real_sequential_adapter_reads() {
    let fixture = far_name_pe();
    let mut input = Window::new(CountingFile::open(&fixture.path)).unwrap();
    let imports = pe::inspect(&mut input, "x64").unwrap();
    assert_eq!(imports.len(), 128);
    assert_eq!(imports.iter().filter(|i| i.load == "N").count(), 64);
    assert_eq!(imports.iter().filter(|i| i.load == "D").count(), 64);
    for index in 0..128usize {
        let name = format!("os{index:03}.dll");
        assert!(imports.iter().any(|i| i.dll == name && i.load == if index.is_multiple_of(2) { "D" } else { "N" }));
    }
    let observed = input.source();
    // Real File::read byte counts, including all skipped bytes and reopens.
    // Old cache-miss restarts OR old descriptor/name alternation violate this.
    assert!(
        observed.bytes <= observed.size + 4 * CHUNK as u64,
        "{} bytes across {} reads",
        observed.bytes,
        observed.reads
    );
    assert!(observed.rewinds <= 1, "{} source rewinds", observed.rewinds);
    assert!(observed.max_read <= CHUNK);
}

#[test]
fn sequential_adapter_reuses_forward_cursor_and_copies_across_windows() {
    let fixture = Fixture::new(3 * CHUNK as u64);
    fixture.write_at(CHUNK as u64 - 2, &[1, 2, 3, 4]);
    let mut input = Window::new(CountingFile::open(&fixture.path)).unwrap();
    let mut output = [0; 4];
    input.read_at(CHUNK as u64 - 2, &mut output).unwrap();
    assert_eq!(output, [1, 2, 3, 4]);
    input.read_at(2 * CHUNK as u64, &mut output[..1]).unwrap();
    assert_eq!(input.source().bytes, 3 * CHUNK as u64);
    assert_eq!(input.source().rewinds, 0);
    input.read_at(0, &mut output[..1]).unwrap();
    assert_eq!(input.source().rewinds, 1);
    assert_eq!(input.source().bytes, 4 * CHUNK as u64);
}

#[test]
fn sequential_adapter_short_read_failure_cannot_reuse_an_old_cache() {
    let fixture = Fixture::new(3 * CHUNK as u64);
    let mut input = Window::new(CountingFile::open(&fixture.path)).unwrap();
    let mut byte = [0];
    input.read_at(0, &mut byte).unwrap();
    fixture.truncate(0);
    let error = input.read_at(2 * CHUNK as u64, &mut byte).unwrap_err();
    assert_eq!(error.code, "unstable-evidence");
    let reads = input.source().reads;
    assert_eq!(input.read_at(0, &mut byte).unwrap_err(), error);
    assert_eq!(input.source().reads, reads);
}

#[test]
fn sequential_adapter_reopen_pins_same_path_and_length() {
    let fixture = Fixture::new(3 * CHUNK as u64);
    let mut input = Window::new(CountingFile::open(&fixture.path)).unwrap();
    let mut byte = [0];
    input.read_at(2 * CHUNK as u64, &mut byte).unwrap();
    fixture.truncate(2 * CHUNK as u64);
    let error = input.read_at(0, &mut byte).unwrap_err();
    assert_eq!(error.resource, "same-path source resized");
    let reads = input.source().reads;
    assert_eq!(input.read_at(2 * CHUNK as u64, &mut byte).unwrap_err(), error);
    assert_eq!(input.source().reads, reads);
}

#[test]
fn sequential_adapter_missing_same_path_cannot_fall_back_to_an_old_handle() {
    let mut fixture = Fixture::new(3 * CHUNK as u64);
    let mut input = Window::new(CountingFile::open(&fixture.path)).unwrap();
    let mut byte = [0];
    input.read_at(2 * CHUNK as u64, &mut byte).unwrap();
    let relocated = fixture.directory.join("relocated.bin");
    std::fs::rename(&fixture.path, &relocated).unwrap();
    fixture.path = relocated; // Cleanup still names only this test's own file.
    let error = input.read_at(0, &mut byte).unwrap_err();
    assert_eq!(error.resource, "same-path reopen failed");
    let reads = input.source().reads;
    assert_eq!(input.read_at(2 * CHUNK as u64, &mut byte).unwrap_err(), error);
    assert_eq!(input.source().reads, reads);
}

#[test]
fn sequential_adapter_checks_deadline_even_on_cache_hits_and_keeps_the_failure() {
    let fixture = Fixture::new(CHUNK as u64);
    let mut input = Window::new(CountingFile::open(&fixture.path)).unwrap();
    let mut byte = [0];
    input.read_at(0, &mut byte).unwrap();
    input.source().expired.set(true);
    let error = input.read_at(1, &mut byte).unwrap_err();
    assert_eq!(error.code, "deadline");
    input.source().expired.set(false);
    assert_eq!(input.read_at(2, &mut byte).unwrap_err(), error);
    assert_eq!(input.source().rewinds, 0);
    assert_eq!(input.source().bytes, CHUNK as u64);
}

fn overlapping_name_pe(cross_section: bool) -> Fixture {
    let fixture = Fixture::new(16_777_216);
    let first_raw = if cross_section { 0x1000 } else { 0x7ff000 };
    let tail_raw = if cross_section { 0xfff000 } else { 0x800000 };
    let backed = if cross_section { 0x1000 } else { 0x2000 };
    let mut header = [0u8; 4096];
    header[..2].copy_from_slice(b"MZ");
    put32(&mut header, 60, 128);
    header[128..132].copy_from_slice(b"PE\0\0");
    put16(&mut header, 132, 0x8664);
    put16(&mut header, 134, if cross_section { 2 } else { 1 });
    put16(&mut header, 148, 240);
    put16(&mut header, 150, 0x2000);
    put16(&mut header, 152, 0x20b);
    put32(&mut header, 260, 16);
    put32(&mut header, 264, 0x1b00);
    put32(&mut header, 268, 0x100);
    put32(&mut header, 272, 0x1000);
    put32(&mut header, 276, 125 * 20);
    put32(&mut header, 400, backed);
    put32(&mut header, 404, 0x1000);
    put32(&mut header, 408, backed);
    put32(&mut header, 412, first_raw);
    if cross_section {
        put32(&mut header, 440, 0x1000);
        put32(&mut header, 444, 0x2000);
        put32(&mut header, 448, 0x1000);
        put32(&mut header, 452, tail_raw);
    }
    let mut low = [0u8; 4096];
    for index in 0..124 {
        put32(&mut low, index * 20 + 12, 0x1f84 + index as u32);
    }
    put32(&mut low, 0xb14, 1);
    put32(&mut low, 0xb18, 1);
    put32(&mut low, 0xb1c, 0x1b80);
    put32(&mut low, 0xb20, 0x1b84);
    put32(&mut low, 0xb24, 0x1b88);
    put32(&mut low, 0xb80, 0x1c00);
    put32(&mut low, 0xb84, 0x1ba0);
    low[0xba0..0xbb0].copy_from_slice(b"KwilPreflightV1\0");
    low[0xc00] = 0xc3;
    low[0xf84..].fill(b'a');
    // The exact review case: 124 overlapping starts 1F84..1FFF, then .dll/NUL
    // at RVA 2000. Only the sparse file's metadata and these small ranges exist
    // in process memory; the second mapping may sit at the last physical page.
    fixture.write_at(0, &header);
    fixture.write_at(first_raw as u64, &low);
    fixture.write_at(tail_raw as u64, b".dll\0");
    fixture
}

#[test]
fn overlapping_cross_section_names_refuse_before_advancing_to_the_last_window() {
    let fixture = overlapping_name_pe(true);
    let mut source = CountingFile::open(&fixture.path);
    // A broken parser fails this oracle after at most two windows, rather than
    // recreating the review's 1984 MiB / 123-rewind load on the owner's PC.
    source.byte_limit = 2 * CHUNK as u64;
    let mut input = Window::new(source).unwrap();
    let error = pe::inspect(&mut input, "x64").unwrap_err();
    assert_eq!(error.code, "invalid-pe");
    assert_eq!(error.resource, "name crosses physical section");
    let observed = input.source();
    assert_eq!(observed.bytes, CHUNK as u64);
    assert_eq!(observed.reads, 1);
    assert_eq!(observed.rewinds, 0);
    assert!(observed.max_read <= CHUNK);
}

#[test]
fn name_terminating_at_its_section_boundary_still_passes() {
    let fixture = overlapping_name_pe(true);
    let mut descriptors = [0u8; 40];
    put32(&mut descriptors, 12, 0x1ffa);
    fixture.write_at(0x1000, &descriptors);
    fixture.write_at(276, &40u32.to_le_bytes());
    fixture.write_at(0x1ffa, b"a.dll\0");
    let mut source = CountingFile::open(&fixture.path);
    source.byte_limit = 2 * CHUNK as u64;
    let mut input = Window::new(source).unwrap();
    assert_eq!(
        pe::inspect(&mut input, "x64").unwrap(),
        vec![crate::model::Import { load: "N".into(), dll: "a.dll".into() }]
    );
    assert_eq!(input.source().bytes, CHUNK as u64);
    assert_eq!(input.source().rewinds, 0);
}

#[test]
fn overlapping_single_section_names_do_not_rewind_at_a_cache_page_boundary() {
    let fixture = overlapping_name_pe(false);
    let mut source = CountingFile::open(&fixture.path);
    source.byte_limit = 8_388_608 + 2 * CHUNK as u64;
    let mut input = Window::new(source).unwrap();
    let imports = pe::inspect(&mut input, "x64").unwrap();
    assert_eq!(imports.len(), 124);
    for length in 1..=124 {
        let expected = format!("{}.dll", "a".repeat(length));
        assert!(imports.iter().any(|import| import.load == "N" && import.dll == expected));
    }
    assert!(input.source().bytes <= 8_388_608 + CHUNK as u64);
    assert_eq!(input.source().rewinds, 0);
    assert!(input.source().max_read <= CHUNK);
}

fn alternating_descriptor_pe(load: &str) -> Fixture {
    let fixture = Fixture::new(16_777_216);
    let width = if load == "N" { 20usize } else { 32 };
    let mut header = [0u8; 8192];
    header[..2].copy_from_slice(b"MZ");
    put32(&mut header, 60, 128);
    header[128..132].copy_from_slice(b"PE\0\0");
    put16(&mut header, 132, 0x8664);
    put16(&mut header, 134, 94);
    put16(&mut header, 148, 240);
    put16(&mut header, 150, 0x2000);
    put16(&mut header, 152, 0x20b);
    put32(&mut header, 260, 16);
    put32(&mut header, 264, 0x1100);
    put32(&mut header, 268, 0x100);
    let directory = if load == "N" { 272 } else { 368 };
    put32(&mut header, directory, 0x2000);
    put32(&mut header, directory + 4, (93 * width) as u32);
    // One shared export/name section, followed by 93 tiny descriptor mappings.
    put32(&mut header, 400, 0x1000);
    put32(&mut header, 404, 0x1000);
    put32(&mut header, 408, 0x1000);
    put32(&mut header, 412, 0x2000);
    for index in 0..93usize {
        let raw = if index.is_multiple_of(2) { 0x4000 } else { 0xfff000 } + width * (index / 2);
        let section = 392 + (index + 1) * 40;
        put32(&mut header, section + 8, width as u32);
        put32(&mut header, section + 12, 0x2000 + (index * width) as u32);
        put32(&mut header, section + 16, width as u32);
        put32(&mut header, section + 20, raw as u32);
        let mut descriptor = [0; 32];
        if index < 92 {
            if load == "D" {
                put32(&mut descriptor, 0, 1);
            }
            put32(&mut descriptor, if load == "N" { 12 } else { 4 }, 0x1300 + (index * 12) as u32);
        }
        fixture.write_at(raw as u64, &descriptor[..width]);
    }
    let mut shared = [0u8; 4096];
    put32(&mut shared, 0x114, 1);
    put32(&mut shared, 0x118, 1);
    put32(&mut shared, 0x11c, 0x1180);
    put32(&mut shared, 0x120, 0x1184);
    put32(&mut shared, 0x124, 0x1188);
    put32(&mut shared, 0x180, 0x1200);
    put32(&mut shared, 0x184, 0x11a0);
    shared[0x1a0..0x1b0].copy_from_slice(b"KwilPreflightV1\0");
    shared[0x200] = 0xc3;
    for index in 0..92 {
        let name = format!("n{index:03}.dll\0");
        let at = 0x300 + index * 12;
        shared[at..at + name.len()].copy_from_slice(name.as_bytes());
    }
    fixture.write_at(0, &header);
    fixture.write_at(0x2000, &shared);
    fixture
}

#[test]
fn complete_descriptor_directories_refuse_alternating_mappings_before_iteration() {
    for load in ["N", "D"] {
        let fixture = alternating_descriptor_pe(load);
        let mut source = CountingFile::open(&fixture.path);
        // The old 94-section case would replay hundreds of MiB. The test's
        // own two-window ceiling makes any regression fail without that load.
        source.byte_limit = 2 * CHUNK as u64;
        let mut input = Window::new(source).unwrap();
        let error = pe::inspect(&mut input, "x64").unwrap_err();
        assert_eq!(error.code, "invalid-pe");
        assert_eq!(error.resource, "RVA outside backed section");
        assert_eq!(input.source().bytes, CHUNK as u64);
        assert_eq!(input.source().reads, 1);
        assert_eq!(input.source().rewinds, 0);
    }
}

#[derive(Clone, Copy)]
enum BoundaryTable {
    Normal,
    Delay,
    ExportDirectory,
    ExportFunctions,
    ExportNames,
    ExportOrdinals,
}

fn write_boundary_bytes(fixture: &Fixture, rva: u64, bytes: &[u8]) {
    let first = bytes.len().min((0x2000 - rva) as usize);
    fixture.write_at(rva, &bytes[..first]);
    if first < bytes.len() {
        fixture.write_at(0xfff000, &bytes[first..]);
    }
}

fn range_boundary_pe(table: BoundaryTable, crosses: bool) -> Fixture {
    let fixture = overlapping_name_pe(true);
    let mut normal = [0; 40];
    put32(&mut normal, 12, 0x1d00);
    fixture.write_at(0x1000, &normal);
    fixture.write_at(276, &40u32.to_le_bytes());
    fixture.write_at(0x1d00, b"a.dll\0");
    match table {
        BoundaryTable::Normal | BoundaryTable::Delay => {
            let delay = matches!(table, BoundaryTable::Delay);
            let width = if delay { 32usize } else { 20 };
            let start = 0x2000 - 2 * width;
            let mut descriptors = [0; 64];
            if delay {
                put32(&mut descriptors, 0, 1);
            }
            put32(&mut descriptors, if delay { 4 } else { 12 }, 0x1d00);
            fixture.write_at(start as u64, &descriptors[..2 * width]);
            fixture.write_at(272, &[0; 8]);
            let field = if delay { 368 } else { 272 };
            let mut directory = [0; 8];
            put32(&mut directory, 0, start as u32);
            put32(&mut directory, 4, (2 * width + usize::from(crosses)) as u32);
            fixture.write_at(field, &directory);
        }
        BoundaryTable::ExportDirectory => {
            let mut directory = [0; 40];
            put32(&mut directory, 20, 1);
            put32(&mut directory, 24, 1);
            put32(&mut directory, 28, 0x1b80);
            put32(&mut directory, 32, 0x1b84);
            put32(&mut directory, 36, 0x1b88);
            fixture.write_at(0x1fd8, &directory);
            fixture.write_at(264, &0x1fd8u32.to_le_bytes());
            fixture.write_at(268, &(40 + u32::from(crosses)).to_le_bytes());
        }
        BoundaryTable::ExportFunctions | BoundaryTable::ExportNames | BoundaryTable::ExportOrdinals => {
            let (field, width, value) = match table {
                BoundaryTable::ExportFunctions => (0x1b1c, 4, 0x1c00u32),
                BoundaryTable::ExportNames => (0x1b20, 4, 0x1ba0u32),
                BoundaryTable::ExportOrdinals => (0x1b24, 2, 0u32),
                _ => unreachable!(),
            };
            let start = 0x2000 - width + u64::from(crosses);
            fixture.write_at(field, &(start as u32).to_le_bytes());
            write_boundary_bytes(&fixture, start, &value.to_le_bytes()[..width as usize]);
        }
    }
    fixture
}

fn assert_boundary_fixture(table: BoundaryTable, crosses: bool, load: &str) {
    let fixture = range_boundary_pe(table, crosses);
    let mut source = CountingFile::open(&fixture.path);
    source.byte_limit = 2 * CHUNK as u64;
    let mut input = Window::new(source).unwrap();
    let result = pe::inspect(&mut input, "x64");
    if crosses {
        let error = result.unwrap_err();
        assert_eq!(error.code, "invalid-pe");
        assert_eq!(error.resource, "RVA outside backed section");
    } else {
        assert_eq!(result.unwrap(), vec![crate::model::Import { load: load.into(), dll: "a.dll".into() }]);
    }
    assert_eq!(input.source().bytes, CHUNK as u64);
    assert_eq!(input.source().rewinds, 0);
}

#[test]
fn normal_and_delay_complete_directory_ranges_accept_boundary_and_refuse_one_byte_crossing() {
    for crosses in [false, true] {
        assert_boundary_fixture(BoundaryTable::Normal, crosses, "N");
        assert_boundary_fixture(BoundaryTable::Delay, crosses, "D");
    }
}

#[test]
fn all_export_fixed_ranges_accept_boundary_and_refuse_one_byte_crossing() {
    for table in [
        BoundaryTable::ExportDirectory,
        BoundaryTable::ExportFunctions,
        BoundaryTable::ExportNames,
        BoundaryTable::ExportOrdinals,
    ] {
        for crosses in [false, true] {
            assert_boundary_fixture(table, crosses, "N");
        }
    }
}

#[test]
fn complete_range_rejects_partial_mapping_overlap_before_walking_rows() {
    let fixture = range_boundary_pe(BoundaryTable::Normal, false);
    fixture.write_at(134, &3u16.to_le_bytes());
    let mut section = [0; 40];
    put32(&mut section, 8, 20);
    put32(&mut section, 12, 0x1fec);
    put32(&mut section, 16, 20);
    put32(&mut section, 20, 0xfff100);
    fixture.write_at(472, &section);
    let mut source = CountingFile::open(&fixture.path);
    source.byte_limit = 2 * CHUNK as u64;
    let mut input = Window::new(source).unwrap();
    let error = pe::inspect(&mut input, "x64").unwrap_err();
    assert_eq!(error.code, "invalid-pe");
    assert_eq!(error.resource, "ambiguous section mapping");
    assert_eq!(input.source().bytes, CHUNK as u64);
    assert_eq!(input.source().rewinds, 0);
}
