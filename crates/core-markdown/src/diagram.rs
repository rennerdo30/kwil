//! Deliberately small, inert Mermaid subset. Parse on the Markdown worker.
//! No recovery: one unsupported statement rejects the entire diagram, allowing
//! the host to retain the original fenced code rather than display a partial graph.
//!
//! Supported: `flowchart`/`graph` TD, TB, LR, RL, BT; id, id[label],
//! id("label"), id{decision}; -->, -->|label| and -- label --> chains.
//! `sequenceDiagram`: participant id [as label], id->>id: label and
//! id-->>id: label (including self messages). Comments start with %% outside labels.
//! Labels are literal text, with quoted `\"` and `\\` escapes; no HTML, execution,
//! directives, styling, subgraphs, activation, notes or other Mermaid grammar.

use std::fmt;

pub const MAX_SOURCE_BYTES: usize = 16 * 1024;
pub const MAX_NODES: usize = 64;
pub const MAX_EDGES: usize = 128;
pub const MAX_LABEL_SCALARS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    TD,
    TB,
    LR,
    RL,
    BT,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Flow(Direction),
    Sequence,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Box,
    Rounded,
    Decision,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stroke {
    Solid,
    Dashed,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub id: String,
    pub label: String,
    pub shape: Shape,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub label: String,
    pub stroke: Stroke,
}

/// Construction is private so callers cannot bypass the parser's admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagram {
    kind: Kind,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}
impl Diagram {
    pub fn kind(&self) -> Kind {
        self.kind
    }
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }
    /// Aggregate host admission; count IDs too, including implicit label copies.
    pub fn text_bytes(&self) -> usize {
        self.nodes.iter().map(|n| n.id.len() + n.label.len()).sum::<usize>()
            + self.edges.iter().map(|e| e.label.len()).sum::<usize>()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    SourceBytes,
    Nodes,
    Edges,
    LabelScalars,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unsupported {
    Header,
    Declaration,
    Shape,
    Edge,
    Label,
    Statement,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Limit(Limit),
    Syntax { line: usize, expected: &'static str },
    Unsupported { line: usize, construct: Unsupported },
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Limit(limit) => write!(f, "Mermaid diagram limit exceeded: {limit:?}"),
            Self::Syntax { line, expected } => write!(f, "Invalid Mermaid at line {line}: expected {expected}"),
            Self::Unsupported { line, construct } => write!(f, "Unsupported Mermaid {construct:?} at line {line}"),
        }
    }
}
impl std::error::Error for Error {}

fn syntax(line: usize, expected: &'static str) -> Error {
    Error::Syntax { line, expected }
}
fn unsupported(line: usize, construct: Unsupported) -> Error {
    Error::Unsupported { line, construct }
}

/// Source admission occurs before any allocation. Scanning and all model storage
/// are bounded by 16 KiB / 64 nodes / 128 edges. No silently omitted statements.
pub fn parse(source: &str) -> Result<Diagram, Error> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(Error::Limit(Limit::SourceBytes));
    }
    let statements = statements(source)?;
    let Some(&(line, header)) = statements.first() else {
        return Err(syntax(1, "diagram header"));
    };
    let kind = if header == "sequenceDiagram" {
        Kind::Sequence
    } else {
        let words: Vec<_> = header.split_whitespace().collect();
        if words.len() != 2 || !matches!(words[0], "flowchart" | "graph") {
            return Err(unsupported(line, Unsupported::Header));
        }
        Kind::Flow(match words[1] {
            "TD" => Direction::TD,
            "TB" => Direction::TB,
            "LR" => Direction::LR,
            "RL" => Direction::RL,
            "BT" => Direction::BT,
            _ => return Err(unsupported(line, Unsupported::Header)),
        })
    };
    let mut parser = Parser { diagram: Diagram { kind, nodes: Vec::new(), edges: Vec::new() }, explicit: Vec::new() };
    for &(line, text) in &statements[1..] {
        match kind {
            Kind::Flow(_) => parser.flow(text, line)?,
            Kind::Sequence => parser.sequence(text, line)?,
        }
    }
    if parser.diagram.nodes.is_empty() {
        return Err(syntax(line, "at least one node or participant"));
    }
    Ok(parser.diagram)
}

/// Split only outside shapes / quoted labels. Comments never consume quoted %%.
fn statements(source: &str) -> Result<Vec<(usize, &str)>, Error> {
    let bytes = source.as_bytes();
    let (mut start, mut line, mut first_line, mut depth, mut quoted, mut escape) = (0, 1, 1, 0usize, false, false);
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\n' {
            line += 1;
        }
        if quoted {
            if escape {
                escape = false;
            } else if c == b'\\' {
                escape = true;
            } else if c == b'"' {
                quoted = false;
            }
        } else if c == b'"' {
            quoted = true;
        } else if depth == 0 && c == b'%' && bytes.get(i + 1) == Some(&b'%') {
            if bytes.get(i + 2) == Some(&b'{') {
                return Err(unsupported(line, Unsupported::Declaration));
            }
            let text = source[start..i].trim();
            if !text.is_empty() {
                out.push((first_line, text));
            }
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            start = i;
            first_line = line;
            continue;
        } else if matches!(c, b'[' | b'(' | b'{') {
            depth += 1;
        } else if matches!(c, b']' | b')' | b'}') {
            depth = depth.checked_sub(1).ok_or_else(|| syntax(line, "matching shape delimiter"))?;
        } else if depth == 0 && matches!(c, b';' | b'\n') {
            let text = source[start..i].trim();
            if !text.is_empty() {
                out.push((first_line, text));
            }
            start = i + 1;
            first_line = line;
        }
        // Lines after whitespace belong to the next statement, not its preceding blank line.
        if start == i && c.is_ascii_whitespace() {
            start = i + 1;
            first_line = line;
        }
        i += 1;
    }
    if quoted || depth != 0 {
        return Err(syntax(line, "closed quote and shape"));
    }
    let text = source[start..].trim();
    if !text.is_empty() {
        out.push((first_line, text));
    }
    Ok(out)
}

struct Cursor<'a> {
    rest: &'a str,
    line: usize,
}
impl<'a> Cursor<'a> {
    fn trim(&mut self) {
        self.rest = self.rest.trim_start();
    }
    fn take(&mut self, prefix: &str) -> bool {
        self.trim();
        if let Some(tail) = self.rest.strip_prefix(prefix) {
            self.rest = tail;
            true
        } else {
            false
        }
    }
    fn id(&mut self) -> Result<&'a str, Error> {
        self.trim();
        let end = self.rest.bytes().take_while(|c| c.is_ascii_alphanumeric() || *c == b'_').count();
        if end == 0 || self.rest.as_bytes()[0].is_ascii_digit() {
            return Err(unsupported(self.line, Unsupported::Statement));
        }
        if end > MAX_LABEL_SCALARS {
            return Err(Error::Limit(Limit::LabelScalars));
        }
        let id = &self.rest[..end];
        self.rest = &self.rest[end..];
        Ok(id)
    }
    fn label_until(&mut self, close: char) -> Result<String, Error> {
        self.trim();
        let mut quoted = false;
        let mut escape = false;
        for (index, c) in self.rest.char_indices() {
            if escape {
                escape = false;
                continue;
            }
            if quoted && c == '\\' {
                escape = true;
                continue;
            }
            if c == '"' {
                quoted = !quoted;
                continue;
            }
            if !quoted && c == close {
                let text = literal(&self.rest[..index], self.line)?;
                self.rest = &self.rest[index + c.len_utf8()..];
                return Ok(text);
            }
        }
        Err(syntax(self.line, "closed label"))
    }
}

fn literal(text: &str, line: usize) -> Result<String, Error> {
    let text = text.trim();
    let mut out = String::new();
    if let Some(inner) = text.strip_prefix('"') {
        let inner = inner.strip_suffix('"').ok_or_else(|| syntax(line, "closing quote"))?;
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some(c @ ('"' | '\\')) => out.push(c),
                    _ => return Err(unsupported(line, Unsupported::Label)),
                },
                '"' => return Err(unsupported(line, Unsupported::Label)),
                c if c.is_control() => return Err(unsupported(line, Unsupported::Label)),
                c => out.push(c),
            }
        }
    } else {
        if text.chars().any(|c| c.is_control() || matches!(c, '"' | '\\' | '[' | ']' | '{' | '}' | '(' | ')' | '|')) {
            return Err(unsupported(line, Unsupported::Label));
        }
        out.push_str(text);
    }
    if out.chars().count() > MAX_LABEL_SCALARS {
        return Err(Error::Limit(Limit::LabelScalars));
    }
    Ok(out)
}

struct Parser {
    diagram: Diagram,
    explicit: Vec<bool>,
}
impl Parser {
    fn node(&mut self, id: &str, label: Option<(String, Shape)>, line: usize) -> Result<usize, Error> {
        if let Some(index) = self.diagram.nodes.iter().position(|n| n.id == id) {
            if let Some((label, shape)) = label {
                let node = &mut self.diagram.nodes[index];
                if self.explicit[index] && (node.label != label || node.shape != shape) {
                    return Err(syntax(line, "consistent node declaration"));
                }
                node.label = label;
                node.shape = shape;
                self.explicit[index] = true;
            }
            return Ok(index);
        }
        if self.diagram.nodes.len() == MAX_NODES {
            return Err(Error::Limit(Limit::Nodes));
        }
        let explicit = label.is_some();
        let (label, shape) = label.unwrap_or_else(|| (id.to_owned(), Shape::Box));
        self.diagram.nodes.push(Node { id: id.into(), label, shape });
        self.explicit.push(explicit);
        Ok(self.diagram.nodes.len() - 1)
    }
    fn edge(&mut self, from: usize, to: usize, label: String, stroke: Stroke) -> Result<(), Error> {
        if self.diagram.edges.len() == MAX_EDGES {
            return Err(Error::Limit(Limit::Edges));
        }
        self.diagram.edges.push(Edge { from, to, label, stroke });
        Ok(())
    }
    fn endpoint(&mut self, cursor: &mut Cursor<'_>) -> Result<usize, Error> {
        let id = cursor.id()?;
        if matches!(id, "subgraph" | "end" | "direction" | "style" | "classDef" | "class" | "click" | "linkStyle") {
            return Err(unsupported(cursor.line, Unsupported::Declaration));
        }
        cursor.trim();
        let shape = match cursor.rest.chars().next() {
            Some('[') => Some((']', Shape::Box)),
            Some('(') => Some((')', Shape::Rounded)),
            Some('{') => Some(('}', Shape::Decision)),
            _ => None,
        };
        let label = if let Some((close, shape)) = shape {
            cursor.rest = &cursor.rest[1..];
            if cursor.rest.chars().next().is_some_and(|c| matches!(c, '[' | '(' | '{')) {
                return Err(unsupported(cursor.line, Unsupported::Shape));
            }
            Some((cursor.label_until(close)?, shape))
        } else {
            None
        };
        self.node(id, label, cursor.line)
    }
    fn flow(&mut self, text: &str, line: usize) -> Result<(), Error> {
        let mut cursor = Cursor { rest: text, line };
        let mut from = self.endpoint(&mut cursor)?;
        cursor.trim();
        while !cursor.rest.is_empty() {
            let label = if cursor.take("-->") {
                if cursor.take("|") { cursor.label_until('|')? } else { String::new() }
            } else if cursor.take("--") {
                // Arrow text can be quoted, so locate --> only outside quotes.
                let mut quoted = false;
                let mut escape = false;
                let mut end = None;
                for (i, c) in cursor.rest.char_indices() {
                    if escape {
                        escape = false;
                        continue;
                    }
                    if quoted && c == '\\' {
                        escape = true;
                        continue;
                    }
                    if c == '"' {
                        quoted = !quoted;
                    }
                    if !quoted && cursor.rest[i..].starts_with("-->") {
                        end = Some(i);
                        break;
                    }
                }
                let end = end.ok_or_else(|| unsupported(line, Unsupported::Edge))?;
                let label = literal(&cursor.rest[..end], line)?;
                if label.is_empty() {
                    return Err(unsupported(line, Unsupported::Edge));
                }
                cursor.rest = &cursor.rest[end + 3..];
                label
            } else {
                return Err(unsupported(line, Unsupported::Edge));
            };
            let to = self.endpoint(&mut cursor)?;
            self.edge(from, to, label, Stroke::Solid)?;
            from = to;
            cursor.trim();
        }
        Ok(())
    }
    fn sequence(&mut self, text: &str, line: usize) -> Result<(), Error> {
        let mut cursor = Cursor { rest: text, line };
        let first = cursor.id()?;
        if first == "participant" {
            let id = cursor.id()?;
            cursor.trim();
            let label = if cursor.rest.is_empty() {
                id.to_owned()
            } else if cursor.rest.starts_with("as ") {
                literal(&cursor.rest[3..], line)?
            } else {
                return Err(unsupported(line, Unsupported::Declaration));
            };
            self.node(id, Some((label, Shape::Box)), line)?;
            return Ok(());
        }
        if matches!(
            first,
            "actor"
                | "activate"
                | "deactivate"
                | "Note"
                | "note"
                | "loop"
                | "alt"
                | "else"
                | "end"
                | "opt"
                | "par"
                | "rect"
                | "autonumber"
                | "title"
                | "box"
                | "create"
                | "destroy"
        ) {
            return Err(unsupported(line, Unsupported::Declaration));
        }
        let stroke = if cursor.take("-->>") {
            Stroke::Dashed
        } else if cursor.take("->>") {
            Stroke::Solid
        } else {
            return Err(unsupported(line, Unsupported::Edge));
        };
        let second = cursor.id()?;
        if !cursor.take(":") {
            return Err(syntax(line, "message colon"));
        }
        let label = literal(cursor.rest, line)?;
        let from = self.node(first, None, line)?;
        let to = self.node(second, None, line)?;
        self.edge(from, to, label, stroke)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn flow_exact_nodes_edges_quoted_labels_and_chains() {
        let d = parse("flowchart TD; A[\"Start; %% [x]\"] -->|\"yes | ok\"| B{Ready?}\n B -- no --> C(\"Try \\\"again\\\"\"); C --> A").unwrap();
        assert_eq!(d.kind(), Kind::Flow(Direction::TD));
        assert_eq!(
            d.nodes(),
            &[
                Node { id: "A".into(), label: "Start; %% [x]".into(), shape: Shape::Box },
                Node { id: "B".into(), label: "Ready?".into(), shape: Shape::Decision },
                Node { id: "C".into(), label: "Try \"again\"".into(), shape: Shape::Rounded },
            ]
        );
        assert_eq!(
            d.edges(),
            &[
                Edge { from: 0, to: 1, label: "yes | ok".into(), stroke: Stroke::Solid },
                Edge { from: 1, to: 2, label: "no".into(), stroke: Stroke::Solid },
                Edge { from: 2, to: 0, label: "".into(), stroke: Stroke::Solid },
            ]
        );
        assert_eq!(parse("graph LR; A-->B-->C").unwrap().edges().len(), 2);
    }
    #[test]
    fn sequence_exact_participants_messages_and_self_message() {
        let d = parse("sequenceDiagram\nparticipant A as Alice\nparticipant B as \"Bob; B\"\nA->>B: Hello\nB-->>A: \"Hi: \\\"Alice\\\"\"\nA->>A: Think").unwrap();
        assert_eq!(d.kind(), Kind::Sequence);
        assert_eq!(d.nodes().iter().map(|n| n.label.as_str()).collect::<Vec<_>>(), ["Alice", "Bob; B"]);
        assert_eq!(
            d.edges().iter().map(|e| (e.from, e.to, e.label.as_str(), e.stroke)).collect::<Vec<_>>(),
            [(0, 1, "Hello", Stroke::Solid), (1, 0, "Hi: \"Alice\"", Stroke::Dashed), (0, 0, "Think", Stroke::Solid)]
        );
    }
    #[test]
    fn unsupported_never_becomes_partial_success() {
        for text in [
            "flowchart TD; A-->B; click A callback",
            "flowchart TD; A-->B; subgraph X",
            "flowchart TD; A((circle))",
            "flowchart TD; A-.->B",
            "flowchart TD; A & B --> C",
            "sequenceDiagram; A->>B: hi; Note over A: no",
            "pie; A",
            "%%{init: {}}%%\nflowchart TD; A-->B",
            "flowchart TD; A-->B\n%%{init: {}}%%",
        ] {
            assert!(matches!(parse(text), Err(Error::Unsupported { .. })), "{text}");
        }
    }
    #[test]
    fn bad_and_oversized_input_is_rejected_before_model_success() {
        assert!(matches!(parse("flowchart TD; A[unfinished"), Err(Error::Syntax { .. })));
        assert!(matches!(parse("flowchart TD; A[one]; A[two]"), Err(Error::Syntax { .. })));
        assert_eq!(parse(&" ".repeat(MAX_SOURCE_BYTES + 1)), Err(Error::Limit(Limit::SourceBytes)));
        let nodes = format!("graph TD; {}", (0..65).map(|n| format!("N{n}")).collect::<Vec<_>>().join(";"));
        assert_eq!(parse(&nodes), Err(Error::Limit(Limit::Nodes)));
        let edges = format!("graph TD; {}", ["A-->B"; 129].join(";"));
        assert_eq!(parse(&edges), Err(Error::Limit(Limit::Edges)));
        assert_eq!(parse(&format!("graph TD; A[{}]", "x".repeat(257))), Err(Error::Limit(Limit::LabelScalars)));
        assert_eq!(parse(&format!("graph TD; {}", "x".repeat(257))), Err(Error::Limit(Limit::LabelScalars)));
    }
    #[test]
    fn cycles_duplicates_all_directions_and_implicit_declarations_are_preserved() {
        for direction in ["TD", "TB", "LR", "RL", "BT"] {
            let d = parse(&format!("graph {direction}; A-->B; B-->A; A-->A; A-->B; B[Later]")).unwrap();
            assert_eq!(d.nodes().len(), 2);
            assert_eq!(d.nodes()[1].label, "Later");
            assert_eq!(d.edges().len(), 4);
        }
        assert_eq!(parse("%% comment\ngraph TD\nA[日本語] %% end\nA-->B").unwrap().nodes()[0].label, "日本語");
    }
}
