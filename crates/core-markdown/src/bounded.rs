//! Whole-document preview admission, before constructing a render model.
use unicode_segmentation::UnicodeSegmentation;
pub const PREVIEW_SOURCE_BYTES: usize = 256 * 1024;
pub const PREVIEW_ITEMS: usize = 8192;
pub const PREVIEW_PAYLOAD_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewFailure {
    AboveLimit,
    Items,
    Payload,
    Target,
    TableColumns,
    Depth,
    Cluster,
    ReferenceExpansion,
    Cancelled,
    Parse,
    Opaque,
}

/// Preflight event/model amplification before the existing Model builder grows.
/// The parser's whole-input first pass is bounded by the admitted source, but
/// its API cannot be interrupted or allocation-metered inside its constructor.
/// No new parser or copied upstream grammar is implemented here.
pub fn preview_model(text: &str, cancelled: &dyn Fn() -> bool) -> Result<crate::Model, PreviewFailure> {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
    if text.len() > PREVIEW_SOURCE_BYTES {
        return Err(PreviewFailure::AboveLimit);
    }
    if cancelled() {
        return Err(PreviewFailure::Cancelled);
    }
    if text.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) {
        return Err(PreviewFailure::Opaque);
    }
    let flags = Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH;
    let mut items = 0usize;
    let mut payload = 0usize;
    let mut quote = 0usize;
    let mut list = 0usize;
    let mut style = 0usize;
    let mut link_styles = [0usize; 65];
    let mut links = 0usize;
    let mut image_depth = 0usize;
    let mut alt_bytes = 0usize;
    let parser = Parser::new_ext(text, flags);
    // Upstream may silently refuse later references after its clone budget.
    // Prove a conservative upper bound before accepting a complete preview:
    // every possible use needs a '[' and cannot exceed the largest definition.
    // Otherwise fail explicitly, rather than publish budget-dependent literals.
    let mut largest_definition = 0usize;
    for (_, definition) in parser.reference_definitions().iter() {
        if cancelled() {
            return Err(PreviewFailure::Cancelled);
        }
        let title = definition.title.as_ref().map_or(0, |title| title.len());
        if definition.dest.len() > 4096 || title > 4096 {
            return Err(PreviewFailure::Target);
        }
        largest_definition = largest_definition.max(definition.dest.len().saturating_add(title));
    }
    let brackets = text.bytes().filter(|&byte| byte == b'[').count();
    if brackets.checked_mul(largest_definition).is_none_or(|bytes| bytes > text.len().max(100000)) {
        return Err(PreviewFailure::ReferenceExpansion);
    }
    for event in parser {
        if cancelled() {
            return Err(PreviewFailure::Cancelled);
        }
        items = items.checked_add(1).ok_or(PreviewFailure::Items)?;
        if items > PREVIEW_ITEMS {
            return Err(PreviewFailure::Items);
        }
        let bytes = match event {
            Event::Start(Tag::Link { dest_url, title, .. }) => {
                if dest_url.len() > 4096 || title.len() > 4096 {
                    return Err(PreviewFailure::Target);
                }
                // Conservatively charge shared link targets per subsequent span.
                if links == 64 {
                    return Err(PreviewFailure::Depth);
                }
                link_styles[links] = style;
                links += 1;
                style = dest_url.len();
                dest_url.len().checked_add(title.len()).ok_or(PreviewFailure::Payload)?
            }
            Event::End(TagEnd::Link) => {
                links = links.saturating_sub(1);
                style = link_styles[links];
                0
            }
            Event::Start(Tag::Image { dest_url, title, .. }) => {
                if dest_url.len() > 4096 || title.len() > 4096 {
                    return Err(PreviewFailure::Target);
                }
                if image_depth == 0 {
                    alt_bytes = 0;
                }
                image_depth += 1;
                if image_depth > 64 {
                    return Err(PreviewFailure::Depth);
                }
                dest_url.len().checked_add(title.len()).ok_or(PreviewFailure::Payload)?
            }
            Event::End(TagEnd::Image) => {
                image_depth = image_depth.saturating_sub(1);
                0
            }
            Event::Start(Tag::Table(columns)) => {
                if columns.len() > 32 {
                    return Err(PreviewFailure::TableColumns);
                }
                0
            }
            Event::Start(Tag::BlockQuote(_)) => {
                quote += 1;
                if quote > 64 {
                    return Err(PreviewFailure::Depth);
                }
                0
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                quote = quote.saturating_sub(1);
                0
            }
            Event::Start(Tag::List(_)) => {
                list += 1;
                if list > 64 {
                    return Err(PreviewFailure::Depth);
                }
                0
            }
            Event::End(TagEnd::List(_)) => {
                list = list.saturating_sub(1);
                0
            }
            Event::Start(Tag::CodeBlock(pulldown_cmark::CodeBlockKind::Fenced(info))) => info.len(),
            Event::Text(text) | Event::Code(text) | Event::Html(text) | Event::InlineHtml(text) => {
                if text.graphemes(true).any(|cluster| cluster.len() > 256) {
                    return Err(PreviewFailure::Cluster);
                }
                if image_depth != 0 {
                    alt_bytes = alt_bytes.checked_add(text.len()).ok_or(PreviewFailure::Payload)?;
                    if alt_bytes > 4096 {
                        return Err(PreviewFailure::Target);
                    }
                }
                text.len().checked_add(style).ok_or(PreviewFailure::Payload)?
            }
            Event::SoftBreak | Event::HardBreak => {
                if image_depth != 0 {
                    alt_bytes = alt_bytes.checked_add(1).ok_or(PreviewFailure::Payload)?;
                    if alt_bytes > 4096 {
                        return Err(PreviewFailure::Target);
                    }
                    style.saturating_add(1)
                } else {
                    style
                }
            }
            _ => 0,
        };
        payload = payload.checked_add(bytes).ok_or(PreviewFailure::Payload)?;
        if payload > PREVIEW_PAYLOAD_BYTES {
            return Err(PreviewFailure::Payload);
        }
    }
    if cancelled() {
        return Err(PreviewFailure::Cancelled);
    }
    let model = crate::parse(text).map_err(|_| PreviewFailure::Parse)?;
    if cancelled() {
        return Err(PreviewFailure::Cancelled);
    }
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn utf8_binary_controls_are_explicit_instead_of_interpreted_as_text() {
        assert_eq!(preview_model("# Header\n\0binary", &|| false), Err(PreviewFailure::Opaque));
        assert!(preview_model("# Header\r\n\ttext\n", &|| false).is_ok());
    }
}
