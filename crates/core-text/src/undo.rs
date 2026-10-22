//! Linear root history. Accounting deliberately overestimates shared nodes
//! and resident add blocks, so the cap never depends on a full tree walk.

use crate::buffer::Snapshot;
use std::collections::VecDeque;

struct Entry {
    before: Snapshot,
    after: Snapshot,
    cost: usize,
}

impl Entry {
    fn new(before: Snapshot, after: Snapshot) -> Self {
        let cost =
            before.retained_bytes().saturating_add(after.retained_bytes()).saturating_add(std::mem::size_of::<Self>());
        Self { before, after, cost }
    }
}

pub(crate) struct History {
    undo: VecDeque<Entry>,
    redo: VecDeque<Entry>,
    bytes: usize,
    limit: usize,
}

impl History {
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

    fn trim(&mut self) {
        while self.bytes > self.limit {
            // The front of undo is oldest. After undoing all the way, the
            // back of redo is the earliest remaining chronological change.
            let entry = self.undo.pop_front().or_else(|| self.redo.pop_back());
            let Some(entry) = entry else { break };
            self.bytes -= entry.cost;
        }
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }
}
