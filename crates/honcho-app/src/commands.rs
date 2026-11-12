//! Every command the app has, in menu order. The macOS menu bar, the
//! in-window menu on Windows and Linux, and the command palette are all
//! built from this one list.

use std::ops::Range;

use gpui::{Action, Keystroke, Menu, MenuItem};

use crate::browser;
use crate::editor_view as ed;
use crate::find;
use crate::keymap::{self, Platform};
use crate::query;
use crate::workspace;

pub enum Entry {
    Command(&'static str, fn() -> Box<dyn Action>),
    Separator,
}

pub struct MenuSpec {
    pub name: &'static str,
    pub entries: Vec<Entry>,
}

macro_rules! menu {
    ($name:expr, [$($e:tt),* $(,)?]) => {
        MenuSpec { name: $name, entries: vec![$(menu!(@entry $e)),*] }
    };
    (@entry -) => { Entry::Separator };
    (@entry ($label:expr, $action:expr)) => { Entry::Command($label, || Box::new($action)) };
}

pub fn menus(p: Platform) -> Vec<MenuSpec> {
    let mut file = menu!(
        "File",
        [
            ("New File", workspace::NewFile),
            ("Open…", workspace::OpenFile),
            ("Open Project Folder…", workspace::OpenProject),
            -,
            ("Save", workspace::Save),
            ("Save As…", workspace::SaveAs),
            -,
            ("Close Tab", workspace::CloseTab),
        ]
    );
    // the app menu holds Quit on macOS; elsewhere it ends the File menu
    match p {
        Platform::Mac => {}
        Platform::Windows => file.entries.extend([
            Entry::Separator,
            Entry::Command("Exit", || Box::new(workspace::Quit)),
        ]),
        Platform::Linux => file.entries.extend([
            Entry::Separator,
            Entry::Command("Quit", || Box::new(workspace::Quit)),
        ]),
    }
    let mut out = Vec::new();
    if p == Platform::Mac {
        out.push(menu!("sql-honcho", [("Quit sql-honcho", workspace::Quit)]));
    }
    out.push(file);
    out.extend([
        menu!(
            "Edit",
            [
                ("Undo", ed::Undo),
                ("Redo", ed::Redo),
                -,
                ("Cut", ed::Cut),
                ("Copy", ed::Copy),
                ("Paste", ed::Paste),
                -,
                ("Find", find::Find),
                ("Replace", find::FindReplace),
                ("Find Next", find::FindNext),
                ("Find Previous", find::FindPrev),
                -,
                ("Toggle Line Comment", ed::ToggleComment),
                ("Indent", ed::Indent),
                ("Outdent", ed::Outdent),
            ]
        ),
        menu!(
            "Selection",
            [
                ("Select All", ed::SelectAll),
                ("Select Line", ed::SelectLine),
                -,
                ("Copy Line Up", ed::CopyLinesUp),
                ("Copy Line Down", ed::CopyLinesDown),
                ("Move Line Up", ed::MoveLinesUp),
                ("Move Line Down", ed::MoveLinesDown),
                ("Delete Line", ed::DeleteLines),
                -,
                ("Add Cursor Above", ed::AddCursorAbove),
                ("Add Cursor Below", ed::AddCursorBelow),
                ("Add Cursors to Line Ends", ed::SplitSelectionIntoLines),
                ("Add Next Occurrence", ed::SelectNextOccurrence),
                ("Select All Occurrences", ed::SelectAllOccurrences),
            ]
        ),
        menu!(
            "Connection",
            [
                ("Connections…", workspace::ManageConnections),
                -,
                ("Run Statement or Selection", query::RunStatement),
                ("Run All", query::RunAll),
                ("Explain", query::Explain),
                ("Explain Analyze", query::ExplainAnalyze),
                ("Cancel Query", query::CancelQuery),
                ("Toggle Results", query::TogglePanel),
                ("Refresh Completions", query::RefreshSchema),
            ]
        ),
        menu!(
            "View",
            [
                ("Command Palette…", workspace::ShowCommandPalette),
                -,
                ("Zoom In", workspace::ZoomIn),
                ("Zoom Out", workspace::ZoomOut),
                ("Reset Zoom", workspace::ZoomReset),
                -,
                ("Toggle Object Browser", browser::ToggleBrowser),
                ("Next Tab", workspace::NextTab),
                ("Previous Tab", workspace::PrevTab),
            ]
        ),
    ]);
    out
}

/// The menus for the macOS menu bar, which shows the shortcuts itself.
pub fn native_menus() -> Vec<Menu> {
    menus(Platform::current())
        .into_iter()
        .map(|m| Menu {
            name: m.name.into(),
            items: m
                .entries
                .into_iter()
                .map(|e| match e {
                    Entry::Separator => MenuItem::Separator,
                    Entry::Command(label, action) => MenuItem::Action {
                        name: label.into(),
                        action: action(),
                        os_action: None,
                    },
                })
                .collect(),
        })
        .collect()
}

/// A command as the palette lists it.
pub struct Command {
    /// "Menu: Item", so typing the menu's name finds its commands too.
    pub label: String,
    pub action: fn() -> Box<dyn Action>,
    pub shortcut: Option<String>,
}

pub fn palette_commands(p: Platform) -> Vec<Command> {
    let bindings = keymap::bindings(p);
    let mut out = Vec::new();
    for m in menus(p) {
        for e in m.entries {
            let Entry::Command(label, action) = e else {
                continue;
            };
            // the palette opening itself is no use
            if action().partial_eq(&workspace::ShowCommandPalette) {
                continue;
            }
            let menu = if m.name == "sql-honcho" {
                "App"
            } else {
                m.name
            };
            out.push(Command {
                label: format!("{menu}: {}", label.trim_end_matches('…')),
                action,
                shortcut: shortcut_in(&bindings, &*action(), p),
            });
        }
    }
    out
}

/// The first binding for the action that works from the editor or
/// anywhere, written the platform's way.
pub fn shortcut(action: &dyn Action, p: Platform) -> Option<String> {
    shortcut_in(&keymap::bindings(p), action, p)
}

fn shortcut_in(bindings: &[keymap::Binding], action: &dyn Action, p: Platform) -> Option<String> {
    let b = bindings
        .iter()
        .find(|b| matches!(b.context, None | Some("Editor")) && (b.action)().partial_eq(action))?;
    let parts: Option<Vec<String>> = b
        .keys
        .split(' ')
        .map(|k| Keystroke::parse(k).ok().map(|k| format_keystroke(&k, p)))
        .collect();
    Some(parts?.join(" "))
}

fn format_keystroke(k: &Keystroke, p: Platform) -> String {
    let m = &k.modifiers;
    let key = key_name(&k.key, p);
    if p == Platform::Mac {
        let mut s = String::new();
        for (on, sym) in [
            (m.control, "⌃"),
            (m.alt, "⌥"),
            (m.shift, "⇧"),
            (m.platform, "⌘"),
        ] {
            if on {
                s.push_str(sym);
            }
        }
        s + &key
    } else {
        let mut parts = Vec::new();
        for (on, name) in [
            (m.control, "Ctrl"),
            (m.alt, "Alt"),
            (m.shift, "Shift"),
            (m.platform, "Super"),
        ] {
            if on {
                parts.push(name.to_string());
            }
        }
        parts.push(key);
        parts.join("+")
    }
}

fn key_name(key: &str, p: Platform) -> String {
    let mac = p == Platform::Mac;
    let named = match key {
        "enter" => Some(if mac { "↩" } else { "Enter" }),
        "escape" => Some("Esc"),
        "tab" => Some("Tab"),
        "space" => Some("Space"),
        "backspace" => Some(if mac { "⌫" } else { "Backspace" }),
        "delete" => Some(if mac { "⌦" } else { "Delete" }),
        "up" => Some("↑"),
        "down" => Some("↓"),
        "left" => Some("←"),
        "right" => Some("→"),
        "home" => Some("Home"),
        "end" => Some("End"),
        "pageup" => Some("PgUp"),
        "pagedown" => Some("PgDn"),
        _ => None,
    };
    match named {
        Some(n) => n.to_string(),
        None => key.to_uppercase(),
    }
}

/// Where `query` matches `label` as a subsequence, ignoring case, and how
/// well: starts of words and runs of letters count most.
pub fn fuzzy(query: &str, label: &str) -> Option<(i32, Vec<Range<usize>>)> {
    let query: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    // the first try skips a letter mid-word when the same letter starts a
    // later word ("tlc" is Toggle Line Comment, not the t in Edit)
    [true, false]
        .into_iter()
        .filter_map(|prefer_starts| match_chars(&query, label, prefer_starts))
        .max_by_key(|(score, _)| *score)
}

fn match_chars(
    query: &[char],
    label: &str,
    prefer_starts: bool,
) -> Option<(i32, Vec<Range<usize>>)> {
    let chars: Vec<(usize, char)> = label.char_indices().collect();
    let lower = |c: char| c.to_lowercase().next().unwrap_or(c);
    let word_start = |i: usize| i == 0 || !chars[i - 1].1.is_alphanumeric();
    let mut score = 0;
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut qi = 0;
    let mut last_end: Option<usize> = None;
    for (ci, &(i, c)) in chars.iter().enumerate() {
        if qi == query.len() {
            break;
        }
        if lower(c) != query[qi] {
            continue;
        }
        let start = word_start(ci);
        let follows = last_end == Some(i);
        if prefer_starts
            && !start
            && !follows
            && chars[ci + 1..]
                .iter()
                .enumerate()
                .any(|(k, &(_, d))| lower(d) == query[qi] && word_start(ci + 1 + k))
        {
            continue;
        }
        score += 1 + if start { 6 } else { 0 } + if follows { 4 } else { 0 };
        let end = i + c.len_utf8();
        match ranges.last_mut() {
            Some(r) if follows => r.end = end,
            _ => ranges.push(i..end),
        }
        last_end = Some(end);
        qi += 1;
    }
    (qi == query.len()).then(|| (score - ranges.len() as i32, ranges))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_menu_command_is_in_the_palette_on_every_platform() {
        for p in [Platform::Mac, Platform::Windows, Platform::Linux] {
            let commands = palette_commands(p);
            for m in menus(p) {
                for e in m.entries {
                    if let Entry::Command(label, action) = e {
                        let in_palette =
                            commands.iter().any(|c| (c.action)().partial_eq(&*action()));
                        let is_palette = action().partial_eq(&workspace::ShowCommandPalette);
                        assert!(in_palette || is_palette, "{p:?}: {label}");
                    }
                }
            }
            assert!(
                commands
                    .iter()
                    .any(|c| (c.action)().partial_eq(&workspace::Quit)),
                "{p:?} can quit"
            );
        }
    }

    #[test]
    fn shortcuts_are_written_the_platforms_way() {
        let save = workspace::Save;
        assert_eq!(shortcut(&save, Platform::Mac).as_deref(), Some("⌘S"));
        assert_eq!(
            shortcut(&save, Platform::Windows).as_deref(),
            Some("Ctrl+S")
        );
        let palette = workspace::ShowCommandPalette;
        assert_eq!(shortcut(&palette, Platform::Mac).as_deref(), Some("⇧⌘P"));
        assert_eq!(
            shortcut(&palette, Platform::Linux).as_deref(),
            Some("Ctrl+Shift+P")
        );
        let run = query::RunStatement;
        assert_eq!(shortcut(&run, Platform::Mac).as_deref(), Some("⌘↩"));
        assert_eq!(
            shortcut(&workspace::ZoomOut, Platform::Linux).as_deref(),
            Some("Ctrl+-")
        );
        assert_eq!(
            shortcut(&workspace::Quit, Platform::Windows).as_deref(),
            Some("Alt+F4")
        );
    }

    #[test]
    fn fuzzy_prefers_word_starts_and_runs() {
        assert!(fuzzy("xyz", "File: Save").is_none());
        let (_, r) = fuzzy("sav", "File: Save As").unwrap();
        assert_eq!(r, vec![6..9]);
        let start = fuzzy("op", "File: Open…").unwrap().0;
        let middle = fuzzy("op", "Edit: Copy").unwrap().0;
        assert!(start > middle, "{start} {middle}");
        // across words, the way people abbreviate
        assert!(fuzzy("runall", "Connection: Run All").is_some());
        let (_, r) = fuzzy("tlc", "Edit: Toggle Line Comment").unwrap();
        assert_eq!(r, [6..7, 13..14, 18..19]);
        // a letter that starts no later word still matches mid-word
        assert!(fuzzy("dt", "Edit: Undo").is_some());
    }
}
