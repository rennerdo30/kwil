//! Menu bar. Every item is a registry command dispatched through the same
//! path as its key binding or palette entry; labels show the user's actual
//! keymap. Alt or F10 focuses the bar, arrows/Enter/Escape navigate, and a
//! click outside dismisses it. Drawing is damage-only like the palette card.
use super::*;
use chrome::{Hover, Metrics};
use kwil_core_commands::{Binding, Context, KeyChord, KeySequence, Keymap, Mode, Preset, Resolution};
use kwil_view_model::TabArea;
use std::collections::HashMap;

/// Frontend-local action: shows the version in the status bar.
pub(super) const ABOUT: &str = "kwil.about";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Entry {
    /// Label with `&` before its mnemonic, and a command id.
    Item(&'static str, &'static str),
    Separator,
}

pub(super) struct Menu {
    pub title: &'static str,
    pub entries: &'static [Entry],
}

use Entry::{Item, Separator};
pub(super) const MENUS: &[Menu] = &[
    Menu {
        title: "&File",
        entries: &[
            Item("&New File", "file.new"),
            Item("&Open File\u{2026}", "file.open"),
            Item("Open as &Hex\u{2026}", "file.openHex"),
            Item("Open &Recent\u{2026}", "file.recent"),
            Item("Reopen Close&d Tab", "file.reopenClosed"),
            Separator,
            Item("&Save", "file.save"),
            Item("Save &As\u{2026}", "file.saveAs"),
            Item("Save A&ll", "file.saveAll"),
            Item("Change File T&ype\u{2026}", "file.setType"),
            Separator,
            Item("&Close Tab", "file.close"),
            Item("Clos&e All Tabs", "file.closeAll"),
            Separator,
            Item("Se&ttings", "view.settings"),
            Item("Open Settings F&ile", "view.settingsFile"),
            Separator,
            Item("E&xit", "file.quit"),
        ],
    },
    Menu {
        title: "&Edit",
        entries: &[
            Item("&Undo", "edit.undo"),
            Item("&Redo", "edit.redo"),
            Separator,
            Item("Cu&t", "edit.cut"),
            Item("&Copy", "edit.copy"),
            Item("&Paste", "edit.paste"),
            Separator,
            Item("Toggle Line Co&mment", "edit.toggleComment"),
            Item("&Duplicate Line", "edit.duplicateLine"),
            Item("Mo&ve Line Up", "edit.moveLineUp"),
            Item("Move Line Do&wn", "edit.moveLineDown"),
            Item("De&lete Line", "edit.deleteLine"),
            Separator,
            Item("&Indent", "edit.indent"),
            Item("&Outdent", "edit.outdent"),
            Item("Trim Trailing W&hitespace", "edit.trimTrailingWhitespace"),
        ],
    },
    Menu {
        title: "&Selection",
        entries: &[
            Item("Select &All", "edit.selectAll"),
            Item("Select &Line", "edit.selectLine"),
            Item("Select &Word", "edit.selectWord"),
            Separator,
            Item("Add &Next Occurrence", "edit.selectNextMatch"),
            Item("Add Cursor A&bove", "edit.addCursorAbove"),
            Item("Add Cursor B&elow", "edit.addCursorBelow"),
        ],
    },
    Menu {
        title: "&View",
        entries: &[
            Item("&Command Palette\u{2026}", "view.commandPalette"),
            Separator,
            Item("Zoom &In", "view.zoomIn"),
            Item("Zoom &Out", "view.zoomOut"),
            Item("&Reset Zoom", "view.zoomReset"),
            Separator,
            Item("&Log Filters", "view.logFilters"),
            Item("Toggle &Hex View", "view.toggleHex"),
            Item("Markdown &Text", "view.markdownText"),
            Item("Markdown &Preview", "view.markdownPreview"),
            Item("Markdown &Split", "view.markdownSplit"),
        ],
    },
    Menu {
        title: "Sea&rch",
        entries: &[
            Item("&Find\u{2026}", "search.find"),
            Item("&Replace\u{2026}", "search.replace"),
            Item("Replace &All", "search.replaceAll"),
            Separator,
            Item("Go to &Line\u{2026}", "navigation.goToLine"),
            Item("&Go to File\u{2026}", "view.fileFinder"),
            Item("Go to Matching &Bracket", "navigation.matchBracket"),
        ],
    },
    Menu {
        title: "&Help",
        entries: &[
            Item("Show All &Commands\u{2026}", "view.commandPalette"),
            Item("Check for &Updates", "update.check"),
            Separator,
            Item("&About Kwil", ABOUT),
        ],
    },
];

pub(super) fn text(label: &str) -> String {
    label.replace('&', "")
}

/// Mnemonic character (lowercase) and its index in the display text.
pub(super) fn mnemonic(label: &str) -> Option<(char, usize)> {
    let at = label.find('&')?;
    let c = label[at + 1..].chars().next()?;
    Some((c.to_ascii_lowercase(), label[..at].chars().count()))
}

pub(super) fn title_for(c: char) -> Option<usize> {
    let c = c.to_ascii_lowercase();
    MENUS.iter().position(|menu| mnemonic(menu.title).is_some_and(|(m, _)| m == c))
}

/// What Alt+letter does, open menu or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AltLetter {
    /// The keymap binds or explicitly unbinds the chord; it runs as a shortcut.
    Keymap,
    Open(usize),
    Nothing,
}

pub(super) fn alt_letter(keymap_owns: bool, c: char) -> AltLetter {
    if keymap_owns { AltLetter::Keymap } else { title_for(c).map_or(AltLetter::Nothing, AltLetter::Open) }
}

/// Snapshot of what the GUI can run right now.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Available {
    pub editor: bool,
    pub markdown: bool,
    pub log: bool,
    pub selection: bool,
    pub palette: bool,
    pub settings: bool,
    pub recent: bool,
    pub closed: bool,
    pub dirty: bool,
    /// The fixed search worker and current Editor snapshot are available.
    pub search: bool,
    pub zoom_in: bool,
    pub zoom_out: bool,
    pub zoom_reset: bool,
}

pub(super) fn enabled(available: &Available, command: &str) -> bool {
    match command {
        "view.logFilters" => available.log,
        "view.toggleMarkdown" | "view.markdownText" | "view.markdownPreview" | "view.markdownSplit" => {
            available.markdown
        }
        "search.find" | "search.replace" | "search.replaceAll" => available.search,
        "edit.cut" | "edit.copy" => available.editor && available.selection,
        "file.recent" => available.recent,
        "file.reopenClosed" => available.closed,
        "file.saveAll" => available.dirty,
        "view.settings" | "view.settingsFile" => available.settings,
        "navigation.goToLine" => true,
        "view.commandPalette" | "view.fileFinder" | "theme.select" => available.palette,
        "view.zoomIn" => available.zoom_in,
        "view.zoomOut" => available.zoom_out,
        "view.zoomReset" => available.zoom_reset,
        command if command.starts_with("edit.") || command.starts_with("navigation.") => available.editor,
        _ => true,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum State {
    #[default]
    Closed,
    /// Keyboard focus on a title without an open list (after Alt or F10).
    Bar(usize),
    Open {
        menu: usize,
        item: Option<usize>,
        keyboard: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MenuKey {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Enter,
    Escape,
    Char(char),
}

fn step(entries: &[Entry], from: Option<usize>, forward: bool, enabled: &dyn Fn(&str) -> bool) -> Option<usize> {
    let count = entries.len();
    let usable = |i: usize| matches!(entries[i], Item(_, command) if enabled(command));
    let start = match (from, forward) {
        (Some(i), true) => i + 1,
        (Some(i), false) => i + count - 1,
        (None, true) => 0,
        (None, false) => count - 1,
    };
    (0..count).map(|k| if forward { (start + k) % count } else { (start + count - k) % count }).find(|&i| usable(i))
}

impl State {
    pub fn is_open(self) -> bool {
        self != State::Closed
    }
    pub fn menu(self) -> Option<usize> {
        match self {
            State::Closed => None,
            State::Bar(menu) | State::Open { menu, .. } => Some(menu),
        }
    }
    /// Open `menu` from the keyboard with its first usable item highlighted.
    pub fn open(menu: usize, enabled: &dyn Fn(&str) -> bool) -> State {
        State::Open { menu, item: step(MENUS[menu].entries, None, true, enabled), keyboard: true }
    }
    /// Returns the next state and, for an activation, the command to run.
    pub fn key(self, key: MenuKey, enabled: &dyn Fn(&str) -> bool) -> (State, Option<&'static str>) {
        let count = MENUS.len();
        match self {
            State::Closed => (self, None),
            State::Bar(menu) => match key {
                MenuKey::Left => (State::Bar((menu + count - 1) % count), None),
                MenuKey::Right => (State::Bar((menu + 1) % count), None),
                MenuKey::Home => (State::Bar(0), None),
                MenuKey::End => (State::Bar(count - 1), None),
                MenuKey::Down | MenuKey::Enter => (State::open(menu, enabled), None),
                MenuKey::Up => {
                    (State::Open { menu, item: step(MENUS[menu].entries, None, false, enabled), keyboard: true }, None)
                }
                MenuKey::Escape => (State::Closed, None),
                MenuKey::Char(c) => (title_for(c).map_or(self, |menu| State::open(menu, enabled)), None),
            },
            State::Open { menu, item, keyboard } => {
                let entries = MENUS[menu].entries;
                let at = |item| State::Open { menu, item, keyboard: true };
                match key {
                    MenuKey::Down => (at(step(entries, item, true, enabled)), None),
                    MenuKey::Up => (at(step(entries, item, false, enabled)), None),
                    MenuKey::Home => (at(step(entries, None, true, enabled)), None),
                    MenuKey::End => (at(step(entries, None, false, enabled)), None),
                    MenuKey::Left => (State::open((menu + count - 1) % count, enabled), None),
                    MenuKey::Right => (State::open((menu + 1) % count, enabled), None),
                    MenuKey::Enter => match item.map(|i| entries[i]) {
                        Some(Item(_, command)) if enabled(command) => (State::Closed, Some(command)),
                        _ => (self, None),
                    },
                    MenuKey::Escape => (if keyboard { State::Bar(menu) } else { State::Closed }, None),
                    MenuKey::Char(c) => {
                        let c = c.to_ascii_lowercase();
                        let hit = entries.iter().find_map(|entry| match entry {
                            Item(label, command) if mnemonic(label).is_some_and(|(m, _)| m == c) => Some(*command),
                            _ => None,
                        });
                        match hit {
                            Some(command) if enabled(command) => (State::Closed, Some(command)),
                            _ => (self, None),
                        }
                    }
                }
            }
        }
    }
}

/// The user's keymap, for shortcut labels and for leaving bound Alt chords
/// to their commands. Rebuilt only when the keymap settings change.
#[derive(Default)]
pub(super) struct Keys {
    source: Option<(Preset, Vec<Binding>)>,
    keymap: Option<Keymap>,
}

impl Keys {
    fn keymap(&mut self, preset: Preset, bindings: &[Binding]) -> Option<&Keymap> {
        if self.source.as_ref().is_none_or(|(p, b)| *p != preset || b != bindings) {
            self.source = Some((preset, bindings.to_vec()));
            self.keymap = Keymap::preset(preset).and_then(|keymap| keymap.with_overrides(bindings)).ok();
        }
        self.keymap.as_ref()
    }

    /// Whether the keymap (including an explicit unbind) owns `chord`.
    pub fn owns(&mut self, preset: Preset, bindings: &[Binding], mode: Mode, chord: KeyChord) -> bool {
        let Some(keymap) = self.keymap(preset, bindings) else { return true };
        match keymap.resolve(&[chord], Context::Editor, mode) {
            Resolution::Command(_) | Resolution::Pending => true,
            Resolution::Unbound => keymap.explicitly_unbound(&[chord], Context::Editor, mode),
        }
    }

    /// Shortcut label for each menu command that a key actually reaches.
    pub fn labels(&mut self, preset: Preset, bindings: &[Binding], mode: Mode) -> HashMap<&'static str, String> {
        let mut labels = HashMap::new();
        let Some(keymap) = self.keymap(preset, bindings) else { return labels };
        for menu in MENUS {
            for entry in menu.entries {
                let Item(_, command) = *entry else { continue };
                let best = keymap
                    .bindings()
                    .filter(|b| b.command == command)
                    .filter(|b| b.context.is_none_or(|c| c == Context::Editor) && b.mode.is_none_or(|m| m == mode))
                    .filter_map(|b| Some((b, b.keys.parse::<KeySequence>().ok()?)))
                    .filter(|(_, sequence)| {
                        keymap.resolve(&sequence.0, Context::Editor, mode) == Resolution::Command(command)
                    })
                    .min_by_key(|(b, sequence)| {
                        let modifiers = sequence
                            .0
                            .iter()
                            .map(|c| {
                                c.modifiers.ctrl as usize
                                    + c.modifiers.alt as usize
                                    + c.modifiers.shift as usize
                                    + c.modifiers.super_key as usize
                            })
                            .sum::<usize>();
                        (sequence.0.len(), b.keys.to_ascii_lowercase().contains("plus"), modifiers)
                    });
                if let Some((binding, _)) = best {
                    labels.insert(command, pretty(&binding.keys));
                }
            }
        }
        labels
    }
}

/// "ctrl+shift+p" → "Ctrl+Shift+P"; chords are separated by a space.
pub(super) fn pretty(keys: &str) -> String {
    keys.split_whitespace()
        .map(|chord| {
            chord
                .split('+')
                .filter(|part| !part.is_empty())
                .map(|part| match part.to_ascii_lowercase().as_str() {
                    "ctrl" | "control" => "Ctrl".to_owned(),
                    "shift" => "Shift".to_owned(),
                    "alt" | "option" => "Alt".to_owned(),
                    "cmd" | "super" | "meta" | "win" => {
                        (if cfg!(target_os = "macos") { "Cmd" } else { "Win" }).to_owned()
                    }
                    "plus" => "Plus".to_owned(),
                    "escape" | "esc" => "Esc".to_owned(),
                    "pageup" => "PgUp".to_owned(),
                    "pagedown" => "PgDn".to_owned(),
                    "delete" => "Del".to_owned(),
                    other => {
                        let mut chars = other.chars();
                        chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
                    }
                })
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Menu bar state plus the geometry it was last drawn with, so hits always
/// match the pixels on screen.
#[derive(Default)]
pub(super) struct MenuBar {
    pub toolbar: Vec<Tool>,
    pub toolbar_surface: Option<(usize, usize, u64)>,
    pub toolbar_hover: Option<&'static str>,
    pub state: State,
    /// Mnemonic underlines, shown while navigating with the keyboard.
    pub cues: bool,
    pub alt_tap: bool,
    pub keys: Keys,
    pub labels: HashMap<&'static str, String>,
    pub titles: Vec<TabArea>,
    pub dropdown: Option<Dropdown>,
    /// First visible entry of a list taller than the window.
    pub scroll: usize,
    /// Keyboard moved the highlight; the next paint scrolls it into view.
    pub reveal: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Tool {
    pub rect: Rect,
    pub icon: icons::Icon,
    pub command: &'static str,
    pub enabled: bool,
    pub selected: bool,
    pub label: Option<&'static str>,
}
pub(super) fn toolbar(area: TabArea, titles: &[TabArea], m: &Metrics, available: &Available) -> Vec<Tool> {
    let first = titles.last().map_or(area.x, |r| r.x + r.width).saturating_add(m.gap);
    let width = m.dip(30.0);
    let mut end = area.x + area.width;
    let mut tools = Vec::with_capacity(7);
    for (icon, command) in [
        (icons::Icon::Settings, "view.settings"),
        (icons::Icon::Search, "search.find"),
        (icons::Icon::GoTo, "navigation.goToLine"),
        (icons::Icon::Redo, "edit.redo"),
        (icons::Icon::Undo, "edit.undo"),
        (icons::Icon::Save, "file.save"),
        (icons::Icon::Open, "file.open"),
    ] {
        if end.saturating_sub(first) < width {
            break;
        }
        end -= width;
        tools.push(Tool {
            rect: Rect { x: end, y: area.y, w: width, h: area.height },
            icon,
            command,
            enabled: enabled(available, command),
            selected: false,
            label: None,
        });
    }
    tools
}

pub(super) fn markdown_tools(
    tools: &mut Vec<Tool>,
    area: TabArea,
    titles: &[TabArea],
    m: &Metrics,
    mode: markdown_ui::Mode,
) {
    let start = titles.last().map_or(area.x, |r| r.x + r.width).saturating_add(m.gap);
    let end = tools.iter().map(|tool| tool.rect.x).min().unwrap_or(area.x + area.width).saturating_sub(m.gap);
    let available = end.saturating_sub(start);
    let labels = available >= m.dip(260.0);
    let width = if labels { m.dip(84.0) } else { m.dip(30.0) };
    if available < width * 3 {
        return;
    }
    let mut x = end - width * 3;
    for (choice, label, icon, command) in [
        (markdown_ui::Mode::Text, "Text", icons::Icon::Text, "view.markdownText"),
        (markdown_ui::Mode::Preview, "Preview", icons::Icon::Markdown, "view.markdownPreview"),
        (markdown_ui::Mode::Split, "Split", icons::Icon::Split, "view.markdownSplit"),
    ] {
        tools.push(Tool {
            rect: Rect { x, y: area.y, w: width, h: area.height },
            icon,
            command,
            enabled: true,
            selected: choice == mode,
            label: labels.then_some(label),
        });
        x += width;
    }
}

impl MenuBar {
    pub fn refresh_labels(&mut self, preset: Preset, bindings: &[Binding], mode: Mode) {
        self.labels = self.keys.labels(preset, bindings, mode);
    }
    /// Returns true when an open menu must repaint with the new labels.
    /// Closed menus refresh lazily when they open.
    pub fn settings_changed(&mut self, preset: Preset, bindings: &[Binding], mode: Mode) -> bool {
        if !self.state.is_open() {
            return false;
        }
        let labels = self.keys.labels(preset, bindings, mode);
        let changed = labels != self.labels;
        self.labels = labels;
        changed
    }
}

/// An open menu owns keyboard input, so IME text never reaches the document
/// behind it; otherwise the usual capture decides.
pub(super) fn ime_target(menu_open: bool, capture: impl FnOnce() -> ime::Destination) -> ime::Destination {
    if menu_open { ime::Destination::Cancelled } else { capture() }
}

/// Geometry of one drawn list. It names its menu, so a pointer event that
/// arrives after the state changed but before the next paint is rejected.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Dropdown {
    pub menu: usize,
    pub area: Rect,
    /// Entry index and its row rectangle, for visible items only.
    pub rows: Vec<(usize, Rect)>,
    pub separators: Vec<Rect>,
    /// First visible entry and the largest useful value.
    pub scroll: usize,
    pub max_scroll: usize,
    /// Scroll indicator strips, present while content is hidden that way.
    pub up: Option<TabArea>,
    pub down: Option<TabArea>,
}

impl Dropdown {
    /// The item under `pointer`, only if this geometry belongs to the open menu.
    pub fn row_at(&self, state: State, pointer: (usize, usize)) -> Option<(usize, &'static str)> {
        let State::Open { menu, .. } = state else { return None };
        if menu != self.menu {
            return None;
        }
        let (index, _) = self.rows.iter().find(|(_, rect)| rect.contains(pointer))?;
        match MENUS.get(menu)?.entries.get(*index)? {
            Item(_, command) => Some((*index, command)),
            Separator => None,
        }
    }
}

fn entry_height(entry: &Entry, m: &Metrics) -> usize {
    if *entry == Separator { m.separator } else { m.item }
}

/// Vertical room for entries when the list would not fit below the bar, or
/// None when every entry fits.
fn scroll_room(menu: usize, y: usize, window_h: usize, m: &Metrics) -> Option<usize> {
    let entries = MENUS[menu].entries;
    let full = 2 * m.radius + entries.iter().map(|e| entry_height(e, m)).sum::<usize>();
    let limit = window_h.saturating_sub(y);
    (full > limit).then(|| limit.saturating_sub(2 * m.radius + 2 * arrow_height(m)))
}

fn arrow_height(m: &Metrics) -> usize {
    m.dip(14.0)
}

/// Smallest first entry from which the rest of the list fits in `room`.
fn max_scroll(menu: usize, room: usize, m: &Metrics) -> usize {
    let entries = MENUS[menu].entries;
    let mut used = 0;
    for (index, entry) in entries.iter().enumerate().rev() {
        used += entry_height(entry, m);
        if used > room {
            return (index + 1).min(entries.len() - 1);
        }
    }
    0
}

/// Scroll position that keeps `item` fully visible, changing `scroll` as little
/// as possible.
pub(super) fn reveal(menu: usize, item: usize, scroll: usize, y: usize, window_h: usize, m: &Metrics) -> usize {
    let Some(room) = scroll_room(menu, y, window_h, m) else { return 0 };
    let entries = MENUS[menu].entries;
    let mut scroll = scroll.min(item);
    while scroll < item && entries[scroll..=item].iter().map(|e| entry_height(e, m)).sum::<usize>() > room {
        scroll += 1;
    }
    scroll.min(max_scroll(menu, room, m))
}

/// Row geometry for a list of `width` at `(x, y)`; no font needed.
pub(super) fn layout_rows(
    menu: usize,
    (x, y): (usize, usize),
    width: usize,
    window_h: usize,
    m: &Metrics,
    scroll: usize,
) -> Dropdown {
    let entries = MENUS[menu].entries;
    let frame = m.radius;
    let full = 2 * frame + entries.iter().map(|e| entry_height(e, m)).sum::<usize>();
    let height = full.min(window_h.saturating_sub(y));
    let area = Rect { x, y, w: width, h: height };
    let room = scroll_room(menu, y, window_h, m);
    let (scroll, max_scroll, arrow) = match room {
        Some(room) => {
            let max = max_scroll(menu, room, m);
            (scroll.min(max), max, arrow_height(m))
        }
        None => (0, 0, 0),
    };
    let inner_w = width.saturating_sub(2 * frame);
    let mut dropdown = Dropdown { menu, area, scroll, max_scroll, ..Default::default() };
    if inner_w == 0 || height <= 2 * frame + 2 * arrow {
        return dropdown;
    }
    let bottom = y + height - frame - arrow;
    let mut top = y + frame + arrow;
    let mut last = scroll;
    for (index, entry) in entries.iter().enumerate().skip(scroll) {
        let h = entry_height(entry, m);
        if top + h > bottom {
            break;
        }
        let rect = Rect { x: x + frame, y: top, w: inner_w, h };
        if *entry == Separator {
            dropdown.separators.push(rect);
        } else {
            dropdown.rows.push((index, rect));
        }
        last = index;
        top += h;
    }
    if arrow > 0 {
        let strip = |y| TabArea { x: x + frame, y, width: inner_w, height: arrow };
        dropdown.up = (scroll > 0).then(|| strip(area.y + frame));
        dropdown.down = (last + 1 < entries.len()).then(|| strip(bottom));
    }
    dropdown
}

pub(super) fn title_areas(area: TabArea, m: &Metrics, font: Option<&mut font::UiFont>) -> Vec<TabArea> {
    let pad = m.dip(9.0);
    let mut x = area.x + m.dip(4.0);
    let mut font = font;
    MENUS
        .iter()
        .map(|menu| {
            let label = text(menu.title);
            let width = font.as_deref_mut().map_or(label.len() * m.dip(8.0), |font| font.width(&label)) + 2 * pad;
            let title = TabArea { x, y: area.y, width, height: area.height };
            x += width;
            title
        })
        .collect()
}

pub(super) fn dropdown(
    menu: usize,
    title: TabArea,
    window: (usize, usize),
    m: &Metrics,
    font: &mut font::UiFont,
    labels: &HashMap<&'static str, String>,
    scroll: usize,
) -> Dropdown {
    let (mut label_w, mut key_w) = (0, 0);
    for entry in MENUS[menu].entries {
        if let Item(label, command) = entry {
            label_w = label_w.max(font.width(&text(label)));
            key_w = key_w.max(labels.get(command).map_or(0, |key| font.width(key)));
        }
    }
    let keys = if key_w > 0 { m.gap + key_w } else { 0 };
    let content = m.pad + label_w + keys + m.pad;
    let width = (content + 2 * m.radius).max(m.menu_min).min(window.0);
    let x = title.x.min(window.0.saturating_sub(width));
    layout_rows(menu, (x, title.y + title.height), width, window.1, m, scroll)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn draw_bar(
    bar: &MenuBar,
    area: TabArea,
    pixels: &mut [u32],
    stride: usize,
    font: Option<&mut font::UiFont>,
    theme: &kwil_core_commands::theme::Theme,
    m: &Metrics,
    hover: Hover,
) {
    use kwil_core_commands::theme::UiRole;
    let bg = theme_ui::rgb(theme, UiRole::TabBackground);
    let fg = theme_ui::rgb(theme, UiRole::TabForeground);
    let rect = tabs::rect(area);
    chrome::fill_clipped(pixels, stride, rect, bg);
    let Some(font) = font else { return };
    let baseline = font.baseline(area.y, area.height);
    let inset = m.dip(4.0).min(area.height / 4);
    for (index, (menu, title)) in MENUS.iter().zip(&bar.titles).enumerate() {
        let selected = bar.state.menu() == Some(index);
        if selected || hover == Hover::Title(index) {
            let amount = if matches!(bar.state, State::Open { menu: open, .. } if open == index) { 0x40 } else { 0x26 };
            chrome::round_rect(
                pixels,
                stride,
                Rect { x: title.x, y: title.y + inset, w: title.width, h: title.height.saturating_sub(2 * inset) },
                m.radius,
                chrome::mix(bg, fg, amount),
            );
        }
        let label = text(menu.title);
        let x = title.x + title.width.saturating_sub(font.width(&label)) / 2;
        font.draw(pixels, stride, rect, x, baseline, &label, fg);
        if bar.cues {
            underline(pixels, stride, font, rect, x, baseline, menu.title, fg, m);
        }
    }
    for tool in &bar.toolbar {
        let r = tool.rect;
        if tool.selected || (bar.toolbar_hover == Some(tool.command) && tool.enabled) {
            chrome::round_rect(
                pixels,
                stride,
                Rect { x: r.x, y: r.y + inset, w: r.w, h: r.h.saturating_sub(2 * inset) },
                m.radius,
                chrome::mix(
                    bg,
                    if tool.selected { theme_ui::rgb(theme, UiRole::Accent) } else { fg },
                    if tool.selected { 0x40 } else { 0x26 },
                ),
            );
        }
        icons::draw(
            pixels,
            stride,
            tool.icon,
            ((r.x + if tool.label.is_some() { m.dip(14.0) } else { r.w / 2 }) as f32, (r.y + r.h / 2) as f32),
            m.dip(18.0),
            if tool.enabled { fg } else { chrome::mix(bg, fg, 0x55) },
        );
        if let Some(label) = tool.label {
            font.draw(pixels, stride, r, r.x + m.dip(28.0), baseline, label, fg);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn underline(
    pixels: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    clip: Rect,
    x: usize,
    baseline: usize,
    label: &str,
    color: u32,
    m: &Metrics,
) {
    if let Some((_, index)) = mnemonic(label) {
        let (offset, width) = font.offset_of(&text(label), index);
        let y = baseline + m.dip(2.0).min(font.descent.max(1));
        let line = Rect { x: x + offset, y, w: width, h: m.line };
        if line.y + line.h <= clip.y + clip.h {
            chrome::fill_clipped(pixels, stride, line, color);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn draw_dropdown(
    bar: &MenuBar,
    dropdown: &Dropdown,
    available: &Available,
    pixels: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    theme: &kwil_core_commands::theme::Theme,
    m: &Metrics,
) {
    use kwil_core_commands::theme::UiRole;
    let State::Open { menu, item, keyboard } = bar.state else { return };
    let Some(entries) = MENUS.get(menu).filter(|_| dropdown.menu == menu).map(|menu| menu.entries) else {
        return;
    };
    let bg = theme_ui::rgb(theme, UiRole::PanelBackground);
    let fg = theme_ui::rgb(theme, UiRole::PanelForeground);
    let border = theme_ui::rgb(theme, UiRole::Border);
    let highlight = theme_ui::rgb(theme, UiRole::Selection);
    let area = dropdown.area;
    chrome::fill_clipped(pixels, stride, area, border);
    let inner = Rect {
        x: area.x + m.line,
        y: area.y + m.line,
        w: area.w.saturating_sub(2 * m.line),
        h: area.h.saturating_sub(2 * m.line),
    };
    chrome::fill_clipped(pixels, stride, inner, bg);
    for separator in &dropdown.separators {
        let line = Rect {
            x: separator.x + m.pad / 2,
            y: separator.y + separator.h / 2,
            w: separator.w.saturating_sub(m.pad),
            h: m.line,
        };
        chrome::fill_clipped(pixels, stride, line, chrome::mix(bg, border, 0xc0));
    }
    // Chevrons mark entries hidden above or below a list taller than the window.
    let stroke = (1.25 * m.scale as f32).max(1.0);
    let muted = chrome::mix(bg, fg, 0x90);
    for (strip, up) in [(dropdown.up, true), (dropdown.down, false)] {
        if let Some(strip) = strip {
            let (cx, cy) = (strip.x as f32 + strip.width as f32 / 2.0, strip.y as f32 + strip.height as f32 / 2.0);
            let (h, q) = (m.glyph as f32 / 2.0, m.glyph as f32 / 4.0);
            let (tip, tail) = if up { (cy - q, cy + q) } else { (cy + q, cy - q) };
            chrome::line(pixels, stride, (cx - h, tail), (cx, tip), stroke, muted);
            chrome::line(pixels, stride, (cx, tip), (cx + h, tail), stroke, muted);
        }
    }
    for &(index, row) in &dropdown.rows {
        let Some(&Item(label, command)) = entries.get(index) else { continue };
        let usable = enabled(available, command);
        if item == Some(index) && usable {
            chrome::round_rect(pixels, stride, row, m.radius, highlight);
        }
        let color = if usable { fg } else { chrome::mix(bg, fg, 0x70) };
        let baseline = font.baseline(row.y, row.h);
        let name = text(label);
        let x = row.x + m.pad;
        font.draw(pixels, stride, inner, x, baseline, &name, color);
        if keyboard || bar.cues {
            underline(pixels, stride, font, inner, x, baseline, label, color, m);
        }
        if let Some(keys) = bar.labels.get(command) {
            let width = font.width(keys);
            let key_color = if usable { chrome::mix(bg, fg, 0xa8) } else { chrome::mix(bg, fg, 0x60) };
            font.draw(pixels, stride, inner, (row.x + row.w).saturating_sub(m.pad + width), baseline, keys, key_color);
        }
    }
}

impl App {
    pub(super) fn ime_preedit(&mut self, text: &str) {
        if self.menu.state.is_open() || self.language_owns_input() {
            // Reject composition without relinquishing the menu's keyboard and
            // mouse ownership. A later Commit remains cancelled after dismissal.
            self.ime_destination = Some(ime::Destination::Cancelled);
            self.composing = false;
        } else {
            if ime::starts_composition(self.composing, text) {
                self.ime_destination = Some(self.ime_capture());
            }
            // Empty preedit can precede Commit; retain its destination.
            self.composing = !text.is_empty();
        }
    }

    pub(super) fn menu_available(&self) -> Available {
        let editor = self.view.editor();
        let tabs = self.view.session().tabs();
        Available {
            editor: editor.is_some(),
            markdown: self.is_markdown_document(),
            log: self.is_log_document(),
            selection: editor.is_some_and(|editor| editor.cursors().iter().any(|cursor| cursor.anchor != cursor.head)),
            palette: self.palette.is_some(),
            settings: self.settings_panel.is_some(),
            recent: !self.view.session().recent().is_empty(),
            closed: !self.view.session().closed().is_empty(),
            dirty: tabs.iter().any(|tab| {
                self.view
                    .view(tab.id)
                    .and_then(View::editor)
                    .map_or(tab.revision != tab.saved_revision, |editor| editor.document().is_dirty())
            }),
            search: self.search.is_some() && self.view.editor().is_some(),
            zoom_in: self.zoom.would_change("view.zoomIn"),
            zoom_out: self.zoom.would_change("view.zoomOut"),
            zoom_reset: self.zoom.would_change("view.zoomReset"),
        }
    }

    /// Menus never compete with a modal card, the save prompt or an IME.
    fn menu_allowed(&self) -> bool {
        !self.language_owns_input()
            && !self.settings_open()
            && !self.palette_open()
            && !self.search_panel.is_open()
            && self.save_path.is_none()
            && !self.composing
    }

    pub(super) fn menu_set(&mut self, state: State, cues: bool) {
        if state == self.menu.state && cues == self.menu.cues {
            return;
        }
        if state.is_open() && !self.menu.state.is_open() {
            let mode = self.keys.as_ref().map_or(Mode::Insert, |keys| keys.mode());
            self.menu.refresh_labels(self.settings.keymap, &self.settings.bindings, mode);
            // The menu owns keyboard input now. A composition captured for the
            // document is cancelled, never redirected (same rule as the palette).
            if self.ime_destination.is_some() || self.composing {
                self.ime_destination = Some(ime::Destination::Cancelled);
                self.composing = false;
            }
        }
        if state.is_open() {
            self.close_context();
            self.cancel_logs_composition();
            self.logs_ime = None;
            self.drag = None;
            self.selecting = false;
            if let Some(keys) = &mut self.keys {
                keys.cancel_pending();
            }
        }
        if !matches!((state, self.menu.state), (State::Open { menu: a, .. }, State::Open { menu: b, .. }) if a == b) {
            self.menu.scroll = 0;
        }
        self.menu.state = state;
        self.menu.cues = cues && state.is_open();
        self.menu_dirty = true;
        self.chrome_dirty = true;
        self.request_frame();
    }

    /// IME destination for a new composition or an uncaptured commit.
    pub(super) fn ime_capture(&self) -> ime::Destination {
        ime_target(self.menu.state.is_open(), || self.capture_input_destination())
    }

    /// Accepted settings changed: an open list shows the new effective keymap
    /// at once (Alt ownership reads the same keymap on its next key).
    pub(super) fn menu_settings_changed(&mut self) {
        let mode = self.keys.as_ref().map_or(Mode::Insert, |keys| keys.mode());
        if self.menu.settings_changed(self.settings.keymap, &self.settings.bindings, mode) {
            self.menu_dirty = true;
            self.chrome_dirty = true;
            self.request_frame();
        }
    }

    pub(super) fn request_frame(&mut self) {
        if let Some(dialog) = &self.desktop_window {
            dialog.gfx.window.request_redraw();
            return;
        }
        if let Some(gfx) = &self.gfx {
            self.redraw_requested = true;
            gfx.window.request_redraw();
        }
    }

    /// Alt pressed and released alone toggles keyboard focus on the bar.
    pub(super) fn menu_alt(&mut self, event: &KeyEvent) -> bool {
        let alt = matches!(event.logical_key, Key::Named(NamedKey::Alt));
        if event.state == ElementState::Pressed {
            if alt && !event.repeat {
                self.menu.alt_tap = !self.mods.control_key() && !self.mods.shift_key() && !self.mods.super_key();
            } else if !alt {
                self.menu.alt_tap = false;
            }
            return false;
        }
        if !alt || !std::mem::take(&mut self.menu.alt_tap) {
            return false;
        }
        if self.menu.state.is_open() {
            self.menu_set(State::Closed, false);
        } else if self.menu_allowed() {
            self.menu_set(State::Bar(0), true);
        } else {
            return false;
        }
        true
    }

    pub(super) fn menu_key(&mut self, el: Option<&ActiveEventLoop>, event: &HostKey<'_>) -> bool {
        let available = self.menu_available();
        let usable = |command: &str| enabled(&available, command);
        let open = self.menu.state.is_open();
        if !open && !self.menu_allowed() {
            return false;
        }
        // Alt+letter follows one rule whether or not a menu is open: the keymap
        // (binding or explicit unbind) first, then title mnemonics. It never
        // activates an item of the open list.
        let only_alt = self.mods.alt_key() && !self.mods.control_key() && !self.mods.super_key();
        if only_alt
            && let Key::Character(c) = &event.logical_key
            && let Some(c) = c.chars().next()
        {
            let owned = input::chord(&event.logical_key, self.mods).is_some_and(|chord| {
                let mode = self.keys.as_ref().map_or(Mode::Insert, |keys| keys.mode());
                self.menu.keys.owns(self.settings.keymap, &self.settings.bindings, mode, chord)
            });
            return match alt_letter(owned, c) {
                AltLetter::Keymap => {
                    self.menu_set(State::Closed, false);
                    false
                }
                AltLetter::Open(menu) => {
                    self.menu_set(State::open(menu, &usable), true);
                    self.menu.reveal = true;
                    true
                }
                AltLetter::Nothing => open,
            };
        }
        if !open {
            if matches!(event.logical_key, Key::Named(NamedKey::F10))
                && self.mods.is_empty()
                && let Some(chord) = input::chord(&event.logical_key, self.mods)
            {
                let mode = self.keys.as_ref().map_or(Mode::Insert, |keys| keys.mode());
                if !self.menu.keys.owns(self.settings.keymap, &self.settings.bindings, mode, chord) {
                    self.menu_set(State::Bar(0), true);
                    return true;
                }
            }
            return false;
        }
        if self.mods.control_key() || self.mods.super_key() {
            // A shortcut while the menu has focus runs as usual.
            self.menu_set(State::Closed, false);
            return false;
        }
        let key = match &event.logical_key {
            Key::Named(NamedKey::ArrowLeft) => MenuKey::Left,
            Key::Named(NamedKey::ArrowRight) => MenuKey::Right,
            Key::Named(NamedKey::ArrowUp) => MenuKey::Up,
            Key::Named(NamedKey::ArrowDown) => MenuKey::Down,
            Key::Named(NamedKey::Home) => MenuKey::Home,
            Key::Named(NamedKey::End) => MenuKey::End,
            Key::Named(NamedKey::Enter | NamedKey::Space) => MenuKey::Enter,
            Key::Named(NamedKey::Escape) => MenuKey::Escape,
            Key::Named(NamedKey::Tab) if self.mods.shift_key() => MenuKey::Left,
            Key::Named(NamedKey::Tab) => MenuKey::Right,
            Key::Named(NamedKey::F10) => {
                // F10 leaves the menu from any depth.
                self.menu_set(State::Closed, false);
                return true;
            }
            Key::Character(c) => match c.chars().next() {
                Some(c) => MenuKey::Char(c),
                None => return true,
            },
            _ => return true,
        };
        let (next, command) = self.menu.state.key(key, &usable);
        self.menu_set(next, next.is_open());
        self.menu.reveal = true;
        if let Some(command) = command {
            self.run_menu_command(el, command);
        }
        true
    }

    /// Returns true when the click belonged to the menu (including a dismissing
    /// click outside an open menu, which is consumed like a native menu).
    pub(super) fn menu_mouse(&mut self, el: &ActiveEventLoop) -> bool {
        if !self.menu_allowed() {
            return false;
        }
        let pointer = self.pointer;
        let size = self.pending_resize.unwrap_or(self.size);
        if self.menu.toolbar_surface == Some((size.0, size.1, self.ui.scale.to_bits()))
            && let Some(tool) = self.menu.toolbar.iter().find(|t| t.rect.contains(pointer)).copied()
        {
            if tool.enabled && enabled(&self.menu_available(), tool.command) {
                self.menu_set(State::Closed, false);
                self.run_menu_command(Some(el), tool.command);
            }
            return true;
        }
        if let Some(dropdown) = self.current_dropdown()
            && dropdown.area.contains(pointer)
        {
            if let Some((_, command)) = dropdown.row_at(self.menu.state, pointer)
                && enabled(&self.menu_available(), command)
            {
                self.menu_set(State::Closed, false);
                self.run_menu_command(Some(el), command);
            }
            return true;
        }
        if let Some(index) = self.menu.titles.iter().position(|title| title.contains(pointer.0, pointer.1)) {
            let next = match self.menu.state {
                State::Open { menu, .. } if menu == index => State::Closed,
                _ => State::Open { menu: index, item: None, keyboard: false },
            };
            self.menu_set(next, false);
            return true;
        }
        if self.menu.state.is_open() {
            self.menu_set(State::Closed, false);
            return true;
        }
        false
    }

    /// Pointer tracking while a list is open: hovering another title switches
    /// menus and hovering a row highlights it.
    pub(super) fn menu_track(&mut self) {
        let State::Open { menu, item, keyboard } = self.menu.state else { return };
        let pointer = self.pointer;
        if let Some(index) = self.menu.titles.iter().position(|title| title.contains(pointer.0, pointer.1)) {
            if index != menu {
                self.menu_set(State::Open { menu: index, item: None, keyboard: false }, self.menu.cues);
            }
            return;
        }
        let available = self.menu_available();
        let row = self
            .current_dropdown()
            .and_then(|dropdown| dropdown.row_at(self.menu.state, pointer))
            .filter(|(_, command)| enabled(&available, command))
            .map(|(index, _)| index);
        if row.is_some() && row != item {
            self.menu_set(State::Open { menu, item: row, keyboard }, self.menu.cues);
        }
    }

    /// The drawn list, only while it still belongs to the open menu.
    fn current_dropdown(&self) -> Option<Dropdown> {
        let State::Open { menu, .. } = self.menu.state else { return None };
        self.menu.dropdown.clone().filter(|dropdown| dropdown.menu == menu)
    }

    /// Wheel over an open list scrolls it; any wheel is consumed while a menu
    /// has focus so the document never scrolls underneath.
    pub(super) fn menu_wheel(&mut self, up: bool) -> bool {
        if !self.menu.state.is_open() {
            return false;
        }
        if let Some(dropdown) = self.current_dropdown()
            && dropdown.area.contains(self.pointer)
        {
            let scroll =
                if up { dropdown.scroll.saturating_sub(1) } else { (dropdown.scroll + 1).min(dropdown.max_scroll) };
            if scroll != self.menu.scroll {
                self.menu.scroll = scroll;
                self.menu.reveal = false;
                self.menu_dirty = true;
                self.request_frame();
            }
        }
        true
    }

    pub(super) fn toolbar_track(&mut self) {
        let hovered = self.menu.toolbar.iter().find(|tool| tool.rect.contains(self.pointer)).map(|tool| tool.command);
        if hovered != self.menu.toolbar_hover {
            self.menu.toolbar_hover = hovered;
            self.chrome_dirty = true;
            self.request_frame();
        }
    }

    /// Runs a menu command through the same path as the palette and bindings.
    pub(super) fn run_menu_command(&mut self, el: Option<&ActiveEventLoop>, command: &'static str) {
        if command == ABOUT {
            self.about.start(&self.proxy);
            if let Some(el) = el {
                self.sync_desktop_window(el);
            }
            self.request_frame();
            return;
        }
        if command == "file.quit" {
            if let Some(el) = el {
                self.request_close(el, false);
            }
            return;
        }
        // The palette's command path owns settings/palette dispatch and the Vim
        // model split; zoom and file commands reach their hosts through `edit`.
        self.palette_action(kwil_core_commands::palette::PaletteAction::Command(command));
        if self.may_exit()
            && let Some(el) = el
        {
            el.exit();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(_: &str) -> bool {
        true
    }

    #[test]
    fn every_item_is_a_registered_command_with_unique_mnemonics() {
        let mut titles = std::collections::HashSet::new();
        for menu in MENUS {
            assert!(titles.insert(mnemonic(menu.title).unwrap().0), "duplicate title mnemonic in {}", menu.title);
            let mut keys = std::collections::HashSet::new();
            assert!(
                !matches!(menu.entries.first(), Some(Separator)) && !matches!(menu.entries.last(), Some(Separator))
            );
            for entry in menu.entries {
                if let Item(label, command) = entry {
                    assert!(
                        *command == ABOUT || kwil_core_commands::command(command).is_some(),
                        "{label} runs unknown command {command}"
                    );
                    let (key, _) = mnemonic(label).unwrap_or_else(|| panic!("{label} has no mnemonic"));
                    assert!(keys.insert(key), "duplicate mnemonic {key} in {}", menu.title);
                }
            }
        }
        let names: Vec<_> = MENUS.iter().map(|menu| text(menu.title)).collect();
        assert_eq!(names, ["File", "Edit", "Selection", "View", "Search", "Help"]);
    }

    #[test]
    fn keyboard_navigation_skips_separators_and_disabled_items_and_activates_with_enter() {
        // Edit: Undo, Redo, |, Cut, Copy, Paste ... with no selection.
        let available = Available { editor: true, palette: true, settings: true, ..Default::default() };
        let usable = |command: &str| enabled(&available, command);
        let edit = title_for('e').unwrap();
        let open = State::open(edit, &usable);
        assert_eq!(open, State::Open { menu: edit, item: Some(0), keyboard: true });
        let (redo, _) = open.key(MenuKey::Down, &usable);
        let (paste, _) = redo.key(MenuKey::Down, &usable);
        assert_eq!(paste, State::Open { menu: edit, item: Some(5), keyboard: true }, "Cut and Copy need a selection");
        let (back, _) = paste.key(MenuKey::Up, &usable);
        assert_eq!(back, redo);
        let (wrapped, _) = open.key(MenuKey::Up, &usable);
        let last = MENUS[edit].entries.len() - 1;
        assert_eq!(wrapped, State::Open { menu: edit, item: Some(last), keyboard: true });
        assert_eq!(paste.key(MenuKey::Enter, &usable), (State::Closed, Some("edit.paste")));
        // A disabled item cannot be activated by its mnemonic or by Enter.
        assert_eq!(paste.key(MenuKey::Char('t'), &usable), (paste, None));
        let on_cut = State::Open { menu: edit, item: Some(3), keyboard: true };
        assert_eq!(on_cut.key(MenuKey::Enter, &usable), (on_cut, None));
        let with_selection = Available { selection: true, ..available };
        assert_eq!(
            on_cut.key(MenuKey::Enter, &|command: &str| enabled(&with_selection, command)),
            (State::Closed, Some("edit.cut"))
        );
    }

    #[test]
    fn bar_focus_arrows_mnemonics_and_escape_follow_windows_conventions() {
        let usable = all;
        let (right, _) = State::Bar(0).key(MenuKey::Right, &usable);
        assert_eq!(right, State::Bar(1));
        let (left, _) = State::Bar(0).key(MenuKey::Left, &usable);
        assert_eq!(left, State::Bar(MENUS.len() - 1), "arrows wrap around the bar");
        let (opened, _) = State::Bar(3).key(MenuKey::Down, &usable);
        assert_eq!(opened, State::Open { menu: 3, item: Some(0), keyboard: true });
        // Left/right in an open list moves to the neighbouring open menu.
        let (neighbour, _) = opened.key(MenuKey::Right, &usable);
        assert_eq!(neighbour.menu(), Some(4));
        assert!(matches!(neighbour, State::Open { item: Some(_), .. }));
        // Escape steps back to the bar, then closes.
        let (bar, _) = opened.key(MenuKey::Escape, &usable);
        assert_eq!(bar, State::Bar(3));
        assert_eq!(bar.key(MenuKey::Escape, &usable), (State::Closed, None));
        // A mouse-opened list closes fully on Escape.
        let mouse = State::Open { menu: 0, item: None, keyboard: false };
        assert_eq!(mouse.key(MenuKey::Escape, &usable), (State::Closed, None));
        // Mnemonics: a title from the bar, then an item in the list.
        let (file, _) = State::Bar(4).key(MenuKey::Char('F'), &usable);
        assert_eq!(file.menu(), Some(0));
        assert_eq!(file.key(MenuKey::Char('s'), &usable), (State::Closed, Some("file.save")));
        assert_eq!(file.key(MenuKey::Char('x'), &usable), (State::Closed, Some("file.quit")));
        assert_eq!(State::Closed.key(MenuKey::Enter, &usable), (State::Closed, None));
    }

    #[test]
    fn stale_list_geometry_never_indexes_another_menu() {
        // File is drawn, then Left reaches Help before the next paint.
        let m = Metrics::new(1.0);
        let file = layout_rows(0, (0, 30), 300, 2000, &m, 0);
        let save_index = MENUS[0].entries.iter().position(|entry| matches!(entry, Item(_, "file.save"))).unwrap();
        let (_, save) = *file.rows.iter().find(|(index, _)| *index == save_index).unwrap();
        let pointer = (save.x + 2, save.y + 2);
        let open = |menu| State::Open { menu, item: None, keyboard: false };
        assert_eq!(file.row_at(open(0), pointer), Some((save_index, "file.save")));
        let help = title_for('h').unwrap();
        assert_eq!(file.row_at(open(help), pointer), None, "rows of File must not index Help");
        assert_eq!(file.row_at(State::Bar(0), pointer), None);
        // Even geometry mislabelled as Help is bounds-checked instead of panicking.
        let forged = Dropdown { menu: help, ..file.clone() };
        assert_eq!(forged.row_at(open(help), pointer), None);
    }

    #[test]
    fn alt_letters_switch_menus_or_yield_to_the_keymap_and_never_run_items() {
        let edit = title_for('e').unwrap();
        assert_eq!(alt_letter(false, 'e'), AltLetter::Open(edit));
        assert_eq!(alt_letter(false, 'E'), AltLetter::Open(edit));
        assert_eq!(alt_letter(true, 'e'), AltLetter::Keymap, "a bound or unbound chord stays the keymap's");
        assert_eq!(alt_letter(false, 'q'), AltLetter::Nothing);
        // A plain 'e' in the open File list is the Close All mnemonic, which is
        // why Alt+letter is decided before item mnemonics.
        assert_eq!(State::open(0, &all).key(MenuKey::Char('e'), &all), (State::Closed, Some("file.closeAll")));
        let mut keys = Keys::default();
        let mut owns = |s: &str| keys.owns(Preset::VsCode, &[], Mode::Insert, s.parse().unwrap());
        assert_eq!(alt_letter(owns("alt+e"), 'e'), AltLetter::Open(edit));
        assert_eq!(alt_letter(owns("alt+z"), 'z'), AltLetter::Keymap, "VS Code Alt+Z stays Word Wrap");
    }

    #[test]
    fn a_list_taller_than_the_window_scrolls_to_every_item() {
        let m = Metrics::new(1.0);
        let (height, top) = (300, m.menu);
        let entries = MENUS[0].entries;
        let last = entries.len() - 1;
        let first = layout_rows(0, (0, top), 300, height, &m, 0);
        assert!(first.max_scroll > 0 && first.down.is_some() && first.up.is_none());
        assert!(!first.rows.iter().any(|(index, _)| *index == last), "Exit starts below the window");
        let mut scroll = 0;
        for index in (0..entries.len()).filter(|index| entries[*index] != Separator) {
            scroll = reveal(0, index, scroll, top, height, &m);
            let list = layout_rows(0, (0, top), 300, height, &m, scroll);
            let (_, row) = list.rows.iter().find(|(i, _)| *i == index).expect("keyboard selection is visible");
            assert!(row.y >= top && row.y + row.h <= height, "selected row lies inside the window");
        }
        // Wheel steps clamp at the end, where Exit is reachable.
        let end = layout_rows(0, (0, top), 300, height, &m, usize::MAX);
        assert_eq!(end.scroll, end.max_scroll);
        assert!(end.rows.iter().any(|(index, _)| *index == last) && end.down.is_none() && end.up.is_some());
        let roomy = layout_rows(0, (0, top), 300, 2000, &m, 5);
        assert_eq!((roomy.scroll, roomy.max_scroll, roomy.up, roomy.down), (0, 0, None, None));
    }

    #[test]
    fn tiny_windows_never_underflow_list_geometry() {
        let m = Metrics::new(1.0);
        for (width, height) in [(0, 0), (3, 3), (7, 300), (8, 300), (9, 300), (300, 31), (300, 45)] {
            for (menu, spec) in MENUS.iter().enumerate() {
                let y = m.menu.min(height);
                let list = layout_rows(menu, (0, y), width, height, &m, 3);
                for (_, row) in &list.rows {
                    assert!(row.x + row.w <= width && row.y + row.h <= height, "{width}x{height} {row:?}");
                }
                let _ = list.row_at(State::Open { menu, item: None, keyboard: false }, (1, y + 6));
                let _ = reveal(menu, spec.entries.len() - 1, 0, y, height, &m);
            }
        }
    }

    #[test]
    fn ime_text_behind_an_open_menu_never_reaches_the_document() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let length = |workspace: &Workspace| workspace.editor().unwrap().document().snapshot().len();
        // Open menu: no document capture is created and the commit is refused.
        let mut captured = false;
        let destination = ime_target(true, || {
            captured = true;
            ime::capture(&workspace, true)
        });
        assert!(!captured && matches!(destination, ime::Destination::Cancelled));
        ime::commit(&mut workspace, destination, "æ—¥æœ¬".into(), std::time::Duration::ZERO);
        assert_eq!(length(&workspace), 0, "IME text never lands behind the menu");
        // Closed menu: the same shared routing captures the editor as before.
        let destination = ime_target(false, || ime::capture(&workspace, true));
        ime::commit(&mut workspace, destination, "æ—¥æœ¬".into(), std::time::Duration::ZERO);
        assert_eq!(length(&workspace), "æ—¥æœ¬".len() as u64);
    }

    #[test]
    fn an_open_menu_relabels_on_settings_reload_and_a_closed_one_waits() {
        let tombstone = |keys: &str| Binding { keys: keys.into(), command: String::new(), context: None, mode: None };
        let bind = |keys: &str, command: &str| Binding {
            keys: keys.into(),
            command: command.into(),
            context: None,
            mode: None,
        };
        let mut bar = MenuBar { state: State::open(0, &all), ..Default::default() };
        bar.refresh_labels(Preset::VsCode, &[], Mode::Insert);
        assert_eq!(bar.labels.get("file.save").map(String::as_str), Some("Ctrl+S"));
        let reloaded = [tombstone("ctrl+s"), bind("ctrl+alt+w", "file.save"), bind("alt+f", "file.new")];
        assert!(bar.settings_changed(Preset::VsCode, &reloaded, Mode::Insert), "the open list repaints");
        assert_eq!(bar.labels.get("file.save").map(String::as_str), Some("Ctrl+Alt+W"));
        let chord = "alt+f".parse::<KeyChord>().unwrap();
        assert!(bar.keys.owns(Preset::VsCode, &reloaded, Mode::Insert, chord), "Alt+F now belongs to the keymap");
        let mut closed = MenuBar::default();
        assert!(!closed.settings_changed(Preset::VsCode, &reloaded, Mode::Insert));
        assert!(closed.labels.is_empty(), "closed menus relabel when they open");
    }

    #[test]
    fn no_item_runs_a_command_this_build_does_not_host() {
        // Theme switching is not hosted; Find follows the fixed worker gate.
        for entry in MENUS.iter().flat_map(|menu| menu.entries) {
            if let Item(label, command) = entry {
                assert!(!command.starts_with("theme."), "{label}");
                if command.starts_with("search.") {
                    assert!(!enabled(&Available { editor: true, palette: true, ..Default::default() }, command));
                }
            }
        }
    }

    #[test]
    fn availability_reflects_real_gui_capabilities() {
        let none = Available::default();
        assert!(!enabled(&none, "edit.undo"), "editing commands need an editor");
        assert!(!enabled(&none, "view.commandPalette"), "palette commands wait for the palette worker");
        assert!(!enabled(&none, "search.find"), "find is disabled until the GUI hosts the search card");
        assert!(enabled(&none, "navigation.goToLine"), "line mode does not need catalogue readiness");
        assert!(enabled(&Available { search: true, ..Default::default() }, "search.find"));
        assert!(enabled(&none, "file.new") && enabled(&none, "file.quit") && enabled(&none, ABOUT));
        let zoomed = Available { zoom_out: true, zoom_reset: true, ..none };
        assert!(!enabled(&zoomed, "view.zoomIn") && enabled(&zoomed, "view.zoomReset"));
    }

    #[test]
    fn shortcut_labels_follow_overrides_and_tombstones() {
        let mut keys = Keys::default();
        let labels = keys.labels(Preset::VsCode, &[], Mode::Insert);
        assert_eq!(labels.get("file.save").map(String::as_str), Some("Ctrl+S"));
        assert_eq!(labels.get("view.zoomIn").map(String::as_str), Some("Ctrl+="));
        let moved = [
            Binding { keys: "ctrl+s".into(), command: String::new(), context: None, mode: None },
            Binding { keys: "ctrl+alt+w".into(), command: "file.save".into(), context: None, mode: None },
        ];
        let labels = keys.labels(Preset::VsCode, &moved, Mode::Insert);
        assert_eq!(labels.get("file.save").map(String::as_str), Some("Ctrl+Alt+W"));
        // Alt mnemonics yield to bound chords and to explicit unbinds.
        let chord = |s: &str| s.parse::<KeyChord>().unwrap();
        assert!(!keys.owns(Preset::VsCode, &[], Mode::Insert, chord("alt+f")));
        assert!(keys.owns(Preset::VsCode, &[], Mode::Insert, chord("alt+z")), "VS Code binds Alt+Z");
        let unbound = [Binding { keys: "alt+f".into(), command: String::new(), context: None, mode: None }];
        assert!(keys.owns(Preset::VsCode, &unbound, Mode::Insert, chord("alt+f")));
    }
    #[test]
    fn svg_command_buttons_fit_without_overlapping_menu_titles_and_keep_disabled_actions() {
        for scale in [1.0, 1.5, 2.0] {
            let m = Metrics::new(scale);
            let titles = [TabArea { x: 0, y: 0, width: m.dip(360.0), height: m.menu }];
            for width in [7, 320, 800, 1600] {
                let area = TabArea { x: 0, y: 0, width, height: m.menu };
                let tools = toolbar(area, &titles, &m, &Available::default());
                for tool in &tools {
                    assert!(tool.rect.x >= titles[0].width + m.gap);
                    assert!(tool.rect.x + tool.rect.w <= width);
                }
                for pair in tools.windows(2) {
                    assert!(pair[1].rect.x + pair[1].rect.w <= pair[0].rect.x);
                }
                if let Some(search) = tools.iter().find(|t| t.command == "search.find") {
                    assert!(!search.enabled);
                }
            }
        }
    }
}
