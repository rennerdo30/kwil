//! Linear root history. Accounting deliberately overestimates shared nodes
//! and resident add blocks, so the cap never depends on a full tree walk.

use crate::buffer::Snapshot;
use std::collections::VecDeque;

struct Entry {
    before: Snapshot,
    after: Snapshot,
    cost: usize,
    model_cost: usize,
}

impl Entry {
    fn new(before: Snapshot, after: Snapshot) -> Self {
        let cost =
            before.retained_bytes().saturating_add(after.retained_bytes()).saturating_add(std::mem::size_of::<Self>());
        Self { before, after, cost, model_cost: 0 }
    }
}

pub(crate) struct History {
    undo: VecDeque<Entry>,
    redo: VecDeque<Entry>,
    bytes: usize,
    limit: usize,
}

/// Capacity and retained-root/model charges prepared on the authority worker.
/// The same History engine installs this without allocating or scanning after
/// the live root changes. Any intervening history policy/edit invalidates epoch.
pub(crate) struct PreparedHistory {
    entry: Entry,
}

impl History {
    pub(crate) fn prepare_worker(
        &mut self,
        before: Snapshot,
        after: Snapshot,
        model_cost: usize,
    ) -> std::io::Result<PreparedHistory> {
        let cost = before
            .retained_bytes()
            .checked_add(after.retained_bytes())
            .and_then(|n| n.checked_add(std::mem::size_of::<Entry>()))
            .and_then(|n| n.checked_add(model_cost))
            .ok_or_else(crate::worker_edit::overflow)?;
        self.bytes.checked_add(cost).ok_or_else(crate::worker_edit::overflow)?;
        self.undo.try_reserve(1).map_err(std::io::Error::other)?;
        Ok(PreparedHistory { entry: Entry { before, after, cost, model_cost } })
    }
    pub(crate) fn install_prepared(&mut self, prepared: PreparedHistory) {
        for entry in self.redo.drain(..) {
            self.bytes -= entry.cost;
        }
        self.bytes += prepared.entry.cost; // overflow prechecked before commit
        self.undo.push_back(prepared.entry); // capacity reserved on THIS history
        self.trim(); // worker-only destruction; existing cap/eviction semantics
    }
    pub fn new(limit: usize) -> Self {
        Self { undo: VecDeque::new(), redo: VecDeque::new(), bytes: 0, limit }
    }

    pub fn record(&mut self, before: Snapshot, after: Snapshot, coalesce: bool) {
        for entry in self.redo.drain(..) {
            self.bytes -= entry.cost;
        }
        let before = if coalesce {
            if let Some(previous) = self.undo.pop_back() {
                self.bytes -= previous.cost;
                previous.before
            } else {
                before
            }
        } else {
            before
        };
        let entry = Entry::new(before, after);
        self.bytes = self.bytes.saturating_add(entry.cost);
        self.undo.push_back(entry);
        self.trim();
    }

    pub fn undo(&mut self) -> Option<Snapshot> {
        let entry = self.undo.pop_back()?;
        let snapshot = entry.before.clone();
        self.redo.push_back(entry);
        Some(snapshot)
    }

    pub fn redo(&mut self) -> Option<Snapshot> {
        let entry = self.redo.pop_back()?;
        let snapshot = entry.after.clone();
        self.undo.push_back(entry);
        Some(snapshot)
    }

    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
        self.trim();
        self.undo.shrink_to_fit();
        self.redo.shrink_to_fit();
    }

    pub fn charge_latest_model(&mut self, cost: usize) {
        if let Some(entry) = self.undo.back_mut() {
            self.bytes = self.bytes.saturating_sub(entry.model_cost).saturating_add(cost);
            entry.cost = entry.cost.saturating_sub(entry.model_cost).saturating_add(cost);
            entry.model_cost = cost;
        }
        self.trim();
    }

    fn trim(&mut self) {
        while self.bytes > self.limit {
            // Evict oldest undo first. Once only redo remains, trim its
            // farthest future: removing the next redo would strand later
            // transitions whose before-state is no longer reachable.
            let entry = self.undo.pop_front().or_else(|| self.redo.pop_front());
            let Some(entry) = entry else { break };
            self.bytes -= entry.cost;
        }
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn depth(&self) -> (usize, usize) {
        (self.undo.len(), self.redo.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Buffer;

    fn snapshot(bytes: &[u8]) -> Snapshot {
        let mut buffer = Buffer::empty();
        buffer.insert(0, bytes).unwrap();
        buffer.snapshot()
    }

    fn bytes(snapshot: &Snapshot) -> Vec<u8> {
        let mut bytes = Vec::new();
        snapshot.write_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn randomized_model_with_cap_shrinking() {
        // Chronological vector plus cursor, independent of the two deques.
        let mut model: Vec<(Vec<u8>, Vec<u8>, usize)> = Vec::new();
        let mut cursor = 0;
        let mut state = Vec::new();
        let mut current = Snapshot::default();
        let mut limit = 16_384;
        let mut history = History::new(limit);
        let mut seed = 0x1618_2026_u64;
        let mut counts = [0; 5];
        for step in 0..3000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let operation = (seed >> 32) as usize % 5;
            counts[operation] += 1;
            match operation {
                0 => {
                    let before = state.clone();
                    state.extend_from_slice(&[step as u8, b'\n']);
                    let after = snapshot(&state);
                    let cost = Entry::new(current.clone(), after.clone()).cost;
                    history.record(current, after.clone(), false);
                    current = after;
                    model.truncate(cursor);
                    model.push((before, state.clone(), cost));
                    cursor += 1;
                }
                1 => {
                    let result = history.undo();
                    assert_eq!(result.is_some(), cursor > 0, "undo at {step}");
                    if let Some(result) = result {
                        cursor -= 1;
                        state = model[cursor].0.clone();
                        current = result;
                    }
                }
                2 => {
                    let result = history.redo();
                    assert_eq!(result.is_some(), cursor < model.len(), "redo at {step}");
                    if let Some(result) = result {
                        state = model[cursor].1.clone();
                        cursor += 1;
                        current = result;
                    }
                }
                3 => {
                    // Shrink while anywhere in history, including all-redo.
                    limit = history.bytes().saturating_sub(1 + (seed as usize % 1024));
                    history.set_limit(limit);
                }
                _ => {
                    limit = 4096 + seed as usize % 16_384;
                    history.set_limit(limit);
                }
            }
            while model.iter().map(|entry| entry.2).sum::<usize>() > limit {
                if cursor > 0 {
                    model.remove(0);
                    cursor -= 1;
                } else {
                    model.pop();
                }
            }
            assert_eq!(bytes(&current), state, "state at {step}");
            assert_eq!(history.bytes(), model.iter().map(|entry| entry.2).sum::<usize>(), "cost at {step}");
            assert!(history.bytes() <= limit);
            assert_eq!(history.undo.len(), cursor, "undo length at {step}");
            assert_eq!(history.redo.len(), model.len() - cursor, "redo length at {step}");
            // Every retained transition must connect to its neighbour and
            // the present state, even before a later undo/redo visits it.
            for (entry, expected) in history.undo.iter().chain(history.redo.iter().rev()).zip(&model) {
                assert_eq!(bytes(&entry.before), expected.0, "before at {step}");
                assert_eq!(bytes(&entry.after), expected.1, "after at {step}");
            }
        }
        assert!(counts.iter().all(|&count| count > 400));
    }
}
