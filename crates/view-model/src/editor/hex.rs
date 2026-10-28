use super::*;

impl Editor {
    /// Explicit byte-mode operation. Unlike text input this deliberately
    /// permits invalid UTF-8, while retaining normal revision and undo rules.
    pub fn replace_byte_range(&mut self, range: Range<u64>, bytes: &[u8]) -> io::Result<Damage> {
        self.document.snapshot().chunks(range.clone())?;
        self.end_burst();
        self.apply(vec![Edit::bytes(range, bytes)], None, true)
    }
}
