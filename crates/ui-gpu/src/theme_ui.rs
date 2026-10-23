//! Chrome colours. The renderer paints the editor with the built-in dark
//! palette, so menus, tabs, status and scrollbars read the same built-in theme;
//! no theme files are parsed and nothing here touches the startup path's I/O.
use kwil_core_commands::theme::{Rgba, Scheme, Theme, UiRole};

pub(super) fn chrome() -> &'static Theme {
    static DARK: std::sync::OnceLock<Theme> = std::sync::OnceLock::new();
    static LIGHT: std::sync::OnceLock<Theme> = std::sync::OnceLock::new();
    if DARK_SELECTED.with(std::cell::Cell::get) {
        DARK.get_or_init(|| Theme::builtin(Scheme::Dark))
    } else {
        LIGHT.get_or_init(|| Theme::builtin(Scheme::Light))
    }
}

thread_local! { static DARK_SELECTED: std::cell::Cell<bool> = const { std::cell::Cell::new(true) }; }

pub(super) fn select(dark: bool) -> bool {
    DARK_SELECTED.with(|selected| selected.replace(dark) != dark)
}

pub(super) fn syntax_rgb(color: u32) -> u32 {
    if DARK_SELECTED.with(std::cell::Cell::get) { color } else { super::chrome::mix(color, 0, 0x78) }
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
            (UiRole::Background, crate::bg()),
            (UiRole::Foreground, crate::fg()),
            (UiRole::GutterBackground, crate::gutter_bg()),
            (UiRole::GutterForeground, crate::gutter_fg()),
            (UiRole::StatusBackground, crate::status_bg()),
            (UiRole::StatusForeground, crate::status_fg()),
        ] {
            assert_eq!(rgb(theme, role), painted, "{role:?}");
        }
    }
}
