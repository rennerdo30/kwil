//! Stable ids are shared by settings, palettes, and both frontends. Presence
//! describes an action; it does not promise that its executor is available yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Command {
    pub id: &'static str,
    pub title: &'static str,
    pub category: &'static str,
}

macro_rules! commands {
    ($($category:literal => {$($id:literal => $title:literal),* $(,)?}),* $(,)?) => {
        pub const COMMANDS: &[Command] = &[$($(Command {
            id: $id, title: $title, category: $category,
        }),*),*];
    };
}

commands! {
    "File" => {
        "file.new" => "New File", "file.open" => "Open File",
        "file.openFolder" => "Open Folder", "file.save" => "Save",
        "file.saveAs" => "Save As", "file.saveAll" => "Save All",
        "file.close" => "Close File", "file.closeAll" => "Close All Files",
        "file.reopenClosed" => "Reopen Closed Tab", "file.recent" => "Open Recent File",
        "file.reload" => "Reload from Disk", "file.keepChanges" => "Keep Local Changes",
        "file.restoreSession" => "Restore Session", "file.setEncoding" => "Change Encoding",
        "file.setLineEnding" => "Change Line Endings", "file.openHex" => "Open as Hex",
        "file.setType" => "Change File Type", "file.quit" => "Quit",
        "file.saveAndQuit" => "Save and Quit", "file.nextTab" => "Next Tab",
        "file.previousTab" => "Previous Tab"
    },
    "Edit" => {
        "edit.undo" => "Undo", "edit.redo" => "Redo", "edit.cut" => "Cut",
        "edit.copy" => "Copy", "edit.paste" => "Paste", "edit.selectAll" => "Select All",
        "edit.selectNextMatch" => "Select Next Match", "edit.addCursor" => "Add Cursor",
        "edit.addCursorAbove" => "Add Cursor Above", "edit.addCursorBelow" => "Add Cursor Below",
        "edit.columnSelection" => "Column Selection", "edit.moveLineUp" => "Move Line Up",
        "edit.moveLineDown" => "Move Line Down", "edit.duplicateLine" => "Duplicate Line",
        "edit.deleteLine" => "Delete Line", "edit.cutLine" => "Cut Line", "edit.indent" => "Indent",
        "edit.outdent" => "Outdent", "edit.toggleComment" => "Toggle Comment",
        "edit.comment" => "Comment Selection", "edit.uncomment" => "Uncomment Selection",
        "edit.trimTrailingWhitespace" => "Trim Trailing Whitespace (May Change Literals)",
        "edit.formatDocument" => "Format Document", "edit.formatSelection" => "Format Selection",
        "edit.deleteCharacter" => "Delete Character", "edit.pasteAfter" => "Paste After",
        "edit.pasteBefore" => "Paste Before", "edit.repeat" => "Repeat Last Change",
        "edit.insert" => "Insert Text", "edit.append" => "Append Text",
        "edit.openBelow" => "Open Line Below", "edit.openAbove" => "Open Line Above",
        "edit.insertLineStart" => "Insert at First Nonblank", "edit.appendLineEnd" => "Append at Line End"
        , "edit.newline" => "Insert Newline", "edit.backspace" => "Delete Previous Character",
        "edit.delete" => "Delete Next Character", "edit.deleteWordBackward" => "Delete Previous Word",
        "edit.deleteWordForward" => "Delete Next Word", "edit.selectWord" => "Select Word", "edit.selectLine" => "Select Line"
    },
    "Navigation" => {
        "navigation.goToLine" => "Go to Line", "navigation.matchBracket" => "Go to Matching Bracket",
        "motion.left" => "Move Left", "motion.down" => "Move Down",
        "motion.up" => "Move Up", "motion.right" => "Move Right",
        "motion.wordForward" => "Next Word", "motion.wordBackward" => "Previous Word",
        "motion.wordEnd" => "End of Word", "motion.lineStart" => "Start of Line",
        "motion.lineEnd" => "End of Line", "motion.firstLine" => "First Line",
        "motion.lastLine" => "Last Line", "motion.line" => "Whole Line",
        "motion.selection" => "Current Selection", "motion.innerWord" => "Inner Word",
        "motion.aroundWord" => "Around Word", "motion.innerQuote" => "Inside Quotes",
        "motion.aroundQuote" => "Around Quotes", "motion.innerParentheses" => "Inside Parentheses",
        "motion.aroundParentheses" => "Around Parentheses"
        , "motion.smartHome" => "First Nonblank or Line Start", "motion.pageUp" => "Page Up", "motion.pageDown" => "Page Down"
    },
    "Vim" => {
        "operator.delete" => "Delete Range", "operator.change" => "Change Range",
        "operator.yank" => "Yank Range", "mode.normal" => "Normal Mode",
        "mode.insert" => "Insert Mode", "mode.visual" => "Visual Mode",
        "mode.visualLine" => "Visual Line Mode", "mode.command" => "Command Mode"
    },
    "View" => {
        "view.commandPalette" => "Command Palette", "view.fileFinder" => "Find File",
        "view.toggleHex" => "Toggle Hex View", "view.toggleWordWrap" => "Toggle Word Wrap",
        "view.retryWrap" => "Retry Wrapped View",
        "view.toggleWhitespace" => "Toggle Whitespace", "view.zoomIn" => "Zoom In",
        "view.zoomOut" => "Zoom Out", "view.zoomReset" => "Reset Zoom",
        "view.logFilters" => "Filter Log Levels and Time",
        "view.markdownText" => "Markdown: Text", "view.markdownPreview" => "Markdown: Preview", "view.markdownSplit" => "Markdown: Split",
        "view.toggleMarkdown" => "Toggle Markdown Preview", "view.settings" => "Open Settings",
        "view.settingsFile" => "Open Settings File"
    },
    "Search" => {
        "search.find" => "Find", "search.findNext" => "Find Next",
        "search.findPrevious" => "Find Previous", "search.replace" => "Replace",
        "search.replaceNext" => "Replace Next", "search.replaceAll" => "Replace All",
        "search.findInFiles" => "Find in Files", "search.replaceInFiles" => "Replace in Files",
        "search.toggleRegex" => "Toggle Regular Expression", "search.toggleCase" => "Toggle Match Case",
        "search.toggleWholeWord" => "Toggle Whole Word", "search.toggleSelection" => "Search in Selection",
        "search.setInclude" => "Set Include Glob", "search.setExclude" => "Set Exclude Glob",
        "search.substitute" => "Substitute"
    },
    "Compare" => {
        "compare.files" => "Compare Files", "compare.saved" => "Compare with Saved File",
        "compare.nextChange" => "Next Change", "compare.previousChange" => "Previous Change",
        "compare.toggleIgnoreWhitespace" => "Toggle Ignore Whitespace",
        "compare.toggleUnchanged" => "Toggle Unchanged Regions", "compare.close" => "Close Comparison"
    },
    "Log" => {
        "log.toggleFollow" => "Toggle Follow Mode", "log.pause" => "Pause Follow",
        "log.resume" => "Resume Follow", "log.filter" => "Filter Log",
        "log.clearFilter" => "Clear Log Filter", "log.toggleRegex" => "Toggle Log Filter Regex",
        "log.toggleLevel" => "Toggle Log Level", "log.highlightRules" => "Edit Log Highlight Rules",
        "log.toggleMark" => "Toggle Line Mark", "log.nextMark" => "Next Line Mark",
        "log.previousMark" => "Previous Line Mark", "log.goToTime" => "Go to Time Range"
    },
    "Theme" => {
        "theme.select" => "Select Theme", "theme.preview" => "Preview Theme",
        "theme.import" => "Import Theme", "theme.followSystem" => "Follow System Theme"
    },
    "Palette" => {
        "palette.accept" => "Accept Selection", "palette.cancel" => "Close Palette",
        "palette.next" => "Next Result", "palette.previous" => "Previous Result"
    },
    "Update" => {
        "update.check" => "Check for Updates", "update.apply" => "Apply Update"
    }
}

/// Commands whose feature no frontend runs yet. They stay bindable and in the
/// keymap editor, but the palette hides them. Remove an id here in the change
/// that lands its feature; the frontend palette contract tests then cover it.
pub const NOT_HOSTED: &[&str] = &[
    "file.openFolder",
    "file.reload",
    "file.keepChanges",
    "file.setEncoding",
    "file.setLineEnding",
    "edit.formatDocument",
    "edit.formatSelection",
    "log.toggleFollow",
    "log.pause",
    "log.resume",
    "log.filter",
    "log.clearFilter",
    "log.toggleRegex",
    "log.toggleLevel",
    "log.highlightRules",
    "log.toggleMark",
    "log.nextMark",
    "log.previousMark",
    "log.goToTime",
];

/// Whether the command palette offers this command. Key-only commands
/// (motions, operators, modes, palette navigation) and `NOT_HOSTED` are hidden,
/// so every palette entry runs.
pub fn in_palette(id: &str) -> bool {
    !["motion.", "operator.", "mode.", "palette."].iter().any(|prefix| id.starts_with(prefix))
        && !NOT_HOSTED.contains(&id)
}

pub fn command(id: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|command| command.id == id)
}

#[cfg(test)]
mod compare_command_tests {
    #[test]
    fn comparison_commands_are_available() {
        for command in super::COMMANDS.iter().filter(|command| command.id.starts_with("compare.")) {
            assert!(super::in_palette(command.id), "{} must be available", command.id);
        }
    }
}
