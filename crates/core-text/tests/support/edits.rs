use kwil_core_text::{Buffer, LineIndex, Snapshot};
use std::sync::atomic::AtomicU64;

pub fn starts(bytes: &[u8]) -> Vec<u64> {
    std::iter::once(0)
        .chain(bytes.iter().enumerate().filter(|(_, b)| **b == b'\n').map(|(i, _)| i as u64 + 1))
        .collect()
}

pub fn check_snapshot(snapshot: &Snapshot, text: &str) {
    let mut actual = Vec::new();
    snapshot.write_to(&mut actual).unwrap();
    assert_eq!(actual, text.as_bytes());
    assert_eq!(snapshot.len(), text.len() as u64);
    let lines = starts(text.as_bytes());
    assert_eq!(snapshot.line_count(), lines.len() as u64);
    for (line, &start) in lines.iter().enumerate() {
        assert_eq!(snapshot.line_start(line as u64).unwrap(), Some(start));
    }
    assert_eq!(snapshot.line_start(lines.len() as u64).unwrap(), None);
    for offset in 0..=text.len() {
        let line = lines.partition_point(|&start| start <= offset as u64) - 1;
        assert_eq!(
            snapshot.offset_to_line_col(offset as u64).unwrap(),
            Some((line as u64, offset as u64 - lines[line]))
        );
    }
    assert_eq!(snapshot.offset_to_line_col(text.len() as u64 + 1).unwrap(), None);
    let actual: Vec<Vec<u8>> = snapshot
        .lines(0, lines.len() + 1)
        .map(|line| line.unwrap().flat_map(|chunk| chunk.unwrap().to_vec()).collect())
        .collect();
    let expected: Vec<Vec<u8>> =
        text.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line).as_bytes().to_vec()).collect();
    assert_eq!(actual, expected);
}

pub fn check_index(bytes: &[u8], every: u64, split: usize) {
    let split = split.min(bytes.len());
    let mut index = LineIndex::build(&bytes[..split], every, 1, &AtomicU64::new(0)).unwrap();
    index.extend(bytes, split as u64).unwrap();
    let fresh = LineIndex::build(bytes, every, 1, &AtomicU64::new(0)).unwrap();
    let lines = starts(bytes);
    assert_eq!(index.line_count(), lines.len() as u64);
    assert_eq!(fresh.line_count(), index.line_count());
    for (line, &start) in lines.iter().enumerate() {
        assert_eq!(index.line_start(bytes, line as u64), Some(start));
        assert_eq!(fresh.line_start(bytes, line as u64), Some(start));
    }
    assert_eq!(index.line_start(bytes, lines.len() as u64), None);
    for offset in 0..=bytes.len() {
        let line = lines.partition_point(|&start| start <= offset as u64) - 1;
        assert_eq!(index.line_of_offset(bytes, offset as u64), Some(line as u64));
        assert_eq!(fresh.line_of_offset(bytes, offset as u64), Some(line as u64));
    }
    assert_eq!(index.line_of_offset(bytes, bytes.len() as u64 + 1), None);
}

// Select scalar boundaries independently of the byte-oriented piece tree.
fn boundary(text: &str, selector: u16) -> usize {
    let offsets: Vec<_> = text.char_indices().map(|(i, _)| i).chain(std::iter::once(text.len())).collect();
    offsets[selector as usize % offsets.len()]
}

pub fn edit_sequence(initial: &str, ops: &[(u8, u16, u16, String)]) {
    let mut buffer = Buffer::empty();
    buffer.insert(0, initial.as_bytes()).unwrap();
    // The generated history begins after loading the initial model.
    buffer.set_undo_memory_limit(0);
    buffer.set_undo_memory_limit(16 << 20);
    let mut model = initial.to_owned();
    let mut undo = Vec::new();
    let mut redo = Vec::new();
    let mut held = Vec::new();
    check_snapshot(&buffer.snapshot(), &model);
    for (kind, a, b, replacement) in ops {
        match kind % 5 {
            0..=2 => {
                let a = boundary(&model, *a);
                let b = boundary(&model, *b);
                let range = a.min(b)..a.max(b);
                let before = model.clone();
                let changed = (kind % 5 != 0 && !range.is_empty()) || (kind % 5 != 1 && !replacement.is_empty());
                buffer.begin();
                if kind % 5 != 0 {
                    buffer.delete(range.start as u64..range.end as u64).unwrap();
                    model.replace_range(range.clone(), "");
                }
                if kind % 5 != 1 {
                    buffer.insert(range.start as u64, replacement.as_bytes()).unwrap();
                    model.insert_str(range.start, replacement);
                }
                buffer.commit();
                if changed {
                    undo.push(before);
                    redo.clear();
                }
            }
            3 => {
                let expected = undo.pop();
                assert_eq!(buffer.undo(), expected.is_some());
                if let Some(previous) = expected {
                    redo.push(std::mem::replace(&mut model, previous));
                }
            }
            _ => {
                let expected = redo.pop();
                assert_eq!(buffer.redo(), expected.is_some());
                if let Some(next) = expected {
                    undo.push(std::mem::replace(&mut model, next));
                }
            }
        }
        assert_eq!(buffer.history_depth(), (undo.len(), redo.len()));
        check_snapshot(&buffer.snapshot(), &model);
        check_index(model.as_bytes(), 3, model.len() / 2);
        // Keep a few old versions to check persistent snapshots after later edits.
        if held.len() < 4 {
            held.push((buffer.snapshot(), model.clone()));
        }
        for (snapshot, text) in &held {
            check_snapshot(snapshot, text);
        }
    }
    while let Some(previous) = undo.pop() {
        assert!(buffer.undo());
        redo.push(std::mem::replace(&mut model, previous));
        check_snapshot(&buffer.snapshot(), &model);
    }
    assert_eq!(model, initial);
    while let Some(next) = redo.pop() {
        assert!(buffer.redo());
        model = next;
        check_snapshot(&buffer.snapshot(), &model);
    }
}
