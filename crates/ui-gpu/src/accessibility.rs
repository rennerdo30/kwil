//! Lazy AccessKit trees. Only state-changing events request a snapshot; only
//! changed nodes are submitted. Text comes exclusively from painted viewport
//! caches, never from a document read or a selection-sized allocation.
use super::*;
use accesskit::{Action, ActionData, Node, NodeId, Role, TextPosition, TextSelection, TreeInfo, TreeUpdate};
use std::collections::BTreeMap;

const ROOT: NodeId = NodeId(1);
const MENU: NodeId = NodeId(2);
const TABS: NodeId = NodeId(3);
const EDITOR: NodeId = NodeId(4);
const STATUS: NodeId = NodeId(5);
const OVERLAY: NodeId = NodeId(6);
const QUERY: NodeId = NodeId(7);
const ROW: u64 = 1 << 32;
const TAB: u64 = 2 << 32;
const ITEM: u64 = 3 << 32;

struct Handler {
    window: WindowId,
    proxy: HostProxy,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search_ui::host_dispatch_tests::host;

    fn start(text: &str) -> App {
        let (mut app, _) = host(text);
        let damage = app.view.editor_mut().unwrap().set_cursors(vec![kwil_view_model::Cursor::caret(0)], 0).unwrap();
        app.view.refresh_after_cursor(damage);
        app
    }

    #[test]
    fn viewport_excludes_document_and_exposes_unicode_selection() {
        let text = format!("日本語 first\nsecond\nthird\n{}", "not visible\n".repeat(100));
        let app = start(&text);
        let snapshot = app.accessibility_snapshot(false);
        let exposed: String = snapshot.text.keys().map(|id| snapshot.nodes[id].value().unwrap()).collect();
        assert!(exposed.starts_with("日本語 first\nsecond\nthird\n"));
        assert!(!exposed.contains("not visible"));
        assert!(snapshot.text.len() <= app.view.rows());
        let caret = snapshot.nodes[&EDITOR].text_selection().unwrap();
        assert_eq!(snapshot.source(caret.focus), Some(0));
        assert_eq!(snapshot.source(snapshot.position(6).unwrap()), Some(6));
        assert_eq!(snapshot.position(6).unwrap().character_index, 2, "Unicode scalar, not UTF-8 byte index");
        document_ui::fixtures::settle_app(app);
    }

    #[test]
    fn complete_visible_document_has_no_synthetic_final_newline() {
        for text in ["Accessible first line\n日本語 second line", "日本語\n", ""] {
            let app = start(text);
            let snapshot = app.accessibility_snapshot(false);
            let exposed: String = snapshot.text.keys().map(|id| snapshot.nodes[id].value().unwrap()).collect();
            assert_eq!(exposed, text, "TextPattern must preserve the document's final newline state");
            document_ui::fixtures::settle_app(app);
        }
    }

    #[test]
    fn caret_change_updates_editor_without_resending_text_and_idle_is_empty() {
        let mut app = start("日本語 first\nsecond");
        let before = app.accessibility_snapshot(false);
        assert!(before.delta(Some(&before)).is_none());
        let damage = app
            .view
            .editor_mut()
            .unwrap()
            .set_cursors(vec![kwil_view_model::Cursor { anchor: 3, head: 6, desired_column: None }], 0)
            .unwrap();
        app.view.refresh_after_cursor(damage);
        let after = app.accessibility_snapshot(false);
        let update = after.delta(Some(&before)).unwrap();
        assert!(update.tree.is_none());
        assert!(update.nodes.iter().any(|(id, _)| *id == EDITOR));
        assert!(update.nodes.iter().all(|(id, _)| id.0 < ROW), "unchanged TextRuns must not be resent");
        let selection = after.nodes[&EDITOR].text_selection().unwrap();
        assert_eq!((after.source(selection.anchor), after.source(selection.focus)), (Some(3), Some(6)));
        document_ui::fixtures::settle_app(app);
    }

    #[test]
    fn prompt_focus_and_removal_update_parent_and_dirty_tabs_are_announced() {
        let mut app = start("dirty");
        let before = app.accessibility_snapshot(false);
        assert!(before.nodes[&NodeId(TAB)].label().unwrap().contains("unsaved changes"));
        app.view.file_command("file.setLineEnding").unwrap();
        let prompt = app.accessibility_snapshot(false);
        assert_eq!(prompt.nodes[&OVERLAY].label(), Some("Change Line Endings"));
        assert_eq!(prompt.focus, Some(QUERY));
        assert_eq!(prompt.nodes[&NodeId(ITEM)].label(), Some("LF"));
        app.view.cancel_file_prompt();
        let after = app.accessibility_snapshot(false);
        let update = after.delta(Some(&prompt)).unwrap();
        assert_eq!(update.focus, EDITOR);
        assert!(!after.nodes.contains_key(&OVERLAY));
        assert!(update.nodes.iter().any(|(id, _)| *id == ROOT));
        document_ui::fixtures::settle_app(app);
    }

    #[test]
    fn pending_prepared_page_keeps_the_editor_text_provider_present() {
        let mut app = start("text awaiting layout");
        app.view.set_body_policy(kwil_view_model::ViewBodyPolicy::Prepared);
        let snapshot = app.accessibility_snapshot(false);
        let children = snapshot.nodes[&EDITOR].children();
        assert_eq!(children.len(), 1, "pending layout must not remove TextPattern support");
        assert_eq!(snapshot.nodes[&children[0]].role(), Role::TextRun);
        assert_eq!(snapshot.nodes[&children[0]].value(), Some(""));
        assert!(snapshot.nodes[&EDITOR].text_selection().is_some());
        document_ui::fixtures::settle_app(app);
    }
}
impl Handler {
    fn send(&self, window_event: accesskit_winit::WindowEvent) {
        let _ = self
            .proxy
            .send_event(UserEvent::Accessibility(accesskit_winit::Event { window_id: self.window, window_event }));
    }
}
impl accesskit::ActivationHandler for Handler {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        self.send(accesskit_winit::WindowEvent::InitialTreeRequested);
        None
    }
}
impl accesskit::ActionHandler for Handler {
    fn do_action(&mut self, request: accesskit::ActionRequest) {
        self.send(accesskit_winit::WindowEvent::ActionRequested(request));
    }
}
impl accesskit::DeactivationHandler for Handler {
    fn deactivate_accessibility(&mut self) {
        self.send(accesskit_winit::WindowEvent::AccessibilityDeactivated);
    }
}

pub(super) struct Host {
    adapter: accesskit_winit::Adapter,
    pub active: bool,
    previous: Option<Snapshot>,
    requested_focus: Option<NodeId>,
}
impl Host {
    pub fn new(el: &ActiveEventLoop, window: &Window, proxy: &HostProxy) -> Self {
        let handler = || Handler { window: window.id(), proxy: proxy.clone() };
        Self {
            adapter: accesskit_winit::Adapter::with_direct_handlers(el, window, handler(), handler(), handler()),
            active: false,
            previous: None,
            requested_focus: None,
        }
    }
    pub fn process(&mut self, window: &Window, event: &WindowEvent) {
        if matches!(event, WindowEvent::KeyboardInput { .. } | WindowEvent::MouseInput { .. }) {
            self.requested_focus = None;
        }
        self.adapter.process_event(window, event);
    }
    fn update(&mut self, mut snapshot: Snapshot) {
        if let Some(id) = self.requested_focus.filter(|id| snapshot.nodes.contains_key(id)) {
            snapshot.focus = Some(id);
        } else {
            self.requested_focus = None;
        }
        if let Some(update) = snapshot.delta(self.previous.as_ref()) {
            self.adapter.update_if_active(|| update);
        }
        self.previous = Some(snapshot);
    }
}

#[derive(Clone, PartialEq)]
enum Target {
    Editor,
    Menu(usize),
    Command(&'static str),
    Tab(kwil_view_model::SessionDocumentId),
    Palette(kwil_core_commands::palette::PaletteAction),
    Setting(usize),
    Panel(usize),
    Prompt(String),
    Query,
}

#[derive(Default)]
struct Snapshot {
    nodes: BTreeMap<NodeId, Node>,
    actions: BTreeMap<NodeId, Target>,
    // Character boundaries in each TextRun mapped to absolute source bytes.
    text: BTreeMap<NodeId, Vec<u64>>,
    focus: Option<NodeId>,
    identity: Option<(kwil_view_model::SessionDocumentId, u64, u64)>,
    next_tab: u64,
}
impl Snapshot {
    fn delta(&self, previous: Option<&Self>) -> Option<TreeUpdate> {
        let nodes: Vec<_> = self
            .nodes
            .iter()
            .filter(|(id, node)| previous.and_then(|old| old.nodes.get(id)) != Some(*node))
            .map(|(id, node)| (*id, node.clone()))
            .collect();
        if nodes.is_empty() && previous.is_some_and(|old| old.focus == self.focus) {
            return None;
        }
        Some(TreeUpdate {
            nodes,
            tree: previous.is_none().then(|| TreeInfo::new(ROOT)),
            focus: self.focus.unwrap_or(ROOT),
            tree_id: accesskit::TreeId::ROOT,
        })
    }
    fn insert(&mut self, id: NodeId, role: Role, label: impl Into<String>, target: Option<Target>) -> &mut Node {
        let mut node = Node::new(role);
        node.set_label(label.into());
        if let Some(target) = target {
            node.add_action(Action::Focus);
            if !matches!(target, Target::Editor | Target::Query) {
                node.add_action(Action::Click);
            }
            self.actions.insert(id, target);
        }
        self.nodes.insert(id, node);
        self.nodes.get_mut(&id).unwrap()
    }
    fn children(&mut self, id: NodeId, children: Vec<NodeId>) {
        self.nodes.get_mut(&id).unwrap().set_children(children);
    }
    fn list(&mut self, title: &str, query: &str, rows: Vec<(String, Target)>, selected: Option<usize>) {
        let selected = selected.filter(|index| *index < rows.len());
        self.insert(OVERLAY, Role::ListBox, title, None);
        self.insert(QUERY, Role::TextInput, title, Some(Target::Query)).set_value(query.to_string());
        let mut children = vec![QUERY];
        for (index, (label, target)) in rows.into_iter().enumerate() {
            let id = NodeId(ITEM + index as u64);
            self.insert(id, Role::ListBoxOption, label, Some(target)).set_selected(selected == Some(index));
            if selected == Some(index) {
                self.focus = Some(id);
            }
            children.push(id);
        }
        self.children(OVERLAY, children);
        self.focus = Some(selected.map_or(QUERY, |index| NodeId(ITEM + index as u64)));
    }
    fn position(&self, byte: u64) -> Option<TextPosition> {
        // Clamp an offscreen endpoint to the nearest exposed boundary. This
        // represents only the intersection with the viewport, never reads it.
        let (&node, positions) = self
            .text
            .iter()
            .find(|(_, p)| p.last().is_some_and(|end| byte <= *end))
            .or_else(|| self.text.last_key_value())?;
        let index = positions.partition_point(|at| *at < byte).min(positions.len().saturating_sub(1));
        Some(TextPosition { node, character_index: index })
    }
    fn source(&self, position: TextPosition) -> Option<u64> {
        self.text.get(&position.node)?.get(position.character_index).copied()
    }
    fn line(&mut self, row: usize, text: String, positions: Vec<u64>, bounds: accesskit::Rect) -> NodeId {
        let id = NodeId(ROW + row as u64);
        let mut node = Node::new(Role::TextRun);
        node.set_character_lengths(text.chars().map(|ch| ch.len_utf8() as u8).collect::<Vec<_>>());
        node.set_value(text);
        node.set_bounds(bounds);
        self.nodes.insert(id, node);
        self.text.insert(id, positions);
        id
    }
}

impl App {
    pub(super) fn accessibility_sync(&mut self) {
        if !std::mem::take(&mut self.accessibility_dirty) {
            return;
        }
        if self.accessibility.as_ref().is_some_and(|host| host.active) {
            let snapshot = self.accessibility_snapshot(false);
            self.accessibility.as_mut().unwrap().update(snapshot);
        }
        if self.desktop_window.as_ref().is_some_and(|d| d.accessibility.active) {
            let snapshot = self.accessibility_snapshot(true);
            self.desktop_window.as_mut().unwrap().accessibility.update(snapshot);
        }
    }
    fn accessibility_snapshot(&self, dialog: bool) -> Snapshot {
        let previous = if dialog {
            self.desktop_window.as_ref().and_then(|d| d.accessibility.previous.as_ref())
        } else {
            self.accessibility.as_ref().and_then(|host| host.previous.as_ref())
        };
        let mut tree = Snapshot {
            next_tab: previous.map_or(TAB, |old| old.next_tab),
            identity: Some((
                self.view.active_id(),
                self.view.input_epoch(),
                self.view.editor().map_or(0, |e| e.document().revision()),
            )),
            ..Default::default()
        };
        tree.insert(ROOT, Role::Window, if dialog { "Settings — Kwil" } else { "Kwil" }, None);
        let mut root = Vec::new();
        if !dialog {
            tree.insert(MENU, Role::MenuBar, "Menu", None);
            let mut titles = Vec::new();
            let available = self.menu_available();
            for (index, menu) in menu::MENUS.iter().enumerate() {
                let id = NodeId(100 + index as u64 * 100);
                tree.insert(id, Role::MenuItem, menu::text(menu.title), Some(Target::Menu(index)));
                titles.push(id);
                if self.menu.state.menu() == Some(index) {
                    tree.focus = Some(id);
                    if let menu::State::Open { item, .. } = self.menu.state {
                        let mut items = Vec::new();
                        for (slot, entry) in menu.entries.iter().enumerate() {
                            if let menu::Entry::Item(label, command) = entry {
                                let child = NodeId(id.0 + slot as u64 + 1);
                                let node = tree.insert(
                                    child,
                                    Role::MenuItem,
                                    menu::text(label),
                                    Some(Target::Command(command)),
                                );
                                if !menu::enabled(&available, command) {
                                    node.set_disabled();
                                }
                                if item == Some(slot) {
                                    tree.focus = Some(child);
                                }
                                items.push(child);
                            }
                        }
                        tree.children(id, items);
                    }
                }
            }
            tree.children(MENU, titles);
            tree.insert(TABS, Role::TabList, "Documents", None);
            let mut tabs = Vec::new();
            let old_tabs: BTreeMap<_, _> = previous
                .into_iter()
                .flat_map(|old| old.actions.iter())
                .filter_map(
                    |(node, target)| {
                        if let Target::Tab(document) = target { Some((document.0, *node)) } else { None }
                    },
                )
                .collect();
            for tab in self.view.session().tabs() {
                // Session document IDs may span u64. Allocate accessibility
                // identities independently and never reuse a retired tab ID.
                let id = old_tabs.get(&tab.id.0).copied().unwrap_or_else(|| {
                    let id = NodeId(tree.next_tab);
                    tree.next_tab += 1;
                    id
                });
                let dirty = if self.view.tab_dirty(tab.id) { ", unsaved changes" } else { "" };
                tree.insert(
                    id,
                    Role::Tab,
                    format!("{}{dirty}", self.view.tab_title(tab.id)),
                    Some(Target::Tab(tab.id)),
                )
                .set_selected(tab.id == self.view.active_id());
                tabs.push(id);
            }
            tree.children(TABS, tabs);
            tree.insert(EDITOR, Role::MultilineTextInput, "Editor", Some(Target::Editor));
            let mut lines = Vec::new();
            let body = self.body_area();
            let columns = self.columns() as usize;
            for row in 0..self.text_rows() {
                let mut text = String::new();
                let mut positions = Vec::new();
                if let Some(page) = self.wrap_page() {
                    let Some(line) = page.rows().get(row) else { break };
                    for atom in page.atoms(row) {
                        for (offset, ch) in atom.text().char_indices() {
                            positions.push((atom.source.start + offset as u64).min(atom.source.end));
                            text.push(ch);
                        }
                    }
                    positions.push(line.source_coverage.end);
                    if line.hard_line_end {
                        text.push('\n');
                        positions.push(line.source_coverage.end);
                    }
                } else {
                    let Some(bytes) = self.view.line(row) else { break };
                    let source = self.view.line_source(row).map_or(0, |range| range.start);
                    let mut end = source;
                    for cell in text::cells(bytes, columns, self.settings.editor.tab_size as usize) {
                        end = source + cell.bytes.end as u64;
                        for (offset, ch) in cell.text.char_indices() {
                            positions.push(
                                (source + cell.bytes.start as u64 + offset as u64).min(source + cell.bytes.end as u64),
                            );
                            text.push(ch);
                        }
                    }
                    positions.push(end);
                    text.push('\n');
                    positions.push(end);
                }
                let bounds = accesskit::Rect::new(
                    body.x as f64,
                    (body.y + row * self.ch()) as f64,
                    (body.x + body.width) as f64,
                    (body.y + (row + 1) * self.ch()) as f64,
                );
                lines.push(tree.line(row, text, positions, bounds));
            }
            if lines.is_empty() {
                // Keep TextPattern available while the prepared viewport is
                // pending. Otherwise the native adapter sees an unsupported ->
                // supported transition and cannot announce the new selection.
                let at = self.view.editor().map_or(0, |editor| editor.primary_cursor().head);
                let bounds = accesskit::Rect::new(
                    body.x as f64,
                    body.y as f64,
                    (body.x + body.width) as f64,
                    (body.y + self.ch()) as f64,
                );
                lines.push(tree.line(0, String::new(), vec![at], bounds));
            }
            tree.children(EDITOR, lines);
            if let Some(cursor) = self.view.editor().map(|editor| editor.primary_cursor())
                && let (Some(anchor), Some(focus)) = (tree.position(cursor.anchor), tree.position(cursor.head))
            {
                let node = tree.nodes.get_mut(&EDITOR).unwrap();
                node.set_text_selection(TextSelection { anchor, focus });
                node.add_action(Action::SetTextSelection);
            } else if let Some(position) = tree.position(0) {
                let node = tree.nodes.get_mut(&EDITOR).unwrap();
                node.set_text_selection(TextSelection { anchor: position, focus: position });
                node.set_read_only();
            }
            let (notice, mut segments) = if self.view.hex_active() {
                (self.hex_status(), vec![chrome::Segment { text: "Physical bytes".into(), command: None, warn: false }])
            } else if self.wrap_owns_text() || self.view.wrap_input_blocked() {
                self.wrap_status()
            } else {
                chrome::status(&self.view, self.zoom.percent())
            };
            if !self.view.hex_active()
                && let Some(host) = &self.highlighting
            {
                segments.push(chrome::language_segment(host.language_status(&self.view)));
            }
            let status = std::iter::once(notice)
                .chain(segments.into_iter().map(|s| s.text))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("; ");
            tree.insert(STATUS, Role::Status, status, None).set_live(accesskit::Live::Polite);
            root.extend([MENU, TABS, EDITOR, STATUS]);
        }
        if dialog {
            if let Some(panel) = self.settings_panel.as_ref().filter(|panel| panel.is_open()) {
                if let Ok(Some(first)) = panel.panel_window(0, 1) {
                    let offset = first.selection.saturating_sub(6);
                    if let Ok(Some(window)) = panel.panel_window(offset, 32) {
                        let rows = window
                            .rows
                            .iter()
                            .enumerate()
                            .map(|(index, row)| {
                                (
                                    format!("{}: {}. {}", row.title, row.value, row.description),
                                    Target::Panel(window.offset + index),
                                )
                            })
                            .collect();
                        let query = window
                            .capture
                            .as_deref()
                            .or(window.edit_text.as_ref().map(|edit| {
                                if edit.column == kwil_view_model::settings_panels::EditColumn::Key {
                                    edit.key.as_str()
                                } else {
                                    edit.value.as_str()
                                }
                            }))
                            .unwrap_or(&window.query);
                        tree.list(window.title, query, rows, Some(window.selection.saturating_sub(window.offset)));
                    }
                } else {
                    let rows = panel
                        .rows()
                        .into_iter()
                        .enumerate()
                        .map(|(index, row)| {
                            (
                                format!(
                                    "{}: {}. {}",
                                    row.spec.title,
                                    settings_ui::value_text(&row.current),
                                    row.spec.description
                                ),
                                Target::Setting(index),
                            )
                        })
                        .collect();
                    tree.list(
                        "Settings",
                        panel.input_text(),
                        rows,
                        (!panel.is_searching() && !panel.is_editing()).then_some(panel.selected()),
                    );
                }
            } else {
                tree.list(if self.about.open { "About Kwil" } else { "Welcome to Kwil" }, "", Vec::new(), None);
            }
        } else if let Some(prompt) = self.view.file_prompt() {
            let choices: Vec<String> = if prompt.kind == kwil_view_model::FilePromptKind::Recent {
                self.view.session().recent().iter().take(12).map(|path| path.display().to_string()).collect()
            } else {
                prompt.kind.choices().iter().map(|s| s.to_string()).collect()
            };
            let selected = prompt.text.trim().parse::<usize>().ok().and_then(|n| n.checked_sub(1));
            let rows = choices.into_iter().enumerate().map(|(i, s)| (s, Target::Prompt((i + 1).to_string()))).collect();
            tree.list(prompt.kind.title(), &prompt.text, rows, selected);
        } else if let Some(model) = self
            .goto
            .as_ref()
            .map(|p| &p.model)
            .or_else(|| self.palette.as_ref().filter(|p| p.model().is_open()).map(|p| p.model()))
        {
            let rows = model
                .rows()
                .iter()
                .map(|row| (format!("{} {}", row.title, row.category), Target::Palette(row.action.clone())))
                .collect();
            let title = match model.mode() {
                kwil_core_commands::palette::PaletteMode::Line => "Go to Line",
                kwil_core_commands::palette::PaletteMode::Files => "File finder",
                kwil_core_commands::palette::PaletteMode::Tabs => "Tabs",
                kwil_core_commands::palette::PaletteMode::Commands => "Command palette",
            };
            tree.list(title, model.query(), rows, model.selected());
        }
        if tree.nodes.contains_key(&OVERLAY) {
            root.push(OVERLAY);
        }
        tree.children(ROOT, root);
        tree.focus.get_or_insert(if dialog { ROOT } else { EDITOR });
        tree
    }
    pub(super) fn accessibility_event(&mut self, el: &ActiveEventLoop, event: accesskit_winit::Event) {
        let dialog = self.desktop_window.as_ref().is_some_and(|d| d.gfx.window.id() == event.window_id);
        let host = if dialog {
            self.desktop_window.as_mut().map(|d| &mut d.accessibility)
        } else if self.gfx.as_ref().is_some_and(|g| g.window.id() == event.window_id) {
            self.accessibility.as_mut()
        } else {
            None
        };
        let Some(host) = host else { return };
        match event.window_event {
            accesskit_winit::WindowEvent::InitialTreeRequested => {
                host.active = true;
                host.previous = None;
                host.requested_focus = None;
            }
            accesskit_winit::WindowEvent::AccessibilityDeactivated => {
                host.active = false;
                host.previous = None;
                host.requested_focus = None;
            }
            accesskit_winit::WindowEvent::ActionRequested(request) => {
                let Some(snapshot) = host.previous.as_ref() else { return };
                let Some(target) = snapshot.actions.get(&request.target_node).cloned() else { return };
                // Reject stale requests against a replaced document/list/query.
                let expected_node = snapshot.nodes.get(&request.target_node).cloned();
                let expected_identity = snapshot.identity;
                let expected_text = snapshot.text.clone();
                let current = self.accessibility_snapshot(dialog);
                if !dialog
                    && (self.desktop_window.is_some()
                        || (self.view.file_prompt().is_some() && !matches!(target, Target::Prompt(_) | Target::Query))
                        || ((self.palette_open() || self.goto.is_some())
                            && !matches!(target, Target::Palette(_) | Target::Query)))
                {
                    return;
                }
                if expected_identity != current.identity
                    || expected_node.as_ref() != current.nodes.get(&request.target_node)
                    || Some(&target) != current.actions.get(&request.target_node)
                    || expected_text != current.text
                {
                    return;
                }
                if request.action == Action::SetTextSelection && matches!(target, Target::Editor) {
                    if let Some(ActionData::SetTextSelection(selection)) = request.data
                        && let (Some(anchor), Some(head)) =
                            (current.source(selection.anchor), current.source(selection.focus))
                        && let Some(editor) = self.view.editor_mut()
                    {
                        let damage =
                            editor.set_cursors(vec![kwil_view_model::Cursor { anchor, head, desired_column: None }], 0);
                        if let Ok(damage) = damage {
                            let damage = self.view.refresh_after_cursor(damage);
                            self.apply(damage);
                        }
                    }
                } else if matches!(request.action, Action::Focus | Action::Click) {
                    match target {
                        Target::Menu(index) => {
                            let available = self.menu_available();
                            self.menu_set(menu::State::open(index, &|id| menu::enabled(&available, id)), true);
                        }
                        Target::Command(id) if request.action == Action::Click => {
                            if menu::enabled(&self.menu_available(), id) {
                                self.menu_set(menu::State::Closed, false);
                                self.run_menu_command(Some(el), id);
                            }
                        }
                        Target::Command(id) => {
                            if let Some(menu) = self.menu.state.menu()
                                && let Some(item) = menu::MENUS[menu]
                                    .entries
                                    .iter()
                                    .position(|entry| matches!(entry, menu::Entry::Item(_, command) if *command == id))
                            {
                                self.menu_set(menu::State::Open { menu, item: Some(item), keyboard: true }, true);
                            }
                        }
                        Target::Tab(id) => {
                            self.view.activate(id);
                            self.apply(Damage { full: true, status: true, ..Default::default() });
                        }
                        Target::Setting(index) => {
                            if let Some(panel) = &mut self.settings_panel {
                                panel.select(index);
                            }
                            if request.action == Action::Click {
                                self.settings_input(kwil_view_model::settings_session::SettingsInput::Accept);
                            }
                            self.request_frame();
                        }
                        Target::Panel(index) => {
                            if let Some(panel) = &mut self.settings_panel {
                                let _ = panel.panel_select(index);
                            }
                            if request.action == Action::Click {
                                self.settings_input(kwil_view_model::settings_session::SettingsInput::Accept);
                            }
                            self.request_frame();
                        }
                        Target::Prompt(value) => {
                            self.save_path = Some(value.clone());
                            if let Some(prompt) = self.view.file_prompt_mut() {
                                prompt.text = value;
                            }
                            if request.action == Action::Click {
                                let damage = file_prompt::submit(&mut self.view);
                                self.apply(damage);
                            }
                            self.sync_file_ui();
                        }
                        Target::Palette(action) if request.action == Action::Click => {
                            if let Some(palette) = &mut self.palette {
                                let _ = palette.input(kwil_view_model::palette_session::PaletteInput::Cancel);
                            }
                            self.goto = None;
                            self.palette_action(action);
                        }
                        Target::Palette(action) => {
                            // Model lists have at most MAX_RESULTS entries; keep keyboard
                            // navigation at the row the accessibility client focused.
                            for _ in 0..kwil_core_commands::palette::MAX_RESULTS {
                                let model = self
                                    .goto
                                    .as_ref()
                                    .map(|p| &p.model)
                                    .or_else(|| self.palette.as_ref().map(|p| p.model()));
                                if model.is_none_or(|m| {
                                    m.selected().and_then(|i| m.rows().get(i)).is_some_and(|row| row.action == action)
                                }) {
                                    break;
                                }
                                if let Some(prompt) = &mut self.goto {
                                    prompt.model.next();
                                } else if let Some(palette) = &mut self.palette {
                                    let _ = palette.input(kwil_view_model::palette_session::PaletteInput::Next);
                                }
                            }
                            self.request_frame();
                        }
                        Target::Query => {
                            if dialog
                                && let Some(panel) = &mut self.settings_panel
                                && !panel.is_editing()
                                && panel.panel().is_none()
                            {
                                self.settings_input(kwil_view_model::settings_session::SettingsInput::Search);
                            }
                        }
                        Target::Editor => {
                            self.menu_set(menu::State::Closed, false);
                        }
                    }
                    if request.action == Action::Focus {
                        let host = if dialog {
                            self.desktop_window.as_mut().map(|d| &mut d.accessibility)
                        } else {
                            self.accessibility.as_mut()
                        };
                        if let Some(host) = host {
                            host.requested_focus = Some(request.target_node);
                        }
                    }
                }
            }
        }
        self.accessibility_dirty = true;
    }
}
