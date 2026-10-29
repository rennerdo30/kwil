//! Native hex body adapter. The host resolves its effective Hex-context
//! keymap first; only unsuppressed/unbound keys reach this body fallback.
//! Use the existing monospace Font and theme. No worker submission or reads
//! occur during draw. Call Session::set_rows on layout changes, request a page
//! after bind/scroll/resize/edit completion, and damage only body/status.
//! `key` returns None only for closed hex or explicit ROOT chrome shortcuts.
//! Consume all other body input, including unsupported commands and releases.
//! Route IME/paste with the Source + input_epoch captured at input origin;
//! focus loss calls cancel_input. Never feed committed IME twice as key text.
//! ROOT routes palette/menu command ids for edits through `command` too.
use super::{Font, Rect, bg, draw_text, fg, fill, theme_ui};
use kwil_core_commands::theme::UiRole;
use kwil_view_model::{
    Damage,
    hex::{ASCII_COLUMN, hex_column},
    hex_session::{Input, Outcome, Session, Source},
};
use winit::keyboard::{Key, ModifiersState, NamedKey};

fn dispatch(session: &mut Session, input: Input<'_>) -> Result<Outcome, String> {
    let source = session.source().ok_or("Hex view is closed")?;
    session.input(source, session.input_epoch(), input).map_err(|e| e.to_string())
}
pub(super) fn key(session: &mut Session, key: &Key, mods: ModifiersState) -> Option<Result<Outcome, String>> {
    if !session.is_open() {
        return None;
    }
    let command = mods.control_key() || mods.super_key();
    let extend = mods.shift_key();
    let input = match key {
        Key::Character(text) if command => match text.to_ascii_lowercase().as_str() {
            "z" if extend => Some(Input::Redo),
            "z" => Some(Input::Undo),
            "y" => Some(Input::Redo),
            "a" => Some(Input::SelectAll),
            // ROOT handles save/open/new/close, palette and tab switching.
            // Ctrl+V/C require explicit ROOT hex clipboard hooks; otherwise
            // consume them here, never fall through to hidden text clipboard.
            "s" | "o" | "n" | "w" | "p" => return None,
            _ => None,
        },
        Key::Named(NamedKey::Tab) if command => return None,
        Key::Named(NamedKey::ArrowLeft) => Some(Input::Move { bytes: -1, extend }),
        Key::Named(NamedKey::ArrowRight) => Some(Input::Move { bytes: 1, extend }),
        Key::Named(NamedKey::ArrowUp) => Some(Input::Move { bytes: -16, extend }),
        Key::Named(NamedKey::ArrowDown) => Some(Input::Move { bytes: 16, extend }),
        Key::Named(NamedKey::PageUp) => {
            Some(Input::Move { bytes: -(session.view().rows().max(1) as i64 * 16), extend })
        }
        Key::Named(NamedKey::PageDown) => Some(Input::Move { bytes: session.view().rows().max(1) as i64 * 16, extend }),
        Key::Named(NamedKey::Home) => Some(Input::Home { document: command, extend }),
        Key::Named(NamedKey::End) => Some(Input::End { document: command, extend }),
        Key::Named(NamedKey::Insert) => Some(Input::ToggleInsert),
        Key::Named(NamedKey::Delete) => Some(Input::Delete { backwards: false }),
        Key::Named(NamedKey::Backspace) => Some(Input::Delete { backwards: true }),
        Key::Named(NamedKey::Escape) => Some(Input::Cancel),
        Key::Character(text) if !command && !mods.alt_key() => {
            let mut chars = text.chars();
            match (chars.next(), chars.next()) {
                (Some(digit), None) if digit.is_ascii_hexdigit() => Some(Input::Digit(digit)),
                _ => None,
            }
        }
        _ => None,
    };
    Some(input.map_or_else(|| Ok(Outcome::default()), |input| dispatch(session, input)))
}
pub(super) fn command(session: &mut Session, id: &str, extend: bool) -> Option<Result<Outcome, String>> {
    if !session.is_open() {
        return None;
    }
    let input = kwil_view_model::hex_session::command_input(id, session.view().rows(), extend);
    if let Some(input) = input {
        return Some(dispatch(session, input));
    }
    // Unknown body commands are consumed; only explicit chrome leaves hex.
    if id.starts_with("file.")
        || id.starts_with("tab.")
        || id.starts_with("palette.")
        || id.starts_with("settings.")
        || id == "view.toggleHex"
    {
        return None;
    }
    Some(Ok(Outcome::default()))
}
pub(super) fn text(session: &mut Session, source: Source, epoch: u64, text: &str) -> Result<Outcome, String> {
    let mut chars = text.chars();
    let input = match (chars.next(), chars.next()) {
        (Some(digit), None) => Input::Digit(digit),
        _ => Input::Paste(text),
    };
    session.input(source, epoch, input).map_err(|e| e.to_string())
}
pub(super) fn paste(session: &mut Session, source: Source, epoch: u64, text: &str) -> Result<Outcome, String> {
    session.input(source, epoch, Input::Paste(text)).map_err(|e| e.to_string())
}
pub(super) fn mouse(
    session: &mut Session,
    body: Rect,
    font: &Font,
    at: (usize, usize),
    extend: bool,
) -> Option<Result<Outcome, String>> {
    if !session.is_open() || !body.contains(at) {
        return None;
    }
    Some(dispatch(
        session,
        Input::Click { row: (at.1 - body.y) / font.ch.max(1), column: (at.0 - body.x) / font.cw.max(1), extend },
    ))
}
pub(super) fn wheel(session: &mut Session, rows: i64) -> Result<Outcome, String> {
    dispatch(session, Input::Scroll(rows))
}
/// Body-local retained buffer, excluding status/scrollbars. Stale pages paint
/// blank; ROOT never paints ordinary source bytes behind an active hex body.
pub(super) struct Paint<'a> {
    pub current: Option<Source>,
    pub horizontal: usize,
    pub read_only: bool,
    pub damage: &'a Damage,
}
/// Apply the hex scroll copy to this body-local retained buffer, then paint
/// only damaged/exposed rows. ROOT forwards the same copy/dirty rectangles to
/// the backend present; it must not copy this buffer a second time.
/// The retained body host can combine borrowed, independently charged damage
/// without copying either row allocation. Scroll is supplied only once.
pub(super) fn draw_borrowed(
    session: &Session,
    pixels: &mut [u32],
    stride: usize,
    font: &mut Font,
    paint: Paint<'_>,
    dirty: impl Fn(usize) -> bool,
) -> bool {
    let Paint { current, horizontal, read_only, damage } = paint;
    if !session.is_open() {
        return false;
    }
    let (cw, ch) = (font.cw.max(1), font.ch.max(1));
    if stride == 0 {
        return true;
    }
    let (columns, height) = (stride / cw, pixels.len() / stride);
    let rows = (height / ch).min(session.view().rows());
    let full = damage.full || current != session.source();
    if full {
        pixels.fill(bg());
    }
    if !full && damage.scroll != 0 {
        let shift = damage.scroll.unsigned_abs() as usize;
        if shift < rows {
            let count = shift * ch * stride;
            let end = rows * ch * stride;
            if damage.scroll > 0 {
                pixels.copy_within(count..end, 0);
            } else {
                pixels.copy_within(0..end - count, count);
            }
        }
    }
    let selection = session.view().selection();
    let selected_bg = theme_ui::rgb(&theme_ui::chrome(), UiRole::Selection);
    for index in 0..rows {
        if !full && !dirty(index) {
            continue;
        }
        let y = index * ch;
        fill(pixels, stride, Rect { x: 0, y, w: stride, h: ch }, bg());
        let Some(row) = session.row(current, index) else {
            continue;
        };
        let line = row.text();
        draw_text(
            pixels,
            stride,
            font,
            0,
            y,
            line.as_bytes().get(horizontal..).unwrap_or_default(),
            columns,
            fg(),
            bg(),
        );
        for (byte, _) in row.bytes.iter().enumerate() {
            if !selection.contains(&(row.offset + byte as u64)) {
                continue;
            }
            for (column, width) in [(hex_column(byte), 2), (ASCII_COLUMN + byte, 1)] {
                let Some(screen) = column.checked_sub(horizontal) else {
                    continue;
                };
                if screen + width > columns {
                    continue;
                }
                fill(pixels, stride, Rect { x: screen * cw, y, w: width * cw, h: ch }, selected_bg);
                draw_text(
                    pixels,
                    stride,
                    font,
                    screen * cw,
                    y,
                    &line.as_bytes()[column..column + width],
                    width,
                    fg(),
                    selected_bg,
                );
            }
        }
    }
    // Caret and pending high digit are scalar UI state; no page read needed.
    if !read_only && current == session.source() {
        let view = session.view();
        if let Some(row) = (view.cursor() / 16).checked_sub(view.top()).filter(|&r| r < view.rows() as u64) {
            let Some(column) = hex_column((view.cursor() % 16) as usize).checked_sub(horizontal) else {
                return true;
            };
            let y = row as usize * ch;
            if y + ch <= height && column + 2 <= columns && (full || dirty(row as usize)) {
                if let Some(high) = view.pending_high_nibble() {
                    let text = format!("{high:X}_");
                    fill(pixels, stride, Rect { x: column * cw, y, w: 2 * cw, h: ch }, bg());
                    draw_text(pixels, stride, font, column * cw, y, text.as_bytes(), 2, fg(), bg());
                }
                let nibble = usize::from(view.pending_high_nibble().is_some());
                fill(pixels, stride, Rect { x: (column + nibble) * cw, y: y + ch - 1, w: cw, h: 1 }, fg());
            }
        }
    }
    true
}
#[cfg(test)]
mod tests {
    use super::*;
    use kwil_view_model::hex_session::{Mutation, Representation};
    fn session() -> Session {
        let mut session = Session::new(3);
        session.bind(Source { document: 1, revision: 0, epoch: 1, len: 0, representation: Representation::RawBytes });
        session
    }
    #[test]
    fn gui_keys_route_raw_bytes_and_consume_hidden_editor_commands() {
        let mut session = session();
        let mods = ModifiersState::empty();
        key(&mut session, &Key::Character("f".into()), mods).unwrap().unwrap();
        let result = key(&mut session, &Key::Character("f".into()), mods).unwrap().unwrap();
        assert_eq!(result.edit.unwrap().mutation, Mutation::Replace { range: 0..0, bytes: vec![255] });
        assert!(command(&mut session, "edit.cut", false).unwrap().unwrap().edit.is_none());
        assert!(key(&mut session, &Key::Character("s".into()), ModifiersState::CONTROL).is_none());
        assert!(key(&mut session, &Key::Named(NamedKey::Enter), mods).unwrap().unwrap().edit.is_none());
    }
    #[test]
    fn gui_ime_commit_is_fenced_and_odd_clipboard_paste_is_atomic() {
        let mut session = session();
        let source = session.source().unwrap();
        let epoch = session.input_epoch();
        assert!(paste(&mut session, source, epoch, "F").is_err());
        assert!(text(&mut session, source, epoch, "F").unwrap().edit.is_none());
        assert!(text(&mut session, source, epoch, "F").is_err());
        let epoch = session.input_epoch();
        assert!(text(&mut session, source, epoch, "F").unwrap().edit.is_some());
    }
}
