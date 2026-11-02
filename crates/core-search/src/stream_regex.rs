//! Boolean regex matching without a line-sized buffer or a maximum match length.
//! Thompson states survive input windows; four context bytes on either side
//! preserve Unicode word assertions, CRLF and anchors at window boundaries.
use crate::Haystack;
use regex_automata::{
    nfa::thompson::{self, NFA, State},
    util::{primitives::StateID, syntax},
};
use std::io;

const WINDOW: usize = (64 << 10) + 8;

pub struct StreamRegex {
    nfa: NFA,
}

impl StreamRegex {
    pub fn new(expression: &str) -> io::Result<Self> {
        let nfa = NFA::compiler()
            .configure(thompson::Config::new().utf8(false).nfa_size_limit(Some(8 << 20)))
            .syntax(syntax::Config::new().utf8(false).crlf(true))
            .build(expression)
            .map_err(io::Error::other)?;
        Ok(Self { nfa })
    }

    /// Workspace workers supply cancellation. Storage is bounded by the compiled
    /// pattern and a 64 KiB window, independently of input and match length.
    pub fn is_match(&self, source: &(impl Haystack + ?Sized), cancelled: impl Fn() -> bool) -> io::Result<bool> {
        let mut machine = Machine::new(&self.nfa);
        let mut window = Vec::with_capacity(WINDOW);
        let mut at = 0;
        for chunk in source.chunks(0..source.len())? {
            let (_, chunk) = chunk?;
            let mut bytes = chunk.as_ref();
            while !bytes.is_empty() {
                if cancelled() {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                let take = bytes.len().min(WINDOW - window.len());
                window.extend_from_slice(&bytes[..take]);
                bytes = &bytes[take..];
                if window.len() == WINDOW {
                    let end = window.len() - 4;
                    for pos in at..end {
                        if machine.step(&window, pos) {
                            return Ok(true);
                        }
                    }
                    // Four consumed bytes of lookbehind and four pending bytes
                    // of lookahead; neither is an artificial beginning or end.
                    window.copy_within(end - 4.., 0);
                    window.truncate(8);
                    at = 4;
                }
            }
        }
        if cancelled() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        // Includes the real EOF position, so $, \z and empty matches work.
        Ok((at..=window.len()).any(|pos| machine.step(&window, pos)))
    }
}

struct Machine<'a> {
    nfa: &'a NFA,
    seeds: Vec<StateID>,
    stack: Vec<StateID>,
    seen: Vec<u64>,
    generation: u64,
}
impl<'a> Machine<'a> {
    fn new(nfa: &'a NFA) -> Self {
        let count = nfa.states().len();
        Self {
            nfa,
            seeds: Vec::with_capacity(count),
            stack: Vec::with_capacity(count),
            seen: vec![0; count],
            generation: 0,
        }
    }
    fn push(&mut self, state: StateID) {
        if self.seen[state.as_usize()] != self.generation {
            self.seen[state.as_usize()] = self.generation;
            self.stack.push(state);
        }
    }
    fn step(&mut self, bytes: &[u8], at: usize) -> bool {
        self.generation += 1;
        // Seed the anchored expression at every byte for an unanchored search.
        // Assertions still see the real neighboring bytes and line boundaries.
        for index in 0..self.seeds.len() {
            self.push(self.seeds[index]);
        }
        self.seeds.clear();
        self.push(self.nfa.start_anchored());
        let nfa = self.nfa;
        while let Some(id) = self.stack.pop() {
            match nfa.state(id) {
                State::Match { .. } => return true,
                State::Fail => {}
                State::Look { look, next } => {
                    if nfa.look_matcher().matches(*look, bytes, at) {
                        self.push(*next);
                    }
                }
                State::Capture { next, .. } => self.push(*next),
                State::Union { alternates } => {
                    for &next in alternates.iter() {
                        self.push(next);
                    }
                }
                State::BinaryUnion { alt1, alt2 } => {
                    self.push(*alt1);
                    self.push(*alt2);
                }
                State::ByteRange { trans } => {
                    if at < bytes.len() && trans.matches_byte(bytes[at]) {
                        self.seeds.push(trans.next);
                    }
                }
                State::Sparse(transitions) => {
                    if let Some(next) = transitions.matches(bytes, at) {
                        self.seeds.push(next);
                    }
                }
                State::Dense(transitions) => {
                    if let Some(next) = transitions.matches(bytes, at) {
                        self.seeds.push(next);
                    }
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kw41_stream_regex_matches_reference_across_windows() {
        for edge in [0, 1, (64 << 10) - 6, (64 << 10) - 4, (64 << 10) - 1, 64 << 10, (128 << 10) - 2] {
            let mut bytes = vec![b'x'; edge];
            bytes.extend_from_slice(" é猫 word42\r\nfinal".as_bytes());
            bytes.push(0xff);
            for expression in [
                "word[0-9]+",
                "^x.*final",
                "(?s)^x.*final",
                "\\b猫\\b",
                "\\bé猫\\b",
                "\\Bé\\B",
                "\\bword42\\b",
                "(?m)^final",
                "(?m)42$",
                "final(?-u:\\xFF)$",
                "^word",
                "\\Ax* é猫",
                "final\\z",
                "^$",
                "",
            ] {
                let expected = regex::bytes::RegexBuilder::new(expression).crlf(true).build().unwrap().is_match(&bytes);
                assert_eq!(
                    StreamRegex::new(expression).unwrap().is_match(bytes.as_slice(), || false).unwrap(),
                    expected,
                    "{expression:?} at {edge}"
                );
            }
        }
    }

    #[test]
    fn kw41_stream_regex_unbounded_match_and_cancellation() {
        let mut bytes = vec![b'a'; (2 << 20) + 19];
        bytes.push(b'z');
        assert!(StreamRegex::new("^a+z$").unwrap().is_match(bytes.as_slice(), || false).unwrap());
        assert!(!StreamRegex::new("^a+$").unwrap().is_match(bytes.as_slice(), || false).unwrap());
        assert_eq!(
            StreamRegex::new("z").unwrap().is_match(bytes.as_slice(), || true).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }
}
