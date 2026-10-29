//! Terminal theme picker and colour adapter.
use super::*;
use kwil_core_commands::theme::{Theme, UiRole};
use kwil_view_model::{palette_session::PaletteInput, theme_session::ThemeHost};
use std::sync::Arc;
thread_local! { static ACTIVE: std::cell::RefCell<Option<Arc<Theme>>> = const { std::cell::RefCell::new(None) }; }
pub(super) fn select(theme: Arc<Theme>) {
    ACTIVE.with(|active| active.replace(Some(theme)));
}
pub(super) fn color(role: UiRole, fallback: Color) -> Color {
    ACTIVE.with(|active| {
        active.borrow().as_ref().map_or(fallback, |theme| {
            let c = theme.ui().get(role);
            Color::Rgb(c.0, c.1, c.2)
        })
    })
}
pub(super) fn syntax(class: kwil_view_model::highlighting::Class) -> u32 {
    ACTIVE.with(|active| {
        active.borrow().as_ref().map_or_else(
            || kwil_view_model::highlight_session::syntax_rgb(class),
            |theme| kwil_view_model::theme_session::syntax_rgb(theme, class),
        )
    })
}
pub(super) fn base() -> Style {
    Style::default().fg(color(UiRole::Foreground, Color::Reset)).bg(color(UiRole::Background, Color::Reset))
}
pub(super) fn key(
    host: &mut ThemeHost,
    event: KeyEvent,
    panel: Option<&mut kwil_view_model::settings_session::SettingsSession>,
) {
    if event.kind == KeyEventKind::Repeat && matches!(event.code, KeyCode::Enter | KeyCode::Esc) {
        return;
    }
    let text;
    let input = match event.code {
        KeyCode::Esc => PaletteInput::Cancel,
        KeyCode::Enter => PaletteInput::Accept,
        KeyCode::Down => PaletteInput::Next,
        KeyCode::Up => PaletteInput::Previous,
        KeyCode::Left => PaletteInput::Left,
        KeyCode::Right => PaletteInput::Right,
        KeyCode::Home => PaletteInput::Home,
        KeyCode::End => PaletteInput::End,
        KeyCode::Backspace => PaletteInput::Backspace,
        KeyCode::Delete => PaletteInput::Delete,
        KeyCode::Char(c)
            if !event.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            text = c.to_string();
            PaletteInput::Text(&text)
        }
        _ => return,
    };
    host.input(input, panel);
}
pub(super) fn draw(frame: &mut ratatui::Frame<'_>, host: &ThemeHost) {
    if !host.is_open() {
        return;
    }
    let screen = frame.area();
    let width = screen.width.saturating_sub(4).min(90);
    let height = screen.height.saturating_sub(2).min(18);
    if width < 12 || height < 5 {
        return;
    }
    let area = Rect::new((screen.width - width) / 2, (screen.height - height) / 2, width, height);
    let style = Style::default()
        .fg(color(UiRole::PanelForeground, Color::White))
        .bg(color(UiRole::PanelBackground, Color::Black));
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(Paragraph::new("").style(style), area);
    let mut line = |row, text: String, selected| {
        frame.render_widget(
            Paragraph::new(kwil_view_model::palette_session::display_text(&text)).style(if selected {
                style.bg(color(UiRole::Selection, Color::DarkGray))
            } else {
                style
            }),
            Rect::new(area.x + 1, area.y + row, width - 2, 1),
        );
    };
    line(0, if host.importing() { "Import Theme — file path" } else { "Select Theme — live preview" }.into(), false);
    let query = kwil_view_model::palette_session::text_window(host.query(), host.cursor(), (width - 2) as usize);
    line(1, query.text, true);
    if !host.importing() {
        let visible = height.saturating_sub(5) as usize;
        let first = host.selected().saturating_sub(visible.saturating_sub(1));
        for (index, entry) in host.rows().iter().enumerate().skip(first).take(visible) {
            line(
                (index - first + 2) as u16,
                format!("{} ({})", entry.theme.name(), kwil_view_model::theme_session::name(&entry.id)),
                index == host.selected(),
            );
        }
    }
    line(height - 2, host.status.clone(), false);
    line(height - 1, "↑ ↓ preview · Enter save · Esc restore".into(), false);
    frame.set_cursor_position((area.x + 1 + query.cursor as u16, area.y + 1));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn theme_picker_in_terminal_paints_the_preview_and_escape_restores() {
        let mut host = ThemeHost::default();
        assert!(host.command("theme.select", None));
        key(&mut host, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), None);
        select(host.active(true));
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &host)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(4, 1)].bg, Color::Rgb(0xec, 0xf0, 0xf6));
        key(&mut host, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None);
        assert_eq!(host.active(true).scheme(), kwil_core_commands::theme::Scheme::Dark);
        assert!(!host.is_open());
    }
}
