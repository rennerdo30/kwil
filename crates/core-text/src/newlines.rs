//! Logical terminator ends. CRLF is one break, completed at LF; a lone CR
//! or LF completes at that byte. The full slice supplies boundary context.
pub(crate) fn ends(bytes: &[u8], range: std::ops::Range<usize>) -> impl Iterator<Item = usize> + '_ {
    let start = range.start;
    memchr::memchr2_iter(b'\r', b'\n', &bytes[range]).filter_map(move |pos| {
        let at = start + pos;
        (bytes[at] == b'\n' || bytes.get(at + 1) != Some(&b'\n')).then_some(at + 1)
    })
}

pub(crate) fn split_crlf(bytes: &[u8], at: usize) -> bool {
    at > 0 && bytes.get(at - 1) == Some(&b'\r') && bytes.get(at) == Some(&b'\n')
}

pub(crate) fn edges(bytes: &[u8]) -> (bool, bool) {
    (bytes.first() == Some(&b'\n'), bytes.last() == Some(&b'\r'))
}
