//! Viewport highlighting on a cancellable worker, independent of the text engine.
//!
//! Submit immutable snapshots; retain the last `Highlights` while a new request runs.
//! See the crate README for bounds, integration and grammar-provider contracts.
mod engine;
mod language;
mod lexer;
mod worker;

#[cfg(feature = "measure")]
pub use engine::Timings;
pub use engine::{Config, Engine, Grammar, GrammarProvider, Highlights, Mode};
pub use language::{Language, detect};
pub use tree_sitter::{InputEdit, Point};
pub use worker::{Request, Worker};

use std::{borrow::Cow, io, ops::Range};

/// Immutable byte snapshot. `chunk` starts exactly at `offset`, returns at most
/// `max_len` bytes, and returns nonempty bytes before EOF. Implementations must
/// bound I/O and must not copy the whole document to serve a chunk.
pub trait TextSource: Send + Sync {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn chunk(&self, offset: usize, max_len: usize) -> io::Result<Cow<'_, [u8]>>;
}

impl TextSource for Vec<u8> {
    fn len(&self) -> usize {
        Vec::len(self)
    }
    fn chunk(&self, offset: usize, max_len: usize) -> io::Result<Cow<'_, [u8]>> {
        if offset > self.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "offset past EOF"));
        }
        Ok(Cow::Borrowed(&self[offset..self.len().min(offset.saturating_add(max_len))]))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    Keyword,
    String,
    Comment,
    Number,
    Type,
    Function,
    Variable,
    Constant,
    Operator,
    Punctuation,
    Tag,
    Attribute,
    Property,
    Heading,
    Link,
    Error,
}

impl Class {
    /// Stable names for theme files (KW-37).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Keyword => "keyword",
            Self::String => "string",
            Self::Comment => "comment",
            Self::Number => "number",
            Self::Type => "type",
            Self::Function => "function",
            Self::Variable => "variable",
            Self::Constant => "constant",
            Self::Operator => "operator",
            Self::Punctuation => "punctuation",
            Self::Tag => "tag",
            Self::Attribute => "attribute",
            Self::Property => "property",
            Self::Heading => "heading",
            Self::Link => "link",
            Self::Error => "error",
        }
    }

    pub(crate) fn capture(name: &str) -> Option<Self> {
        let root = name.split('.').next().unwrap_or(name);
        Some(match root {
            "keyword" | "conditional" | "repeat" | "include" | "exception" => Self::Keyword,
            "string" | "character" => Self::String,
            "comment" => Self::Comment,
            "number" | "float" => Self::Number,
            "type" | "constructor" | "namespace" | "module" => Self::Type,
            "function" | "method" => Self::Function,
            "variable" | "parameter" => Self::Variable,
            "constant" | "boolean" => Self::Constant,
            "operator" => Self::Operator,
            "punctuation" => Self::Punctuation,
            "tag" => Self::Tag,
            "attribute" => Self::Attribute,
            "property" | "field" => Self::Property,
            "label" => Self::Heading,
            "error" => Self::Error,
            "markup" | "text" if name.contains("heading") || name.contains("title") => Self::Heading,
            "markup" | "text" if name.contains("link") || name.contains("uri") || name.contains("reference") => {
                Self::Link
            }
            "markup" | "text" if name.contains("raw") || name.contains("literal") => Self::String,
            _ => return None,
        })
    }
}

/// Absolute document byte offsets, clipped to `Highlights::requested`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub range: Range<usize>,
    pub class: Class,
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    InvalidRange,
    InvalidConfig,
    Cancelled,
    Closed,
    Grammar(String),
    QueryLimit,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub(crate) fn read_chunk(source: &dyn TextSource, offset: usize, max: usize) -> Result<Cow<'_, [u8]>, Error> {
    let bytes = source.chunk(offset, max)?;
    if bytes.len() > max
        || (offset < source.len() && bytes.is_empty())
        || bytes.len() > source.len().saturating_sub(offset)
    {
        return Err(Error::Io(io::Error::new(io::ErrorKind::InvalidData, "invalid source chunk")));
    }
    Ok(bytes)
}
