use super::*;
use crate::{
    theme_cache::{self, CachedTheme},
    theme_parse::{self, ImportFormat},
};

fn native() -> &'static [u8] {
    br##"version=1
name="Fixture"
base="dark"
[ui]
"editor.background"="#1234"
"selection.background"="#abc8"
[syntax.string]
foreground="#12345678"
font_style="bold italic"
[syntax."string.quoted"]
background="#abcdef01"
font_style=""
"##
}
#[test]
fn rgba_alpha_and_remote_background_contract() {
    assert_eq!(Rgba::parse("#1234").unwrap(), Rgba(17, 34, 51, 68));
    assert_eq!(Rgba::parse("#12ab34ef").unwrap(), Rgba(18, 171, 52, 239));
    assert_eq!(Rgba(255, 0, 0, 128).over(Rgba::rgb(0x0000ff)), Rgba(128, 0, 127, 255));
    for invalid in ["fff", "#12", "#12345", "#123456789", "#gg0000", "#é00000"] {
        assert!(Rgba::parse(invalid).is_err(), "{invalid}");
    }
    let t = theme_parse::parse_toml(native()).unwrap();
    let remote = t.for_remote();
    assert_eq!(t.ui.get(UiRole::Background).3, 68, "remote adapter must not mutate persisted alpha");
    assert_eq!(remote.ui.get(UiRole::Background), Rgba(14, 24, 40, 255));
    for role in UiRole::ALL {
        if role.is_background() {
            assert_eq!(remote.ui.get(*role).3, 255);
        }
    }
    assert_eq!(remote.style("string.quoted").background.unwrap().3, 255);
    assert_eq!(remote.style("string.quoted").foreground.unwrap().3, 120);
}
#[test]
fn builtins_and_native_round_trip_contract() {
    let dark = Theme::builtin(Scheme::Dark);
    for (role, expected) in [
        (UiRole::Background, 0x0d1424),
        (UiRole::Foreground, 0xd6e4f0),
        (UiRole::GutterBackground, 0x0a101d),
        (UiRole::GutterForeground, 0x5b6b85),
        (UiRole::StatusBackground, 0x080d18),
        (UiRole::StatusForeground, 0x8fe3ff),
    ] {
        assert_eq!(dark.ui.get(role), Rgba::rgb(expected));
    }
    for t in [dark, Theme::builtin(Scheme::Light), theme_parse::parse_toml(native()).unwrap()] {
        assert_eq!(theme_parse::parse_toml(theme_parse::to_toml(&t).unwrap().as_bytes()).unwrap(), t);
    }
    assert_eq!(theme_parse::parse_toml(include_bytes!("themes/dark.toml")).unwrap(), Theme::builtin(Scheme::Dark));
    assert_eq!(theme_parse::parse_toml(include_bytes!("themes/light.toml")).unwrap(), Theme::builtin(Scheme::Light));
}
#[test]
fn native_parser_bounds_and_strictness_contract() {
    let mut oversized = b"version=1\nname='Oversized'\nbase='dark'\n#".to_vec();
    oversized.resize(MAX_THEME_BYTES + 1, b'x');
    assert!(theme_parse::parse_toml(&oversized).is_err());
    let exactly = format!("version=1\nname='Exact'\nbase='dark'\n#{}", "x".repeat(MAX_THEME_BYTES - 41));
    assert!(exactly.len() <= MAX_THEME_BYTES);
    assert!(theme_parse::parse_toml(exactly.as_bytes()).is_ok());
    for source in [
        "version=2\nname='x'\nbase='dark'",
        "version=1\nname='x'\nbase='dark'\nextra=1",
        "version=1\nname='x'\nbase='dark'\n[ui]\n'unknown.background'='#fff'",
        "version=1\nname='x'\nbase='dark'\n[syntax.'source.rust string']\nforeground='#fff'",
        "version=1\nname=''\nbase='dark'",
        "version=1\nname='x'\nbase='dark'\n[syntax.string]\nfont_style='blink'",
    ] {
        assert!(theme_parse::parse_toml(source.as_bytes()).is_err(), "{source}");
    }
    let mut t = Theme::builtin(Scheme::Dark);
    for n in 0..=MAX_SCOPES {
        t.syntax.insert(format!("scope{n}"), SyntaxStyle::default());
    }
    assert!(t.validate().is_err());
}
#[test]
fn scope_cascade_and_explicit_reset_contract() {
    let t = theme_parse::parse_toml(native()).unwrap();
    let rule = t.style("string.quoted.double");
    assert_eq!(rule.foreground, Some(Rgba(18, 52, 86, 120)));
    assert_eq!(rule.background, Some(Rgba(171, 205, 239, 1)));
    assert_eq!(rule.font_style, Some(0));
    assert_eq!(t.style("stringish"), SyntaxStyle::default());
}
#[test]
fn vscode_import_preserves_order_alpha_and_scopes_contract() {
    let t = theme_parse::import(ImportFormat::VsCodeJson, Scheme::Dark, br##"{"name":"Fixture","type":"light","colors":{"editor.background":"#fff8","statusBar.foreground":"#123"},"tokenColors":[{"scope":["string, comment","constant.numeric"],"settings":{"foreground":"#12345678","fontStyle":"bold italic"}},{"scope":"string","settings":{"background":"#abcdef01","fontStyle":""}}]}"##).unwrap();
    assert_eq!(t.scheme(), Scheme::Light);
    assert_eq!(t.ui.get(UiRole::Background), Rgba(255, 255, 255, 136));
    assert!(t.syntax.contains_key("constant.numeric"));
    assert_eq!(t.style("string").foreground, Some(Rgba(18, 52, 86, 120)));
    assert_eq!(t.style("string").font_style, Some(0));
    assert_eq!(t.style("comment.line").font_style, Some(3));
    assert_eq!(theme_parse::parse_toml(theme_parse::to_toml(&t).unwrap().as_bytes()).unwrap(), t);
}
#[test]
fn vscode_import_rejects_unsupported_instead_of_discarding_contract() {
    for source in [
        r##"{"include":"other.json"}"##,
        r##"{"semanticTokenColors":{}}"##,
        r##"{"colors":{"activityBar.background":"#fff"}}"##,
        r##"{"tokenColors":"other.tmTheme"}"##,
        r##"{"tokenColors":[{"scope":"source.rust string","settings":{"foreground":"#fff"}}]}"##,
        r##"{"tokenColors":[{"scope":"string - comment","settings":{"foreground":"#fff"}}]}"##,
        r##"{"tokenColors":[{"scope":"string","settings":{"fontSize":1.2}}]}"##,
        r##"{"colors":{"editor.background":"#fff","editor.background":"#000"}}"##,
        r##"{/*comment*/"name":"x"}"##,
        r##"{"name":"x",}"##,
    ] {
        assert!(theme_parse::import(ImportFormat::VsCodeJson, Scheme::Dark, source.as_bytes()).is_err(), "{source}");
    }
}
#[test]
fn textmate_import_structure_and_entity_contract() {
    let source = br##"<?xml version="1.0"?><plist version="1.0"><dict><key>name</key><string>A &amp; B</string><key>settings</key><array><dict><key>settings</key><dict><key>background</key><string>#1234</string><key>caret</key><string>#abc</string></dict></dict><dict><key>scope</key><string>string,comment</string><key>settings</key><dict><key>foreground</key><string>#12345678</string><key>fontStyle</key><string>italic</string></dict></dict></array></dict></plist>"##;
    let t = theme_parse::import(ImportFormat::TextMate, Scheme::Dark, source).unwrap();
    assert_eq!(t.name(), "A & B");
    assert_eq!(t.ui.get(UiRole::Background), Rgba(17, 34, 51, 68));
    assert_eq!(t.style("comment.line").foreground, Some(Rgba(18, 52, 86, 120)));
    assert_eq!(t.style("string").font_style, Some(2));
    for source in [
        "<!DOCTYPE plist [<!ENTITY x SYSTEM 'file:///no-read'>]><plist><dict/></plist>",
        "<plist><dict><key>name</key><string>&external;</string></dict></plist>",
        "<plist><dict><key>name</key><string>x</string><key>name</key><string>y</string></dict></plist>",
        "<plist><dict><key>settings</key><string>not an array</string></dict></plist>",
        "<plist><dict><key>settings</key><array><dict><key>scope</key><string>string - comment</string><key>settings</key><dict/></dict></array></dict></plist>",
        "<plist><dict/></plist><plist><dict/></plist>",
    ] {
        assert!(theme_parse::import(ImportFormat::TextMate, Scheme::Dark, source.as_bytes()).is_err(), "{source}");
    }
    let deep = format!("<plist>{}<string>x</string>{}</plist>", "<array>".repeat(18), "</array>".repeat(18));
    assert!(theme_parse::import(ImportFormat::TextMate, Scheme::Dark, deep.as_bytes()).is_err());
}
#[test]
fn preview_cancel_apply_and_os_follow_contract() {
    let mut s = ThemeSession::new(ThemeSelection::follow_os(), Scheme::Dark);
    s.preview(ThemeId::User("fixture".into()));
    s.os_changed(Scheme::Light);
    assert_eq!(s.active_id(), &ThemeId::User("fixture".into()));
    assert_eq!(s.committed_id(), &ThemeId::Light);
    s.cancel();
    assert_eq!(s.active_id(), &ThemeId::Light);
    assert_eq!(s.selection(), &ThemeSelection::follow_os());
    s.preview(ThemeId::Dark);
    s.apply();
    s.os_changed(Scheme::Light);
    assert_eq!(s.selection(), &ThemeSelection::Named(ThemeId::Dark));
    assert_eq!(s.active_id(), &ThemeId::Dark);
}
#[test]
fn binary_cache_validated_round_trip_contract() {
    let cached = CachedTheme {
        selection: ThemeSelection::FollowOs { dark: ThemeId::user("fixture").unwrap(), light: ThemeId::Light },
        theme: theme_parse::parse_toml(native()).unwrap(),
    };
    let bytes = theme_cache::encode(&cached).unwrap();
    assert_eq!(theme_cache::decode(&bytes).unwrap(), cached);
    let mut wrong_version = bytes.clone();
    wrong_version[8] = 2;
    assert!(theme_cache::decode(&wrong_version).is_err());
    let mut damaged = bytes.clone();
    // Change a valid RGBA byte, not a tag that structural validation could
    // reject independently and thereby mask a missing checksum check.
    let color_offset = damaged.windows(4).position(|w| w == [17, 34, 51, 68]).unwrap();
    damaged[color_offset] ^= 1;
    assert!(theme_cache::decode(&damaged).is_err());
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(theme_cache::decode(&trailing).is_err());
    assert!(theme_cache::decode(&bytes[..bytes.len() - 1]).is_err());
    assert!(theme_cache::decode(&vec![0; theme_cache::MAX_CACHE_BYTES + 1]).is_err());
}
#[test]
fn theme_id_cannot_escape_import_directory_contract() {
    for id in ["", "../escape", "C:/elsewhere", "a/b", "a\\b", ".", "a.toml", "é"] {
        assert!(ThemeId::user(id).is_err(), "{id}");
    }
    assert!(ThemeId::User("../escape".into()).validate().is_err());
}
