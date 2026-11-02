//! Final terminal colour conversion, after every body and overlay has drawn.
use ratatui::{buffer::Buffer, style::Color};

/// Read capabilities once when the terminal opens, never per cell or frame.
pub(super) fn color_count() -> u16 {
    capabilities(
        std::env::var("TERM").ok().as_deref(),
        std::env::var("COLORTERM").ok().as_deref(),
        crossterm::style::available_color_count(),
    )
}

fn capabilities(term: Option<&str>, colorterm: Option<&str>, detected: u16) -> u16 {
    if [term, colorterm].into_iter().flatten().any(|value| value.contains("truecolor") || value.contains("24bit")) {
        u16::MAX
    } else if term.is_some_and(|value| value.contains("256color")) {
        // Honour an explicit terminal capability even on an ANSI Windows console.
        256
    } else {
        detected
    }
}

pub(super) fn apply(buffer: &mut Buffer, colors: u16) {
    if colors > 256 {
        return;
    }
    for cell in &mut buffer.content {
        cell.fg = indexed(cell.fg);
        cell.bg = indexed(cell.bg);
    }
}

fn indexed(color: Color) -> Color {
    let Color::Rgb(r, g, b) = color else { return color };
    // Entries 0..16 are user-configurable. Only the fixed cube and grey ramp
    // have predictable RGB values. Both nearest candidates cost constant work.
    let level = |c: u8| match c {
        0..=47 => 0_u8,
        48..=114 => 1,
        115..=154 => 2,
        155..=194 => 3,
        195..=234 => 4,
        _ => 5,
    };
    let cube = [level(r), level(g), level(b)];
    let intensity = |c: u8| if c == 0 { 0 } else { 55 + i32::from(c) * 40 };
    let distance = |rgb: [i32; 3]| [r, g, b].into_iter().zip(rgb).map(|(a, b)| (i32::from(a) - b).pow(2)).sum::<i32>();
    let grey = ((i32::from(r) + i32::from(g) + i32::from(b) + 15) / 3 - 8).clamp(0, 230) / 10;
    let grey_rgb = 8 + grey * 10;
    Color::Indexed(if distance([grey_rgb; 3]) < distance(cube.map(intensity)) {
        232 + grey as u8
    } else {
        16 + 36 * cube[0] + 6 * cube[1] + cube[2]
    })
}

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
        let buffer = terminal.backend().buffer().clone();
        theme_ui::select(Arc::new(Theme::builtin(Scheme::Dark)));
        buffer
    }

    #[test]
    fn terminal_palette_keeps_custom_theme_rgb_on_truecolor_terminals() {
        assert_eq!(capabilities(Some("xterm-256color"), Some("truecolor"), 8), u16::MAX);
        assert_eq!(capabilities(Some("xterm-24bit"), None, 8), u16::MAX);
        assert_eq!(capabilities(None, None, u16::MAX), u16::MAX);
        let buffer = render(u16::MAX);
        assert_eq!(buffer[(1, 0)].bg, Color::Rgb(18, 52, 86));
        assert_eq!(buffer[(1, 0)].fg, Color::Rgb(175, 215, 255));
        assert_eq!(buffer[(0, 0)].fg, Color::Rgb(168, 215, 160));
        assert_eq!(buffer[(0, 1)].bg, Color::Rgb(128, 128, 128));
    }

    #[test]
    fn terminal_palette_maps_custom_theme_and_syntax_to_fixed_xterm_colours() {
        assert_eq!(capabilities(Some("screen-256color"), Some("color"), u16::MAX), 256);
        assert_eq!(capabilities(None, None, 8), 8);
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
