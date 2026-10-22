//! One-shot KW-26 measurements. No benchmark loops. Fixtures stay ignored.
use kwil_core_syntax::{Config, Engine, InputEdit, Language, Point, TextSource};
use std::{
    borrow::Cow,
    fs::File,
    io::{self, BufWriter, Write},
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

struct Disk {
    file: File,
    len: usize,
    read: AtomicUsize,
}
impl Disk {
    fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        Ok(Self { len: file.metadata()?.len() as usize, file, read: AtomicUsize::new(0) })
    }
}
impl TextSource for Disk {
    fn len(&self) -> usize {
        self.len
    }
    fn chunk(&self, offset: usize, max: usize) -> io::Result<Cow<'_, [u8]>> {
        let mut bytes = vec![0; max.min(self.len.saturating_sub(offset))];
        #[cfg(windows)]
        let read = {
            use std::os::windows::fs::FileExt;
            self.file.seek_read(&mut bytes, offset as u64)?
        };
        #[cfg(unix)]
        let read = {
            use std::os::unix::fs::FileExt;
            self.file.read_at(&mut bytes, offset as u64)?
        };
        bytes.truncate(read);
        self.read.fetch_add(read, Ordering::Relaxed);
        Ok(Cow::Owned(bytes))
    }
}
struct Mapped {
    map: memmap2::Mmap,
    read: AtomicUsize,
}
impl TextSource for Mapped {
    fn len(&self) -> usize {
        self.map.len()
    }
    fn chunk(&self, offset: usize, max: usize) -> io::Result<Cow<'_, [u8]>> {
        let end = self.len().min(offset.saturating_add(max));
        self.read.fetch_add(end - offset, Ordering::Relaxed);
        Ok(Cow::Borrowed(&self.map[offset..end]))
    }
}
struct Insert {
    original: Mapped,
    at: usize,
}
impl TextSource for Insert {
    fn len(&self) -> usize {
        self.original.len() + 1
    }
    fn chunk(&self, offset: usize, max: usize) -> io::Result<Cow<'_, [u8]>> {
        if max == 0 {
            return Ok(Cow::Borrowed(b""));
        }
        if offset == self.at {
            return Ok(Cow::Borrowed(b"2"));
        }
        if offset < self.at {
            self.original.chunk(offset, max.min(self.at - offset))
        } else {
            self.original.chunk(offset - 1, max)
        }
    }
}
fn measure_rust(count: usize, normal: bool) -> Result<(), Box<dyn std::error::Error>> {
    let filename = format!("fixtures/syntax-{count}.rs");
    let path = Path::new(&filename);
    let mut file = BufWriter::new(File::create(path)?);
    let mut offset = 0;
    let mut visible = 0..0;
    let mut at = 0;
    let mut column = 0;
    let middle = count / 2;
    let edit_line = middle + usize::from(normal);
    for line in 0..count {
        let text = if normal {
            match line % 10 {
                0 => format!("fn item_{}() {{\n", line / 10),
                9 => "}\n".to_owned(),
                _ => format!("    let value_{} = 1; // sample\n", line % 10),
            }
        } else {
            format!("fn line_{line}() {{ let value = 1; }} // sample\n")
        };
        if line == middle {
            visible.start = offset;
        }
        if line == edit_line {
            column = text.find("1;").unwrap() + 1;
            at = offset + column;
        }
        if line == middle + 60 {
            visible.end = offset;
        }
        file.write_all(text.as_bytes())?;
        offset += text.len();
    }
    file.flush()?;
    drop(file);
    let file = File::open(path)?;
    // SAFETY: this standalone harness exclusively owns the generated fixture;
    // no writer or truncation can occur while the immutable mapping is alive.
    let original = Mapped { map: unsafe { memmap2::Mmap::map(&file)? }, read: AtomicUsize::new(0) };
    let mut engine = Engine::new(Config::default())?;
    let initial = engine.highlight(&original, Language::Rust, 1, None, None, visible.clone(), &|| false)?;
    assert_eq!(initial.mode, kwil_core_syntax::Mode::TreeSitter, "enable lang-rust");
    let first = engine.timings().clone();
    let changed = Insert { original, at };
    let edit = InputEdit {
        start_byte: at,
        old_end_byte: at,
        new_end_byte: at + 1,
        start_position: Point::new(edit_line, column),
        old_end_position: Point::new(edit_line, column),
        new_end_position: Point::new(edit_line, column + 1),
    };
    changed.original.read.store(0, Ordering::Relaxed);
    let now = Instant::now();
    let updated =
        engine
            .highlight(&changed, Language::Rust, 2, Some(1), Some(&edit), visible.start..visible.end + 1, &|| false)?;
    let ms = now.elapsed().as_secs_f64() * 1000.0;
    assert!(updated.incremental);
    println!(
        "{count} Rust lines (normal={normal}), 60-line viewport: total={ms:.3}ms, queried={} bytes, source_read={} bytes, spans={}",
        updated.queried.len(),
        changed.original.read.load(Ordering::Relaxed),
        updated.spans.len()
    );
    let times = engine.timings();
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    println!(
        "first_full_parse={:.3}ms edit={:.3}ms incremental_reparse={:.3}ms query={:.3}ms old_tree_disposal={:.3}ms",
        ms(first.parse),
        ms(times.edit),
        ms(times.parse),
        ms(times.query),
        ms(times.dispose)
    );
    Ok(())
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all("fixtures")?;
    measure_rust(50_000, false)?;
    measure_rust(5_000, true)?;
    let log_path = std::env::args().nth(1).unwrap_or_else(|| "fixtures/log-1g.log".into());
    let huge = Disk::open(Path::new(&log_path))?;
    assert!(huge.len() >= 1 << 30);
    // Read only enough to locate sixty lines near the middle; outside the timer.
    let middle = huge.len() / 2;
    let sample = huge.chunk(middle, 64 * 1024)?;
    let mut lines = sample.iter().enumerate().filter(|(_, b)| **b == b'\n');
    let start = middle + lines.next().ok_or("fixture needs lines")?.0 + 1;
    let end = middle + lines.nth(59).ok_or("fixture needs sixty lines")?.0 + 1;
    huge.read.store(0, Ordering::Relaxed);
    // A fresh engine keeps unrelated previous-tree disposal outside this timer.
    let mut engine = Engine::new(Config::default())?;
    let now = Instant::now();
    let h = engine.highlight(&huge, Language::Rust, 3, None, None, start..end, &|| false)?;
    let ms = now.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(h.mode, kwil_core_syntax::Mode::Lexical);
    println!(
        "1GB log, 60-line viewport: lexical={ms:.3}ms, queried={} bytes, source_read={} bytes, spans={}",
        h.queried.len(),
        huge.read.load(Ordering::Relaxed),
        h.spans.len()
    );
    Ok(())
}
