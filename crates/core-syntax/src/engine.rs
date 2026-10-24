use crate::{Class, Error, InputEdit, Language, Span, TextSource, lexer, read_chunk};
use std::{
    borrow::Cow,
    collections::BTreeSet,
    ops::{ControlFlow, Range},
    sync::Arc,
};
use tree_sitter::{
    Node, ParseOptions, Parser, Query, QueryCursor, QueryCursorOptions, StreamingIterator, TextProvider, Tree,
};

const CHUNK: usize = 16 * 1024;

/// Optional, one-shot instrumentation; compiled out without the `measure` feature.
#[cfg(feature = "measure")]
#[derive(Debug, Default, Clone)]
pub struct Timings {
    pub edit: std::time::Duration,
    pub parse: std::time::Duration,
    pub query: std::time::Duration,
    pub dispose: std::time::Duration,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub parse_limit: usize,
    pub margin_bytes: usize,
    /// Hard admission bound, including the margin. Long lines are split by callers.
    pub max_window_bytes: usize,
    /// Upper bound on text copied for one query predicate, including offscreen nodes.
    pub max_capture_bytes: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            parse_limit: 20 * 1024 * 1024,
            margin_bytes: 4096,
            max_window_bytes: 256 * 1024,
            max_capture_bytes: 64 * 1024,
        }
    }
}

/// Injection-free base grammar. The owned query supports future prebuilt providers.
pub struct Grammar {
    pub language: tree_sitter::Language,
    pub highlights: Cow<'static, str>,
}

/// Seam for prebuilt libraries: retain their handles at least as long as the
/// provider/worker and return ABI-compatible `Language`s plus owned queries.
pub trait GrammarProvider: Send + Sync {
    fn grammar(&self, language: Language) -> Result<Option<Grammar>, Error>;
}
struct Bundled;
impl GrammarProvider for Bundled {
    fn grammar(&self, language: Language) -> Result<Option<Grammar>, Error> {
        Ok(language.bundled())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    TreeSitter,
    Lexical,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Highlights {
    pub revision: u64,
    pub requested: Range<usize>,
    pub queried: Range<usize>,
    pub spans: Vec<Span>,
    pub mode: Mode,
    pub incremental: bool,
}

/// Worker-owned parser state. This synchronous API is also useful for headless
/// consumers; UI code should use `Worker`.
pub struct Engine {
    config: Config,
    parser: Parser,
    query: Option<Query>,
    language: Option<Language>,
    tree: Option<Tree>,
    revision: Option<u64>,
    staged_revision: Option<u64>,
    #[cfg(feature = "measure")]
    timings: Timings,
    // Drop provider-owned prebuilt library handles after parsers, queries and trees.
    provider: Arc<dyn GrammarProvider>,
}
impl Engine {
    pub fn new(config: Config) -> Result<Self, Error> {
        Self::with_provider(config, Arc::new(Bundled))
    }
    pub fn with_provider(config: Config, provider: Arc<dyn GrammarProvider>) -> Result<Self, Error> {
        if config.max_window_bytes == 0
            || config.max_capture_bytes == 0
            || config.margin_bytes > config.max_window_bytes / 2
        {
            return Err(Error::InvalidConfig);
        }
        Ok(Self {
            config,
            provider,
            parser: Parser::new(),
            query: None,
            language: None,
            tree: None,
            revision: None,
            staged_revision: None,
            #[cfg(feature = "measure")]
            timings: Timings::default(),
        })
    }

    #[cfg(feature = "measure")]
    pub fn timings(&self) -> &Timings {
        &self.timings
    }

    /// A document identity change must invalidate trees even if revisions and
    /// languages coincide. Call only on the owning worker: tree disposal can be
    /// expensive. Grammar/query state is reusable across documents.
    pub fn reset_document(&mut self) {
        self.tree = None;
        self.revision = None;
        self.staged_revision = None;
        self.parser.reset();
    }

    /// Stream exact, sequential edits on the owning worker before ONE parse.
    /// No edit vector or whole-document envelope is needed. A failed producer
    /// invalidates the partially edited tree; a missing base never visits it.
    pub fn stage_edits(
        &mut self,
        language: Language,
        previous_revision: u64,
        revision: u64,
        produce: impl FnOnce(&mut dyn FnMut(InputEdit)) -> Result<(), Error>,
    ) -> Result<bool, Error> {
        if self.language != Some(language) || self.revision != Some(previous_revision) || self.tree.is_none() {
            self.reset_document();
            return Ok(false);
        }
        self.revision = None;
        self.staged_revision = None;
        if let Err(error) = produce(&mut |edit| self.tree.as_mut().unwrap().edit(&edit)) {
            self.reset_document();
            return Err(error);
        }
        self.staged_revision = Some(revision);
        Ok(true)
    }

    /// `edit` must describe the change from `previous_revision` to `revision`.
    /// A missing/mismatched base safely triggers a fresh parse. An unchanged
    /// snapshot may request another viewport with the same revision and no edit.
    #[expect(
        clippy::too_many_arguments,
        reason = "Borrowed equivalent of Request avoids allocating an Arc for synchronous callers"
    )]
    pub fn highlight(
        &mut self,
        source: &dyn TextSource,
        language: Language,
        revision: u64,
        previous_revision: Option<u64>,
        edit: Option<&InputEdit>,
        requested: Range<usize>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Highlights, Error> {
        let result = self.highlight_inner(source, language, revision, previous_revision, edit, requested, cancelled);
        if result.is_err() {
            self.reset_document();
        }
        result
    }

    #[expect(clippy::too_many_arguments, reason = "Worker-owned borrowed request")]
    fn highlight_inner(
        &mut self,
        source: &dyn TextSource,
        language: Language,
        revision: u64,
        previous_revision: Option<u64>,
        edit: Option<&InputEdit>,
        requested: Range<usize>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Highlights, Error> {
        #[cfg(feature = "measure")]
        {
            self.timings = Timings::default();
        }
        if requested.start > requested.end || requested.end > source.len() {
            return Err(Error::InvalidRange);
        }
        let queried = requested.start.saturating_sub(self.config.margin_bytes)
            ..source.len().min(requested.end.saturating_add(self.config.margin_bytes));
        if queried.len() > self.config.max_window_bytes {
            return Err(Error::InvalidRange);
        }
        if cancelled() {
            return Err(Error::Cancelled);
        }
        let mut incremental = false;
        let mut mode = Mode::Lexical;
        // Tree-sitter offsets are u32. Huge sources never enter its parser.
        let parse = !matches!(language, Language::Generic | Language::Log)
            && source.len() <= self.config.parse_limit
            && source.len() < u32::MAX as usize;
        if parse && self.language != Some(language) {
            self.reset_document();
            self.language = None;
            self.query = None;
            if let Some(grammar) = self.provider.grammar(language)? {
                self.parser.set_language(&grammar.language).map_err(|e| Error::Grammar(e.to_string()))?;
                let query = format!("{}\n(ERROR) @error", grammar.highlights);
                self.query = Some(Query::new(&grammar.language, &query).map_err(|e| Error::Grammar(e.to_string()))?);
            }
            self.language = Some(language);
        }
        let spans = if parse && self.query.is_some() {
            mode = Mode::TreeSitter;
            let unchanged = self.revision == Some(revision) && edit.is_none() && self.tree.is_some();
            if !unchanged {
                let staged = self.staged_revision == Some(revision);
                incremental = self.tree.is_some() && (staged || (self.revision == previous_revision && edit.is_some()));
                #[cfg(feature = "measure")]
                let edit_start = std::time::Instant::now();
                if incremental {
                    if !staged {
                        self.tree.as_mut().unwrap().edit(edit.unwrap());
                    }
                } else {
                    self.tree = None;
                }
                self.staged_revision = None;
                #[cfg(feature = "measure")]
                {
                    self.timings.edit = edit_start.elapsed();
                }
                let mut read_error = None;
                let mut reader = |offset: usize, _| {
                    if cancelled() {
                        return Cow::Borrowed(&[] as &[u8]);
                    }
                    match read_chunk(source, offset, CHUNK.min(source.len().saturating_sub(offset))) {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            read_error = Some(error);
                            Cow::Borrowed(&[])
                        }
                    }
                };
                #[cfg(feature = "measure")]
                let parse_start = std::time::Instant::now();
                self.parser.reset(); // Never resume a cancelled parse against a different snapshot.
                let mut progress = |_: &tree_sitter::ParseState| {
                    if cancelled() { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
                };
                let tree = self.parser.parse_with_options(
                    &mut reader,
                    self.tree.as_ref(),
                    Some(ParseOptions::new().progress_callback(&mut progress)),
                );
                #[cfg(feature = "measure")]
                {
                    self.timings.parse = parse_start.elapsed();
                }
                if let Some(error) = read_error {
                    self.tree = None;
                    self.revision = None;
                    return Err(error);
                }
                if cancelled() || tree.is_none() {
                    self.tree = None;
                    self.revision = None;
                    return Err(Error::Cancelled);
                }
                let old_tree = std::mem::replace(&mut self.tree, tree);
                #[cfg(feature = "measure")]
                let dispose_start = std::time::Instant::now();
                drop(old_tree);
                #[cfg(feature = "measure")]
                {
                    self.timings.dispose = dispose_start.elapsed();
                }
                self.revision = Some(revision);
            }
            #[cfg(feature = "measure")]
            let query_start = std::time::Instant::now();
            let spans = self.query_range(source, queried.clone(), requested.clone(), cancelled)?;
            #[cfg(feature = "measure")]
            {
                self.timings.query = query_start.elapsed();
            }
            spans
        } else {
            self.reset_document();
            lexer::highlight(source, language, queried.clone(), requested.clone(), cancelled)?
        };
        if cancelled() {
            return Err(Error::Cancelled);
        }
        Ok(Highlights { revision, requested, queried, spans, mode, incremental })
    }

    fn query_range(
        &self,
        source: &dyn TextSource,
        queried: Range<usize>,
        requested: Range<usize>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<Span>, Error> {
        if queried.is_empty() {
            return Ok(Vec::new());
        }
        let query = self.query.as_ref().unwrap();
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(queried.clone());
        cursor.set_match_limit(4096);
        let provider = Provider { source, max: self.config.max_capture_bytes, error: std::cell::RefCell::new(None) };
        let mut progress = |_: &tree_sitter::QueryCursorState| {
            if cancelled() { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
        };
        let mut captures = cursor.captures_with_options(
            query,
            self.tree.as_ref().unwrap().root_node(),
            &provider,
            QueryCursorOptions::new().progress_callback(&mut progress),
        );
        let mut raw = Vec::new();
        while let Some((matched, index)) = captures.next() {
            if cancelled() {
                return Err(Error::Cancelled);
            }
            let capture = matched.captures()[*index];
            let range = capture.node.start_byte().max(requested.start)..capture.node.end_byte().min(requested.end);
            if !range.is_empty()
                && let Some(mut class) = Class::capture(query.capture_names()[capture.index as usize])
            {
                // Several Arborium queries use @constant.builtin for numerals.
                let kind = capture.node.kind();
                if class == Class::Constant
                    && (kind.contains("integer") || kind.contains("float") || kind.contains("number"))
                {
                    class = Class::Number;
                }
                raw.push((Span { range, class }, matched.pattern_index));
            }
            if raw.len() > self.config.max_window_bytes.saturating_mul(4) {
                return Err(Error::QueryLimit);
            }
        }
        drop(captures);
        if let Some(error) = provider.error.into_inner() {
            return Err(error);
        }
        if cursor.did_exceed_match_limit() {
            return Err(Error::QueryLimit);
        }
        Ok(flatten(raw))
    }
}

struct Provider<'a> {
    source: &'a dyn TextSource,
    max: usize,
    error: std::cell::RefCell<Option<Error>>,
}
impl<'a> TextProvider<Cow<'a, [u8]>> for &Provider<'a> {
    type I = std::vec::IntoIter<Cow<'a, [u8]>>;
    fn text(&mut self, node: Node) -> Self::I {
        let mut offset = node.start_byte();
        let end = node.end_byte().min(offset.saturating_add(self.max));
        let mut chunks = Vec::new();
        while offset < end {
            match read_chunk(self.source, offset, (end - offset).min(CHUNK)) {
                Ok(chunk) => {
                    offset += chunk.len();
                    chunks.push(chunk);
                }
                Err(error) => {
                    *self.error.borrow_mut() = Some(error);
                    break;
                }
            }
        }
        chunks.into_iter()
    }
}

// Later query patterns take precedence, as in tree-sitter-highlight. Narrower
// captures win ties. Sweep boundaries to emit sorted, disjoint spans.
fn flatten(raw: Vec<(Span, usize)>) -> Vec<Span> {
    let mut events = Vec::with_capacity(raw.len() * 2);
    for (i, (span, _)) in raw.iter().enumerate() {
        events.push((span.range.start, true, i));
        events.push((span.range.end, false, i));
    }
    events.sort_unstable();
    let mut active: BTreeSet<(usize, std::cmp::Reverse<usize>, usize)> = BTreeSet::new();
    let mut out: Vec<Span> = Vec::new();
    let mut previous = 0;
    for (at, start, index) in events {
        if previous < at
            && let Some(&(_, _, i)) = active.last()
        {
            let class = raw[i].0.class;
            if let Some(last) = out.last_mut()
                && last.class == class
                && last.range.end == previous
            {
                last.range.end = at;
            } else {
                out.push(Span { range: previous..at, class });
            }
        }
        let key = (raw[index].1, std::cmp::Reverse(raw[index].0.range.len()), index);
        if start {
            active.insert(key);
        } else {
            active.remove(&key);
        }
        previous = at;
    }
    out
}

#[cfg(all(test, feature = "lang-rust"))]
mod transaction_tests {
    use super::*;
    use crate::Point;

    fn insertion(at: usize, len: usize) -> InputEdit {
        InputEdit {
            start_byte: at,
            old_end_byte: at,
            new_end_byte: at + len,
            start_position: Point::new(0, at),
            old_end_position: Point::new(0, at),
            new_end_position: Point::new(0, at + len),
        }
    }
    #[test]
    fn staged_exact_edits_parse_once_and_equal_fresh() {
        let before = b"fn main() { let n = 1; }".to_vec();
        let after = b"/*fn main() { let n = 1; }*/".to_vec();
        let mut engine = Engine::new(Config::default()).unwrap();
        engine.highlight(&before, Language::Rust, 1, None, None, 0..before.len(), &|| false).unwrap();
        assert!(
            engine
                .stage_edits(Language::Rust, 1, 2, |emit| {
                    emit(insertion(before.len(), 2));
                    emit(insertion(0, 2));
                    Ok(())
                })
                .unwrap()
        );
        let result = engine.highlight(&after, Language::Rust, 2, None, None, 0..after.len(), &|| false).unwrap();
        let mut fresh = Engine::new(Config::default()).unwrap();
        let expected = fresh.highlight(&after, Language::Rust, 2, None, None, 0..after.len(), &|| false).unwrap();
        assert!(result.incremental);
        assert_eq!(result.spans, expected.spans);
        assert_eq!(
            engine.tree.as_ref().unwrap().root_node().to_sexp(),
            fresh.tree.as_ref().unwrap().root_node().to_sexp()
        );
        assert_eq!(result.spans, vec![Span { range: 0..after.len(), class: Class::Comment }]);
    }

    #[test]
    fn staged_failure_cancellation_gap_and_language_change_invalidate_tree() {
        let source = b"fn main() {}".to_vec();
        let mut engine = Engine::new(Config::default()).unwrap();
        let seed = |engine: &mut Engine| {
            engine.highlight(&source, Language::Rust, 1, None, None, 0..source.len(), &|| false).unwrap();
        };
        seed(&mut engine);
        assert!(
            engine
                .stage_edits(Language::Rust, 1, 2, |emit| {
                    emit(insertion(0, 2));
                    Err(Error::Io(std::io::Error::other("recipe failed")))
                })
                .is_err()
        );
        assert!(engine.tree.is_none(), "failed edit stream must dispose its partially edited tree");
        seed(&mut engine);
        assert!(engine.stage_edits(Language::Rust, 1, 2, |_| Ok(())).unwrap());
        assert!(matches!(
            engine.highlight(&source, Language::Rust, 2, None, None, 0..source.len(), &|| true),
            Err(Error::Cancelled)
        ));
        assert!(engine.tree.is_none(), "cancelled staged parse must dispose its tree");
        seed(&mut engine);
        assert!(!engine.stage_edits(Language::Rust, 99, 2, |_| panic!("gap must not enumerate edits")).unwrap());
        seed(&mut engine);
        assert!(
            !engine
                .stage_edits(Language::Generic, 1, 2, |_| panic!("language change must not enumerate edits"))
                .unwrap()
        );
        let result = engine.highlight(&source, Language::Rust, 2, None, None, 0..source.len(), &|| false).unwrap();
        assert!(!result.incremental);
    }
}
