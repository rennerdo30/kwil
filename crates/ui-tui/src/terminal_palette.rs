//! Final terminal colour conversion, after every body and overlay has drawn.
use ratatui::buffer::Buffer;

pub(super) fn apply(_buffer: &mut Buffer, _colors: u16) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme_ui;
    use kwil_core_commands::theme::{Rgba, Scheme, Theme, UiRole};
    use ratatui::{
        Terminal,
        backend::TestBackend,
        style::{Color, Style},
        widgets::Paragraph,
    };
    use std::sync::Arc;

    fn render(colors: u16) -> Buffer {
        let mut theme = Theme::builtin(Scheme::Dark);
        theme.set_ui(UiRole::Background, Rgba(18, 52, 86, 255));
        theme.set_ui(UiRole::Foreground, Rgba(175, 215, 255, 255));
        theme.set_ui(UiRole::PanelBackground, Rgba(128, 128, 128, 255));
        theme_ui::select(Arc::new(theme));
        let mut terminal = Terminal::new(TestBackend::new(8, 2)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new("body").style(theme_ui::base()), frame.area());
                let rgb = theme_ui::syntax(kwil_view_model::highlighting::Class::String);
                frame.render_widget(
                    Paragraph::new("s").style(Style::default().fg(Color::Rgb(
                        (rgb >> 16) as u8,
                        (rgb >> 8) as u8,
                        rgb as u8,
                    ))),
                    ratatui::layout::Rect::new(0, 0, 1, 1),
                );
                frame.render_widget(
                    Paragraph::new("panel")
                        .style(Style::default().bg(theme_ui::color(UiRole::PanelBackground, Color::Reset))),
                    ratatui::layout::Rect::new(0, 1, 8, 1),
                );
                apply(frame.buffer_mut(), colors);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn terminal_palette_keeps_custom_theme_rgb_on_truecolor_terminals() {
        let buffer = render(u16::MAX);
        assert_eq!(buffer[(1, 0)].bg, Color::Rgb(18, 52, 86));
        assert_eq!(buffer[(1, 0)].fg, Color::Rgb(175, 215, 255));
        assert_eq!(buffer[(0, 0)].fg, Color::Rgb(168, 215, 160));
        assert_eq!(buffer[(0, 1)].bg, Color::Rgb(128, 128, 128));
    }

    #[test]
    fn terminal_palette_maps_custom_theme_and_syntax_to_fixed_xterm_colours() {
        let buffer = render(256);
        assert_eq!(buffer[(1, 0)].bg, Color::Indexed(23));
        assert_eq!(buffer[(1, 0)].fg, Color::Indexed(153));
        assert_eq!(buffer[(0, 0)].fg, Color::Indexed(151));
        assert_eq!(buffer[(0, 1)].bg, Color::Indexed(244));
        assert!(
            buffer.content.iter().all(|cell| !matches!(cell.fg, Color::Rgb(..)) && !matches!(cell.bg, Color::Rgb(..)))
        );
    }

    #[test]
    fn terminal_palette_preserves_symbols_modifiers_and_existing_terminal_colours() {
        let mut buffer = Buffer::empty(ratatui::layout::Rect::new(0, 0, 3, 1));
        buffer[(0, 0)].set_symbol("界").set_style(
            Style::default().fg(Color::Indexed(42)).bg(Color::Reset).add_modifier(ratatui::style::Modifier::BOLD),
        );
        buffer[(1, 0)].set_style(Style::default().fg(Color::Red));
        let before = buffer.clone();
        apply(&mut buffer, 256);
        assert_eq!(buffer, before);
    }
}
