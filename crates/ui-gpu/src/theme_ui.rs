//! Chrome colours. The renderer paints the editor with the built-in dark
//! palette, so menus, tabs, status and scrollbars read the same built-in theme;
//! no theme files are parsed and nothing here touches the startup path's I/O.
use kwil_core_commands::theme::{Rgba, Scheme, Theme, UiRole};

pub(super) fn chrome() -> &'static Theme {
    static THEME: std::sync::OnceLock<Theme> = std::sync::OnceLock::new();
    THEME.get_or_init(|| Theme::builtin(Scheme::Dark))
}

pub(super) fn rgb(theme: &Theme, role: UiRole) -> u32 {
    let Rgba(r, g, b, _) = theme.ui().get(role);
    (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_palette_matches_the_painted_editor_colours() {
        // A mismatch would put a differently coloured frame around the text.
        let theme = chrome();
        for (role, painted) in [
            (UiRole::Background, crate::BG),
            (UiRole::Foreground, crate::FG),
            (UiRole::GutterBackground, crate::GUTTER_BG),
            (UiRole::GutterForeground, crate::GUTTER_FG),
            (UiRole::StatusBackground, crate::STATUS_BG),
            (UiRole::StatusForeground, crate::STATUS_FG),
        ] {
            assert_eq!(rgb(theme, role), painted, "{role:?}");
        }
    }
}
