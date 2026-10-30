//! Headless UI tests: the real workspace in a gpui test window, driven by
//! simulated keystrokes through the platform keymap.

use std::path::PathBuf;

use gpui::{Entity, TestAppContext, VisualTestContext};

use crate::keymap::{self, Platform};
use crate::theme;
use crate::workspace::Workspace;

fn open(
    cx: &mut TestAppContext,
    paths: Vec<PathBuf>,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    cx.update(|cx| {
        theme::init_font(cx);
        keymap::install(cx);
    });
    cx.add_window_view(|window, cx| Workspace::new(paths, window, cx))
}

/// `P-` becomes the platform's primary modifier.
fn keys(s: &str) -> String {
    s.replace("P-", &format!("{}-", Platform::current().primary()))
}

fn add_cursor_below() -> &'static str {
    match Platform::current() {
        Platform::Mac => "alt-cmd-down",
        Platform::Windows => "ctrl-alt-down",
        Platform::Linux => "alt-shift-down",
    }
}

fn open_replace() -> &'static str {
    match Platform::current() {
        Platform::Mac => "cmd-alt-f",
        _ => "ctrl-h",
    }
}

fn toggle_regex() -> &'static str {
    match Platform::current() {
        Platform::Mac => "alt-cmd-r",
        _ => "alt-r",
    }
}

fn text(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    ws.read_with(cx, |ws, cx| ws.active_editor().read(cx).editor.text())
}

fn set_cursor(ws: &Entity<Workspace>, cx: &mut VisualTestContext, at: usize) {
    ws.update(cx, |ws, cx| {
        ws.active_editor()
            .update(cx, |e, _| e.editor.set_cursor(at))
    });
}

fn press(cx: &mut VisualTestContext, k: &str) {
    cx.simulate_keystrokes(&keys(k));
}

#[gpui::test]
fn typing_and_undo(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    cx.simulate_input("select 1");
    assert_eq!(text(&ws, cx), "select 1");
    press(cx, "enter");
    cx.simulate_input("from t");
    assert_eq!(text(&ws, cx), "select 1\nfrom t");
    // Enter starts a new undo step, like VS Code
    press(cx, "P-z");
    assert_eq!(text(&ws, cx), "select 1\n");
    press(cx, "P-z");
    assert_eq!(text(&ws, cx), "select 1");
    press(cx, "P-shift-z P-shift-z");
    assert_eq!(text(&ws, cx), "select 1\nfrom t");
}

#[gpui::test]
fn cmd_d_renames_every_occurrence(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    cx.simulate_input("select id, xid from t where id = 1");
    set_cursor(&ws, cx, 8);
    press(cx, "P-d P-d P-d");
    cx.simulate_input("user_id");
    // whole-word match started from a cursor, so xid is left alone
    assert_eq!(
        text(&ws, cx),
        "select user_id, xid from t where user_id = 1"
    );
}

#[gpui::test]
fn add_cursor_below_and_type(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    cx.simulate_input("a");
    press(cx, "enter");
    cx.simulate_input("b");
    press(cx, "enter");
    cx.simulate_input("c");
    set_cursor(&ws, cx, 0);
    press(cx, add_cursor_below());
    press(cx, add_cursor_below());
    cx.simulate_input("-");
    assert_eq!(text(&ws, cx), "-a\n-b\n-c");
    press(cx, "escape");
    let cursors = ws.read_with(cx, |ws, cx| {
        ws.active_editor().read(cx).editor.selections().len()
    });
    assert_eq!(cursors, 1);
}

#[gpui::test]
fn find_replace_flow(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    cx.simulate_input("foo bar foo");
    press(cx, "enter");
    cx.simulate_input("FOO");
    set_cursor(&ws, cx, 0);

    press(cx, "P-f");
    let find = ws.read_with(cx, |ws, _| ws.find_bar().clone());
    assert!(find.read_with(cx, |f, _| f.open));
    // seeded with the word under the cursor
    assert_eq!(find.read_with(cx, |f, _| f.match_count()), 3);

    press(cx, "escape");
    assert!(!find.read_with(cx, |f, _| f.open));

    // the query is already filled, so replace mode focuses the replace field
    press(cx, open_replace());
    cx.simulate_input("baz");
    // tab cycles between the fields instead of typing a tab
    press(cx, "tab");
    press(cx, "tab");
    press(cx, "P-enter");
    assert_eq!(text(&ws, cx), "baz bar baz\nbaz");

    // replace all is one undo step, run from the editor
    press(cx, "escape");
    press(cx, "P-z");
    assert_eq!(text(&ws, cx), "foo bar foo\nFOO");
}

#[gpui::test]
fn invalid_regex_is_reported(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    cx.simulate_input("a(b");
    press(cx, "P-f");
    press(cx, toggle_regex());
    // typed as-is: no bracket pairing in the search box
    cx.simulate_input("(");
    let find = ws.read_with(cx, |ws, _| ws.find_bar().clone());
    assert_eq!(find.read_with(cx, |f, cx| f.query_text(cx)), "(");
    assert!(find.read_with(cx, |f, _| f.has_error()));
    assert_eq!(find.read_with(cx, |f, _| f.match_count()), 0);
}

#[gpui::test]
fn open_edit_save_and_close(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("q.sql");
    std::fs::write(&path, "select 1;\r\n").unwrap();

    let (ws, cx) = open(cx, vec![path.clone()]);
    assert_eq!(ws.read_with(cx, |ws, _| ws.tab_count()), 1);
    assert_eq!(text(&ws, cx), "select 1;\n");

    set_cursor(&ws, cx, 8);
    cx.simulate_input("0");
    press(cx, "P-s");
    // line endings are kept as they were
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "select 10;\r\n");

    // closing with unsaved changes asks first
    cx.simulate_input("0");
    press(cx, "P-w");
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert_eq!(text(&ws, cx), "select 100;\n");

    press(cx, "P-w");
    cx.simulate_prompt_answer("Don't Save");
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "select 10;\r\n");
    // the last tab closing leaves a fresh untitled one
    assert_eq!(ws.read_with(cx, |ws, _| ws.tab_count()), 1);
    assert_eq!(text(&ws, cx), "");
}

#[gpui::test]
fn save_untitled_asks_for_a_path(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("new.sql");

    let (ws, cx) = open(cx, vec![]);
    press(cx, "P-n");
    assert_eq!(ws.read_with(cx, |ws, _| ws.tab_count()), 2);
    cx.simulate_input("select 2");
    press(cx, "P-s");
    assert!(cx.did_prompt_for_new_path());
    let chosen = target.clone();
    cx.simulate_new_path_selection(move |_| Some(chosen));
    cx.run_until_parked();

    assert_eq!(std::fs::read_to_string(&target).unwrap(), "select 2");
    let title = ws.read_with(cx, |ws, cx| ws.active_editor().read(cx).title());
    assert_eq!(title, "new.sql");
}

#[gpui::test]
fn window_close_with_unsaved_changes_asks(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    cx.simulate_input("unsaved");
    let may_close = ws.update_in(cx, |ws, window, cx| ws.should_close(window, cx));
    assert!(!may_close);
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert_eq!(text(&ws, cx), "unsaved");
}

#[gpui::test]
fn window_close_without_changes_does_not_ask(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    assert!(ws.update_in(cx, |ws, window, cx| ws.should_close(window, cx)));
    assert!(!cx.has_pending_prompt());
}
