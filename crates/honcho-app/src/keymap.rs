//! Default key bindings, one table per platform, following VS Code's
//! defaults on each.
//!
//! Modifiers are written out per platform rather than with gpui's
//! `secondary-`, which resolves against the host. That way the tests can
//! check every platform's table from any machine.

use gpui::{Action, App, DummyKeyboardMapper, KeyBinding, KeyBindingContextPredicate};

use crate::editor_view::*;
use crate::find::*;
use crate::workspace::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Mac,
    Windows,
    Linux,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Platform::Mac
        } else if cfg!(target_os = "windows") {
            Platform::Windows
        } else {
            Platform::Linux
        }
    }

    /// The main shortcut modifier.
    fn primary(self) -> &'static str {
        match self {
            Platform::Mac => "cmd",
            _ => "ctrl",
        }
    }
}

pub struct Binding {
    pub keys: String,
    pub action: fn() -> Box<dyn Action>,
    pub context: Option<&'static str>,
}

const EDITOR: Option<&str> = Some("Editor");
const FIND_BAR: Option<&str> = Some("FindBar");
const FIND_QUERY: Option<&str> = Some("FindQuery");
const REPLACE_QUERY: Option<&str> = Some("ReplaceQuery");

macro_rules! bindings {
    ($p:expr, [$( ($keys:expr, $action:expr, $ctx:expr) ),* $(,)?]) => {
        vec![$(
            Binding {
                keys: $keys.replace("P-", &format!("{}-", $p.primary())),
                action: || Box::new($action),
                context: $ctx,
            }
        ),*]
    };
}

/// All bindings for a platform, in registration order. Order matters:
/// later bindings win at the same context depth, so the find-bar inputs
/// come after the editor they're built from.
pub fn bindings(p: Platform) -> Vec<Binding> {
    let mut b = editor(p);
    b.extend(workspace(p));
    b.extend(find(p));
    b
}

pub fn install(cx: &mut App) {
    let bindings = bindings(Platform::current())
        .into_iter()
        .map(|b| to_key_binding(&b).expect("default keymap is valid"));
    cx.bind_keys(bindings);
}

fn to_key_binding(b: &Binding) -> Result<KeyBinding, String> {
    let predicate = match b.context {
        Some(c) => Some(
            KeyBindingContextPredicate::parse(c)
                .map_err(|e| e.to_string())?
                .into(),
        ),
        None => None,
    };
    KeyBinding::load(
        &b.keys,
        (b.action)(),
        predicate,
        false,
        None,
        &DummyKeyboardMapper,
    )
    .map_err(|e| format!("{}: {e}", b.keys))
}

// "P-" is replaced with the platform's primary modifier.
fn editor(p: Platform) -> Vec<Binding> {
    let mut b = bindings!(
        p,
        [
            ("left", MoveLeft, EDITOR),
            ("right", MoveRight, EDITOR),
            ("up", MoveUp, EDITOR),
            ("down", MoveDown, EDITOR),
            ("shift-left", SelectLeft, EDITOR),
            ("shift-right", SelectRight, EDITOR),
            ("shift-up", SelectUp, EDITOR),
            ("shift-down", SelectDown, EDITOR),
            ("home", MoveLineStart, EDITOR),
            ("end", MoveLineEnd, EDITOR),
            ("shift-home", SelectLineStart, EDITOR),
            ("shift-end", SelectLineEnd, EDITOR),
            ("pageup", PageUp, EDITOR),
            ("pagedown", PageDown, EDITOR),
            ("shift-pageup", SelectPageUp, EDITOR),
            ("shift-pagedown", SelectPageDown, EDITOR),
            ("backspace", Backspace, EDITOR),
            ("shift-backspace", Backspace, EDITOR),
            ("delete", Delete, EDITOR),
            ("enter", Newline, EDITOR),
            ("shift-enter", Newline, EDITOR),
            ("tab", Tab, EDITOR),
            ("shift-tab", Outdent, EDITOR),
            ("P-]", Indent, EDITOR),
            ("P-[", Outdent, EDITOR),
            ("P-enter", InsertLineBelow, EDITOR),
            ("P-shift-enter", InsertLineAbove, EDITOR),
            ("P-z", Undo, EDITOR),
            ("P-shift-z", Redo, EDITOR),
            ("P-a", SelectAll, EDITOR),
            ("P-l", SelectLine, EDITOR),
            ("P-c", Copy, EDITOR),
            ("P-x", Cut, EDITOR),
            ("P-v", Paste, EDITOR),
            ("P-d", SelectNextOccurrence, EDITOR),
            ("P-shift-l", SelectAllOccurrences, EDITOR),
            ("alt-shift-i", SplitSelectionIntoLines, EDITOR),
            ("escape", Cancel, EDITOR),
            ("P-/", ToggleComment, EDITOR),
            ("alt-up", MoveLinesUp, EDITOR),
            ("alt-down", MoveLinesDown, EDITOR),
            ("P-shift-k", DeleteLines, EDITOR),
        ]
    );
    match p {
        Platform::Mac => b.extend(bindings!(
            p,
            [
                ("alt-left", MoveWordLeft, EDITOR),
                ("alt-right", MoveWordRight, EDITOR),
                ("alt-shift-left", SelectWordLeft, EDITOR),
                ("alt-shift-right", SelectWordRight, EDITOR),
                ("cmd-left", MoveLineStart, EDITOR),
                ("cmd-right", MoveLineEnd, EDITOR),
                ("ctrl-a", MoveLineStart, EDITOR),
                ("ctrl-e", MoveLineEnd, EDITOR),
                ("cmd-shift-left", SelectLineStart, EDITOR),
                ("cmd-shift-right", SelectLineEnd, EDITOR),
                ("cmd-up", MoveDocStart, EDITOR),
                ("cmd-down", MoveDocEnd, EDITOR),
                ("cmd-shift-up", SelectDocStart, EDITOR),
                ("cmd-shift-down", SelectDocEnd, EDITOR),
                ("alt-backspace", DeleteWordLeft, EDITOR),
                ("alt-delete", DeleteWordRight, EDITOR),
                ("cmd-backspace", DeleteToLineStart, EDITOR),
                ("alt-cmd-up", AddCursorAbove, EDITOR),
                ("alt-cmd-down", AddCursorBelow, EDITOR),
                ("alt-shift-up", CopyLinesUp, EDITOR),
                ("alt-shift-down", CopyLinesDown, EDITOR),
            ]
        )),
        Platform::Windows | Platform::Linux => b.extend(bindings!(
            p,
            [
                ("ctrl-left", MoveWordLeft, EDITOR),
                ("ctrl-right", MoveWordRight, EDITOR),
                ("ctrl-shift-left", SelectWordLeft, EDITOR),
                ("ctrl-shift-right", SelectWordRight, EDITOR),
                ("ctrl-home", MoveDocStart, EDITOR),
                ("ctrl-end", MoveDocEnd, EDITOR),
                ("ctrl-shift-home", SelectDocStart, EDITOR),
                ("ctrl-shift-end", SelectDocEnd, EDITOR),
                ("ctrl-backspace", DeleteWordLeft, EDITOR),
                ("ctrl-delete", DeleteWordRight, EDITOR),
                ("ctrl-shift-backspace", DeleteToLineStart, EDITOR),
                ("ctrl-y", Redo, EDITOR),
            ]
        )),
    }
    match p {
        Platform::Windows => b.extend(bindings!(
            p,
            [
                ("ctrl-alt-up", AddCursorAbove, EDITOR),
                ("ctrl-alt-down", AddCursorBelow, EDITOR),
                ("alt-shift-up", CopyLinesUp, EDITOR),
                ("alt-shift-down", CopyLinesDown, EDITOR),
            ]
        )),
        // shift-alt-up is taken by add-cursor on Linux, so copy-line moves to ctrl
        Platform::Linux => b.extend(bindings!(
            p,
            [
                ("alt-shift-up", AddCursorAbove, EDITOR),
                ("alt-shift-down", AddCursorBelow, EDITOR),
                ("ctrl-alt-shift-up", CopyLinesUp, EDITOR),
                ("ctrl-alt-shift-down", CopyLinesDown, EDITOR),
            ]
        )),
        Platform::Mac => {}
    }
    b
}

fn workspace(p: Platform) -> Vec<Binding> {
    let mut b = bindings!(
        p,
        [
            ("P-n", NewFile, None),
            ("P-o", OpenFile, None),
            ("P-s", Save, None),
            ("P-shift-s", SaveAs, None),
            ("P-w", CloseTab, None),
            ("ctrl-tab", NextTab, None),
            ("ctrl-shift-tab", PrevTab, None),
            ("P-=", ZoomIn, None),
            ("P-+", ZoomIn, None),
            ("P--", ZoomOut, None),
            ("P-0", ZoomReset, None),
        ]
    );
    match p {
        Platform::Mac => b.extend(bindings!(
            p,
            [
                ("cmd-shift-]", NextTab, None),
                ("cmd-shift-[", PrevTab, None),
                ("cmd-q", Quit, None),
            ]
        )),
        Platform::Windows => b.extend(bindings!(
            p,
            [
                ("ctrl-pagedown", NextTab, None),
                ("ctrl-pageup", PrevTab, None),
                ("alt-f4", Quit, None),
            ]
        )),
        Platform::Linux => b.extend(bindings!(
            p,
            [
                ("ctrl-pagedown", NextTab, None),
                ("ctrl-pageup", PrevTab, None),
                ("ctrl-q", Quit, None),
            ]
        )),
    }
    b
}

fn find(p: Platform) -> Vec<Binding> {
    let mut b = bindings!(
        p,
        [
            ("P-f", Find, None),
            ("f3", FindNext, None),
            ("shift-f3", FindPrev, None),
            ("enter", FindNext, FIND_QUERY),
            ("shift-enter", FindPrev, FIND_QUERY),
            ("alt-enter", SelectAllMatches, FIND_QUERY),
            ("tab", FocusReplace, FIND_QUERY),
            ("enter", ReplaceNext, REPLACE_QUERY),
            ("P-enter", ReplaceAll, REPLACE_QUERY),
            ("shift-tab", FocusFind, REPLACE_QUERY),
        ]
    );
    match p {
        Platform::Mac => b.extend(bindings!(
            p,
            [
                ("ctrl-f", Find, None),
                ("cmd-alt-f", FindReplace, None),
                ("ctrl-h", FindReplace, None),
                ("cmd-g", FindNext, None),
                ("cmd-shift-g", FindPrev, None),
                ("alt-cmd-c", ToggleCase, FIND_BAR),
                ("alt-cmd-w", ToggleWholeWord, FIND_BAR),
                ("alt-cmd-r", ToggleRegex, FIND_BAR),
                ("cmd-alt-enter", ReplaceAll, FIND_BAR),
            ]
        )),
        Platform::Windows | Platform::Linux => b.extend(bindings!(
            p,
            [
                ("ctrl-h", FindReplace, None),
                ("alt-c", ToggleCase, FIND_BAR),
                ("alt-w", ToggleWholeWord, FIND_BAR),
                ("alt-r", ToggleRegex, FIND_BAR),
                ("ctrl-alt-enter", ReplaceAll, FIND_BAR),
            ]
        )),
    }
    b
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use gpui::Keystroke;

    use super::*;

    const ALL: [Platform; 3] = [Platform::Mac, Platform::Windows, Platform::Linux];

    fn action_names(p: Platform) -> BTreeSet<&'static str> {
        bindings(p).iter().map(|b| (b.action)().name()).collect()
    }

    #[test]
    fn every_binding_parses() {
        for p in ALL {
            for b in bindings(p) {
                to_key_binding(&b).unwrap_or_else(|e| panic!("{p:?}: {e}"));
                assert!(!b.keys.contains("P-"), "{p:?}: unexpanded {}", b.keys);
            }
        }
    }

    #[test]
    fn every_action_is_bound_on_every_platform() {
        let mac = action_names(Platform::Mac);
        for p in [Platform::Windows, Platform::Linux] {
            let other = action_names(p);
            assert_eq!(
                mac.symmetric_difference(&other).collect::<Vec<_>>(),
                Vec::<&&str>::new(),
                "actions bound on only one of Mac / {p:?}"
            );
        }
    }

    #[test]
    fn no_key_is_bound_twice_in_the_same_context() {
        for p in ALL {
            let mut seen: HashMap<(Vec<Keystroke>, Option<&str>), &str> = HashMap::new();
            for b in bindings(p) {
                let keys: Vec<Keystroke> = b
                    .keys
                    .split_whitespace()
                    .map(|k| Keystroke::parse(k).unwrap())
                    .collect();
                let name = (b.action)().name();
                if let Some(prev) = seen.insert((keys, b.context), name) {
                    panic!("{p:?}: {} bound to both {prev} and {name}", b.keys);
                }
            }
        }
    }

    #[test]
    fn only_mac_uses_cmd() {
        for p in [Platform::Windows, Platform::Linux] {
            for b in bindings(p) {
                assert!(!b.keys.contains("cmd"), "{p:?}: {}", b.keys);
            }
        }
    }
}
