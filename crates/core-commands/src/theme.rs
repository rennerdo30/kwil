//! Frontend-independent theme data and selection state. No I/O or parsing is
//! performed by a constructor, selection change, or startup cache read.
use std::{collections::BTreeMap, fmt, sync::Arc};

pub const MAX_THEME_BYTES: usize = 1024 * 1024;
pub const MAX_THEMES: usize = 256;
pub const MAX_SCOPES: usize = 1024;
pub const MAX_NAME_BYTES: usize = 128;
pub const MAX_SCOPE_BYTES: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeError(pub String);
impl fmt::Display for ThemeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ThemeError {}
impl From<std::io::Error> for ThemeError {
    fn from(e: std::io::Error) -> Self {
        Self(e.to_string())
    }
}
pub(crate) fn error(message: impl Into<String>) -> ThemeError {
    ThemeError(message.into())
}

/// Straight (unpremultiplied) sRGB bytes, always RGBA, never platform packed ARGB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);
impl Rgba {
    pub const fn rgb(rgb: u32) -> Self {
        Self((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, 255)
    }
    pub fn parse(value: &str) -> Result<Self, ThemeError> {
        let hex = value.strip_prefix('#').ok_or_else(|| error("colour needs #"))?;
        if !hex.is_ascii() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(error("invalid hex colour"));
        }
        let byte = |s| u8::from_str_radix(s, 16).map_err(|_| error("invalid colour"));
        match hex.len() {
            3 | 4 => {
                let mut c = [255; 4];
                for (i, b) in hex.bytes().enumerate() {
                    c[i] = (b as char).to_digit(16).unwrap() as u8 * 17;
                }
                Ok(Self(c[0], c[1], c[2], c[3]))
            }
            6 | 8 => Ok(Self(
                byte(&hex[0..2])?,
                byte(&hex[2..4])?,
                byte(&hex[4..6])?,
                if hex.len() == 8 { byte(&hex[6..8])? } else { 255 },
            )),
            _ => Err(error("expected #RGB, #RGBA, #RRGGBB or #RRGGBBAA")),
        }
    }
    pub fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}{:02x}", self.0, self.1, self.2, self.3)
    }
    /// Composite over an opaque renderer backing colour, with integer rounding.
    pub fn over(self, backing: Self) -> Self {
        let a = u32::from(self.3);
        let mix = |v, b| ((u32::from(v) * a + u32::from(b) * (255 - a) + 127) / 255) as u8;
        Self(mix(self.0, backing.0), mix(self.1, backing.1), mix(self.2, backing.2), 255)
    }
}

macro_rules! roles {
    ($($variant:ident => $name:literal),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[repr(u8)]
        pub enum UiRole { $($variant),+ }
        impl UiRole {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
            pub fn key(self) -> &'static str { match self { $(Self::$variant => $name),+ } }
            pub fn from_key(key: &str) -> Option<Self> { match key { $($name => Some(Self::$variant)),+, _ => None } }
            pub fn is_background(self) -> bool { self.key().ends_with(".background") }
        }
    }
}
roles! {
    Background => "editor.background", Foreground => "editor.foreground",
    GutterBackground => "gutter.background", GutterForeground => "gutter.foreground",
    StatusBackground => "status.background", StatusForeground => "status.foreground",
    Selection => "selection.background", Cursor => "cursor.foreground",
    LineHighlight => "line.background", Whitespace => "whitespace.foreground",
    TabBackground => "tab.background", TabForeground => "tab.foreground",
    ActiveTabBackground => "tab.active.background", ActiveTabForeground => "tab.active.foreground",
    PanelBackground => "panel.background", PanelForeground => "panel.foreground",
    Border => "border.foreground", Accent => "accent.foreground",
    SearchMatch => "search.background", Added => "diff.added.background",
    Removed => "diff.removed.background", Error => "error.foreground",
}
pub const UI_ROLE_COUNT: usize = UiRole::ALL.len();

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiColors(pub(crate) [Rgba; UI_ROLE_COUNT]);
impl UiColors {
    pub fn get(&self, role: UiRole) -> Rgba {
        self.0[role as usize]
    }
    pub fn set(&mut self, role: UiRole, color: Rgba) {
        self.0[role as usize] = color;
    }
}

/// None inherits; Some(0) explicitly clears inherited font flags.
/// Flags: bold=1, italic=2, underline=4, strikethrough=8.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyntaxStyle {
    pub foreground: Option<Rgba>,
    pub background: Option<Rgba>,
    pub font_style: Option<u8>,
}
impl SyntaxStyle {
    pub(crate) fn overlay(&mut self, other: &Self) {
        if other.foreground.is_some() {
            self.foreground = other.foreground;
        }
        if other.background.is_some() {
            self.background = other.background;
        }
        if other.font_style.is_some() {
            self.font_style = other.font_style;
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    Dark,
    Light,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    pub(crate) name: String,
    pub(crate) scheme: Scheme,
    pub(crate) ui: UiColors,
    pub(crate) syntax: BTreeMap<String, SyntaxStyle>,
}
impl Theme {
    /// Static built-ins share the native TOML schema; these constructors do not
    /// parse that schema and are safe before the first frame.
    pub fn builtin(scheme: Scheme) -> Self {
        let (name, colors): (&str, [u32; UI_ROLE_COUNT]) = match scheme {
            Scheme::Dark => (
                "dark",
                [
                    0x0d1424, 0xd6e4f0, 0x0a101d, 0x5b6b85, 0x080d18, 0x8fe3ff, 0x305880, 0xd6e4f0, 0x152038, 0x5b6b85,
                    0x0a101d, 0x8b9bb2, 0x0d1424, 0xd6e4f0, 0x0a101d, 0xd6e4f0, 0x2a3a54, 0x8fe3ff, 0x695522, 0x183d30,
                    0x482832, 0xff8080,
                ],
            ),
            Scheme::Light => (
                "light",
                [
                    0xf7f9fc, 0x202c40, 0xecf0f6, 0x65738a, 0xe7edf5, 0x164e70, 0xc9dff5, 0x202c40, 0xeef2f8, 0x758399,
                    0xecf0f6, 0x526178, 0xf7f9fc, 0x202c40, 0xecf0f6, 0x202c40, 0xc2ccda, 0x165b85, 0xf4e0a1, 0xd8eee0,
                    0xf5dce0, 0xb52d40,
                ],
            ),
        };
        let scope_colors = match scheme {
            Scheme::Dark => [
                0xc5a3ff, 0xa8d7a0, 0x74869c, 0xf1bf8a, 0x8fd8d0, 0x8fe3ff, 0xd6e4f0, 0xf1bf8a, 0xa7b9d0, 0x8b9bb2,
                0xc5a3ff, 0x8fd8d0, 0xd6e4f0, 0x8fe3ff, 0x8fe3ff, 0xff8080,
            ],
            Scheme::Light => [
                0x7042a0, 0x356b30, 0x66748a, 0x93511b, 0x256f68, 0x165b85, 0x202c40, 0x93511b, 0x465b78, 0x526178,
                0x7042a0, 0x256f68, 0x202c40, 0x165b85, 0x165b85, 0xb52d40,
            ],
        };
        let names = [
            "keyword",
            "string",
            "comment",
            "number",
            "type",
            "function",
            "variable",
            "constant",
            "operator",
            "punctuation",
            "tag",
            "attribute",
            "property",
            "heading",
            "link",
            "error",
        ];
        let syntax = names
            .into_iter()
            .zip(scope_colors)
            .map(|(name, c)| (name.into(), SyntaxStyle { foreground: Some(Rgba::rgb(c)), ..SyntaxStyle::default() }))
            .collect();
        Self { name: name.into(), scheme, ui: UiColors(colors.map(Rgba::rgb)), syntax }
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn scheme(&self) -> Scheme {
        self.scheme
    }
    pub fn ui(&self) -> &UiColors {
        &self.ui
    }
    pub fn syntax(&self) -> &BTreeMap<String, SyntaxStyle> {
        &self.syntax
    }
    /// Single dotted scope only. Ancestor rules cascade; component boundaries
    /// prevent "string" matching "stringish". Context selectors are rejected.
    pub fn style(&self, scope: &str) -> SyntaxStyle {
        let mut result = SyntaxStyle::default();
        if scope.len() > MAX_SCOPE_BYTES {
            return result;
        }
        for (end, _) in scope.match_indices('.').chain(std::iter::once((scope.len(), ""))) {
            if let Some(rule) = self.syntax.get(&scope[..end]) {
                result.overlay(rule);
            }
        }
        result
    }
    /// Renderer adaptation, leaving persisted RGBA intact. Remote surfaces and
    /// syntax backgrounds are composited to opaque bytes over the editor base.
    pub fn for_remote(&self) -> Self {
        let mut theme = self.clone();
        let backing = self.ui.get(UiRole::Background).over(Self::builtin(self.scheme).ui.get(UiRole::Background));
        for role in UiRole::ALL {
            if role.is_background() {
                theme
                    .ui
                    .set(*role, if *role == UiRole::Background { backing } else { self.ui.get(*role).over(backing) });
            }
        }
        for style in theme.syntax.values_mut() {
            style.background = style.background.map(|c| c.over(backing));
        }
        theme
    }
    pub(crate) fn validate(&self) -> Result<(), ThemeError> {
        if self.name.trim().is_empty() || self.name.len() > MAX_NAME_BYTES || self.name.chars().any(char::is_control) {
            return Err(error("theme name must be nonempty, printable and <=128 bytes"));
        }
        if self.syntax.len() > MAX_SCOPES {
            return Err(error("too many syntax scopes"));
        }
        for (scope, style) in &self.syntax {
            validate_scope(scope)?;
            if style.font_style.is_some_and(|flags| flags > 15) {
                return Err(error("invalid font flags"));
            }
        }
        Ok(())
    }
}
pub(crate) fn validate_scope(scope: &str) -> Result<(), ThemeError> {
    if scope.len() > MAX_SCOPE_BYTES
        || scope
            .split('.')
            .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
    {
        return Err(error(format!("unsupported scope selector: {scope}; only dotted scopes are supported")));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThemeId {
    Dark,
    Light,
    User(String),
}
impl ThemeId {
    pub fn user(id: &str) -> Result<Self, ThemeError> {
        if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            return Err(error("theme id must be 1..64 ASCII letters, digits, _ or -"));
        }
        Ok(Self::User(id.into()))
    }
    pub(crate) fn validate(&self) -> Result<(), ThemeError> {
        if let Self::User(id) = self {
            Self::user(id)?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThemeSelection {
    Named(ThemeId),
    FollowOs { dark: ThemeId, light: ThemeId },
}
impl ThemeSelection {
    pub fn follow_os() -> Self {
        Self::FollowOs { dark: ThemeId::Dark, light: ThemeId::Light }
    }
    pub fn resolve(&self, scheme: Scheme) -> &ThemeId {
        match self {
            Self::Named(id) => id,
            Self::FollowOs { dark, light } => {
                if scheme == Scheme::Dark {
                    dark
                } else {
                    light
                }
            }
        }
    }
    pub(crate) fn validate(&self) -> Result<(), ThemeError> {
        match self {
            Self::Named(id) => id.validate(),
            Self::FollowOs { dark, light } => {
                dark.validate()?;
                light.validate()
            }
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeSession {
    selection: ThemeSelection,
    preview: Option<ThemeId>,
    scheme: Scheme,
}
impl ThemeSession {
    pub fn new(selection: ThemeSelection, scheme: Scheme) -> Self {
        Self { selection, preview: None, scheme }
    }
    pub fn selection(&self) -> &ThemeSelection {
        &self.selection
    }
    pub fn active_id(&self) -> &ThemeId {
        self.preview.as_ref().unwrap_or_else(|| self.selection.resolve(self.scheme))
    }
    pub fn committed_id(&self) -> &ThemeId {
        self.selection.resolve(self.scheme)
    }
    pub fn preview(&mut self, id: ThemeId) {
        self.preview = Some(id);
    }
    pub fn cancel(&mut self) {
        self.preview = None;
    }
    pub fn apply(&mut self) {
        if let Some(id) = self.preview.take() {
            self.selection = ThemeSelection::Named(id);
        }
    }
    pub fn select(&mut self, selection: ThemeSelection) {
        self.selection = selection;
        self.preview = None;
    }
    pub fn os_changed(&mut self, scheme: Scheme) {
        self.scheme = scheme;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeEntry {
    pub id: ThemeId,
    pub theme: Arc<Theme>,
}

#[cfg(test)]
#[path = "theme_tests.rs"]
mod tests;
