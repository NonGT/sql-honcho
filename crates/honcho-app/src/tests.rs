//! Headless UI tests: the real workspace in a gpui test window, driven by
//! simulated keystrokes through the platform keymap.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, AppContext, ClipboardItem, Entity, Focusable, TestAppContext, VisualTestContext};
use honcho_config::{ColorTag, ConnectionStore, MemorySecrets};
use honcho_db::{DbRuntime, DriverKind};

use crate::connection_dialog::ConnectionDialog;
use crate::connections::Connections;
use crate::grid::{CellRange, Format, ResultGrid};
use crate::keymap::{self, Platform};
use crate::query::QueryTab;
use crate::theme;
use crate::workspace::Workspace;

fn open(
    cx: &mut TestAppContext,
    paths: Vec<PathBuf>,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    cx.update(|cx| {
        theme::init_font(cx);
        keymap::install(cx);
        test_connections(cx);
    });
    cx.add_window_view(|window, cx| Workspace::new(paths, window, cx))
}

fn test_connections(cx: &mut App) {
    let dir = tempfile::tempdir().unwrap().keep();
    cx.set_global(Connections {
        store: ConnectionStore::empty(dir.join("connections.json")),
        secrets: Arc::new(MemorySecrets::default()),
        runtime: Arc::new(DbRuntime::new().unwrap()),
        load_error: None,
    });
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

#[gpui::test]
fn undoing_every_edit_clears_the_dirty_marker(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    let dirty = |ws: &Entity<Workspace>, cx: &mut VisualTestContext| {
        ws.read_with(cx, |ws, cx| ws.active_editor().read(cx).is_dirty())
    };
    cx.simulate_input("abc");
    press(cx, "enter");
    assert!(dirty(&ws, cx));
    press(cx, "P-z P-z");
    assert_eq!(text(&ws, cx), "");
    assert!(!dirty(&ws, cx));
    press(cx, "P-shift-z");
    assert!(dirty(&ws, cx));
}

#[gpui::test]
fn find_with_no_results_hides_selection_highlights(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    cx.simulate_input("qux QUERY qux");
    set_cursor(&ws, cx, 0);
    let searching = |ws: &Entity<Workspace>, cx: &mut VisualTestContext| {
        ws.read_with(cx, |ws, cx| ws.active_editor().read(cx).searching)
    };
    press(cx, "P-f");
    let find = ws.read_with(cx, |ws, _| ws.find_bar().clone());
    press(cx, "P-a");
    cx.simulate_input("qu");
    // the first match is now selected; the find highlights replace the
    // usual highlighting of other occurrences of the selection
    assert_eq!(find.read_with(cx, |f, _| f.match_count()), 3);
    press(
        cx,
        match Platform::current() {
            Platform::Mac => "alt-cmd-w",
            _ => "alt-w",
        },
    );
    assert_eq!(find.read_with(cx, |f, _| f.match_count()), 0);
    assert!(searching(&ws, cx));
    press(cx, "escape");
    assert!(!searching(&ws, cx));
}

#[gpui::test]
fn opening_a_file_gives_back_the_untitled_number(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.sql");
    std::fs::write(&path, "select 1").unwrap();
    let (ws, cx) = open(cx, vec![path]);
    press(cx, "P-n");
    let title = ws.read_with(cx, |ws, cx| ws.active_editor().read(cx).title());
    assert_eq!(title, "Untitled-1");
}

fn dialog(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<Entity<ConnectionDialog>> {
    ws.read_with(cx, |ws, _| ws.connection_dialog().cloned())
}

fn status(d: &Entity<ConnectionDialog>, cx: &mut VisualTestContext) -> Option<String> {
    d.read_with(cx, |d, _| d.status_text().map(str::to_string))
}

/// Fills the form field by field, the way a user tabs through it.
fn fill(cx: &mut VisualTestContext, values: [&str; 6]) {
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            press(cx, "tab");
        }
        cx.simulate_input(v);
    }
}

#[gpui::test]
fn saving_a_connection_keeps_the_password_out_of_the_file(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    press(cx, "P-shift-c");
    let d = dialog(&ws, cx).expect("dialog opens");
    fill(
        cx,
        ["shop prod", "db.example.com", "3307", "app", "pw!", "shop"],
    );
    d.update(cx, |d, cx| d.save(cx));
    cx.run_until_parked();
    assert_eq!(status(&d, cx).as_deref(), Some("Saved."));

    let (saved, path) = cx.update(|_, cx| {
        let state = cx.global::<Connections>();
        (
            state.store.connections().to_vec(),
            state.store.path().to_path_buf(),
        )
    });
    assert_eq!(saved.len(), 1);
    let c = &saved[0];
    assert_eq!(
        (c.name.as_str(), c.host.as_str(), c.port, c.user.as_str()),
        ("shop prod", "db.example.com", Some(3307), "app")
    );
    assert_eq!(c.database.as_deref(), Some("shop"));
    let pw = cx.update(|_, cx| c.password(cx.global::<Connections>().secrets.as_ref()));
    assert_eq!(pw.unwrap().as_deref(), Some("pw!"));
    let file = std::fs::read_to_string(path).unwrap();
    assert!(
        file.contains("db.example.com") && !file.contains("pw!"),
        "{file}"
    );
}

fn sqlite_file(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("shop.db");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(
            "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT);
             INSERT INTO items (name) VALUES ('ชา'), ('kafae');",
        )
        .unwrap();
    path
}

fn saved_connections(cx: &mut VisualTestContext) -> Vec<honcho_config::SavedConnection> {
    cx.update(|_, cx| cx.global::<Connections>().store.connections().to_vec())
}

#[gpui::test]
fn a_sqlite_connection_is_just_a_file(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    let dir = tempfile::tempdir().unwrap();
    let path = sqlite_file(dir.path());
    press(cx, "P-shift-c");
    let d = dialog(&ws, cx).unwrap();
    d.update(cx, |d, _| d.set_driver(DriverKind::Sqlite));
    cx.simulate_input("shop");
    d.update(cx, |d, cx| d.save(cx));
    cx.run_until_parked();
    assert_eq!(status(&d, cx).as_deref(), Some("Pick the database file."));

    // name, then straight to the file
    press(cx, "tab");
    cx.simulate_input(&path.display().to_string());
    press(cx, "enter");
    wait_until(cx, |cx| dialog(&ws, cx).is_none());
    let saved = saved_connections(cx);
    assert_eq!(saved.len(), 1);
    assert_eq!(
        (
            saved[0].host.as_str(),
            saved[0].port,
            saved[0].user.as_str()
        ),
        (path.display().to_string().as_str(), None, "")
    );
    let q = query_tab(&ws, cx);
    wait_until(cx, |cx| q.read_with(cx, |q, _| q.connection.is_some()));
    cx.simulate_input("select count(*) from items");
    run(&ws, cx, "P-enter");
    assert_eq!(result_rows(&ws, cx), [1]);
}

#[gpui::test]
fn opening_a_sqlite_file_connects_to_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = sqlite_file(dir.path());
    let (ws, cx) = open(cx, vec![path.clone()]);
    let q = query_tab(&ws, cx);
    wait_until(cx, |cx| q.read_with(cx, |q, _| q.connection.is_some()));
    assert_eq!(
        tab_target(&ws, cx).map(|t| t.0),
        Some("shop.db".to_string())
    );
    // it opened as a database, not as text
    assert_eq!(text(&ws, cx), "");
    cx.simulate_input("select name from items order by id");
    run(&ws, cx, "P-enter");
    assert_eq!(result_rows(&ws, cx), [2]);

    // again: a new tab, through the same saved connection
    ws.update_in(cx, |ws, window, cx| ws.open_paths(vec![path], window, cx));
    assert_eq!(ws.read_with(cx, |ws, _| ws.tab_count()), 2);
    assert_eq!(saved_connections(cx).len(), 1);
}

#[gpui::test]
fn password_field_wont_copy(cx: &mut TestAppContext) {
    let (_ws, cx) = open(cx, vec![]);
    press(cx, "P-shift-c");
    cx.write_to_clipboard(ClipboardItem::new_string("before".into()));
    fill(cx, ["x", "", "", "admin", "secret", ""]);
    press(cx, "shift-tab");
    press(cx, "P-a P-c");
    assert_eq!(
        cx.read_from_clipboard().and_then(|c| c.text()).as_deref(),
        Some("before")
    );
    press(cx, "shift-tab P-a P-c");
    assert_eq!(
        cx.read_from_clipboard().and_then(|c| c.text()).as_deref(),
        Some("admin"),
        "other fields still copy"
    );
}

#[gpui::test]
fn bad_input_is_reported_and_not_saved(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    press(cx, "P-shift-c");
    let d = dialog(&ws, cx).unwrap();
    d.update(cx, |d, cx| d.save(cx));
    cx.run_until_parked();
    assert_eq!(
        status(&d, cx).as_deref(),
        Some("Give the connection a name.")
    );
    fill(cx, ["local", "", "99999", "", "", ""]);
    d.update(cx, |d, cx| d.save(cx));
    cx.run_until_parked();
    assert!(status(&d, cx).unwrap().contains("Port 99999"));
    let count = cx.update(|_, cx| cx.global::<Connections>().store.connections().len());
    assert_eq!(count, 0);

    press(cx, "escape");
    assert!(dialog(&ws, cx).is_none(), "escape closes the dialog");
}

/// Steps the app until `done` holds. Sessions run on the database
/// runtime's own threads, which the test executor doesn't drive.
fn wait_until(cx: &mut VisualTestContext, mut done: impl FnMut(&mut VisualTestContext) -> bool) {
    cx.executor().allow_parking();
    // generous, for loaded CI runners; it returns as soon as `done` holds
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("timed out");
}

fn connect_fake(ws: &Entity<Workspace>, cx: &mut VisualTestContext, color: Option<ColorTag>) {
    connect_fake_with(ws, cx, color, false)
}

fn connect_fake_with(
    ws: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    color: Option<ColorTag>,
    auto_reconnect: bool,
) {
    connect_fake_as(ws, cx, "dev", color, auto_reconnect)
}

fn connect_fake_as(
    ws: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    user: &str,
    color: Option<ColorTag>,
    auto_reconnect: bool,
) {
    press(cx, "P-shift-c");
    let d = dialog(ws, cx).unwrap();
    d.update(cx, |d, _| {
        d.set_driver(DriverKind::Fake);
        d.set_color(color);
        d.set_auto_reconnect(auto_reconnect);
    });
    fill(cx, ["fake db", "", "", user, "", "app"]);
    press(cx, "enter");
    wait_until(cx, |cx| dialog(ws, cx).is_none());
    // the browser opens the connection the tab is on
    let b = ws.read_with(cx, |ws, _| ws.browser().clone());
    wait_until(cx, |cx| {
        b.read_with(cx, |b, _| b.is_idle() && b.labels().len() > 1)
    });
}

fn query_tab(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<QueryTab> {
    ws.read_with(cx, |ws, _| ws.active_query_tab().clone())
}

/// Runs with `keys` and waits for the query to finish.
fn run(ws: &Entity<Workspace>, cx: &mut VisualTestContext, keys: &str) -> Vec<String> {
    press(cx, keys);
    let q = query_tab(ws, cx);
    wait_until(cx, |cx| !q.read_with(cx, |q, _| q.is_running()));
    q.read_with(cx, |q, _| q.output_text())
}

fn result_rows(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<usize> {
    let q = query_tab(ws, cx);
    q.read_with(cx, |q, _| {
        q.output
            .iter()
            .flat_map(|o| o.sets.clone())
            .collect::<Vec<_>>()
    })
    .into_iter()
    .map(|g| g.read_with(cx, |g, _| g.rows.len()))
    .collect()
}

#[gpui::test]
fn connect_attaches_the_session_to_the_tab(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    let name = ws.read_with(cx, |ws, cx| {
        ws.active_connection(cx).map(|c| c.saved.name.clone())
    });
    assert_eq!(name.as_deref(), Some("fake db"));
}

fn tab_target(
    ws: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<(String, Option<String>)> {
    ws.read_with(cx, |ws, cx| {
        let q = ws.active_query_tab().read(cx);
        q.connection
            .as_ref()
            .map(|c| (c.saved.name.clone(), q.database.clone()))
    })
}

fn wait_for_target(ws: &Entity<Workspace>, cx: &mut VisualTestContext, database: &str) {
    wait_until(cx, |cx| {
        tab_target(ws, cx) == Some(("fake db".into(), Some(database.into())))
    });
}

/// A tab with no connection, as a file opens in.
fn open_unconnected(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.sql");
    std::fs::write(&path, "select 1;").unwrap();
    // an untouched tab would take the file in, connection and all
    cx.simulate_input("-- ");
    ws.update_in(cx, |ws, window, cx| ws.open_paths(vec![path], window, cx));
    assert_eq!(tab_target(ws, cx), None);
    dir
}

#[gpui::test]
fn new_tabs_open_where_the_current_tab_or_the_browser_is(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    // connecting a tab opened the tree down to its database
    let b = ws.read_with(cx, |ws, _| ws.browser().clone());
    assert_eq!(
        b.read_with(cx, |b, cx| b.selected_target(cx))
            .map(|(s, d)| (s.name, d)),
        Some(("fake db".into(), Some("app".into())))
    );

    press(cx, "P-n");
    assert_eq!(ws.read_with(cx, |ws, _| ws.tab_count()), 2);
    wait_for_target(&ws, cx, "app");

    // a tab without a connection goes by what the browser has selected
    let _dir = open_unconnected(&ws, cx);
    let id = b.read_with(cx, |b, cx| b.open_connections(cx)[0].id.clone());
    b.update(cx, |b, cx| b.reveal(&id, Some("analytics"), cx));
    press(cx, "P-n");
    wait_for_target(&ws, cx, "analytics");
}

#[gpui::test]
fn the_connection_chip_offers_what_the_browser_has_open(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    let pick = |cx: &mut VisualTestContext| {
        let q = query_tab(&ws, cx);
        q.update(cx, |_, cx| cx.emit(crate::query::TabEvent::PickConnection));
        cx.run_until_parked();
        q.read_with(cx, |q, _| q.connection_names())
    };
    // nothing open yet: straight to the dialog
    assert_eq!(pick(cx), None);
    assert!(dialog(&ws, cx).is_some());
    press(cx, "escape");
    wait_until(cx, |cx| dialog(&ws, cx).is_none());

    connect_fake(&ws, cx, None);
    let _dir = open_unconnected(&ws, cx);
    assert_eq!(pick(cx), Some(vec!["fake db".to_string()]));
    press(cx, "escape");
    assert_eq!(
        query_tab(&ws, cx).read_with(cx, |q, _| q.connection_names()),
        None
    );

    pick(cx);
    press(cx, "enter");
    wait_for_target(&ws, cx, "app");

    // past the list is the dialog
    pick(cx);
    press(cx, "down");
    press(cx, "enter");
    assert!(dialog(&ws, cx).is_some());
}

#[gpui::test]
fn use_in_this_tab_connects_the_current_tab(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    let _dir = open_unconnected(&ws, cx);
    let b = ws.read_with(cx, |ws, _| ws.browser().clone());
    b.update(cx, |b, cx| b.act("analytics", "use", cx));
    wait_for_target(&ws, cx, "analytics");
    assert_eq!(ws.read_with(cx, |ws, _| ws.tab_count()), 2);

    // a tab that recreates an object keeps its server; a new tab gets it
    browse(&ws, cx, "Procedures (1)");
    b.update(cx, |b, cx| b.act("refresh_stats", "definition", cx));
    wait_until(cx, |cx| ws.read_with(cx, |ws, _| ws.tab_count() == 3));
    let q = query_tab(&ws, cx);
    wait_until(cx, |cx| q.read_with(cx, |q, _| q.origin.is_some()));
    let tabs = ws.read_with(cx, |ws, _| ws.tab_count());
    b.update(cx, |b, cx| b.act("fake db", "use", cx));
    cx.run_until_parked();
    assert_eq!(ws.read_with(cx, |ws, _| ws.tab_count()), tabs + 1);
    wait_until(cx, |cx| tab_target(&ws, cx).is_some());
}

fn pick_database(ws: &Entity<Workspace>, cx: &mut VisualTestContext, name: &str) {
    let q = query_tab(ws, cx);
    q.update_in(cx, |q, window, cx| q.pick_database(name.into(), window, cx));
    cx.run_until_parked();
}

#[gpui::test]
fn switching_away_from_an_open_transaction_asks_first(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    // like Postgres: another database means another connection
    connect_fake_as(&ws, cx, "pg", None, false);
    wait_for_target(&ws, cx, "app");
    cx.simulate_input("begin");
    run(&ws, cx, "P-enter");

    pick_database(&ws, cx, "analytics");
    wait_until(cx, |cx| cx.has_pending_prompt());
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert_eq!(
        tab_target(&ws, cx),
        Some(("fake db".into(), Some("app".into())))
    );

    pick_database(&ws, cx, "analytics");
    wait_until(cx, |cx| cx.has_pending_prompt());
    cx.simulate_prompt_answer("Roll back and switch");
    wait_for_target(&ws, cx, "analytics");

    // nothing open now, so nothing to ask
    pick_database(&ws, cx, "app");
    wait_for_target(&ws, cx, "app");
    assert!(!cx.has_pending_prompt());

    // "use in this tab" replaces the connection whatever the driver
    run(&ws, cx, "P-enter");
    let b = ws.read_with(cx, |ws, _| ws.browser().clone());
    b.update(cx, |b, cx| b.act("analytics", "use", cx));
    cx.run_until_parked();
    wait_until(cx, |cx| cx.has_pending_prompt());
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert_eq!(
        tab_target(&ws, cx),
        Some(("fake db".into(), Some("app".into())))
    );
}

#[gpui::test]
fn use_keeps_the_transaction_so_nothing_is_asked(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    wait_for_target(&ws, cx, "app");
    cx.simulate_input("begin");
    run(&ws, cx, "P-enter");
    pick_database(&ws, cx, "analytics");
    wait_for_target(&ws, cx, "analytics");
    assert!(!cx.has_pending_prompt());
}

#[gpui::test]
fn run_all_then_the_statement_at_the_cursor(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    cx.simulate_input("select 3;\nupdate 2;");

    // the statement under the cursor is marked before anything runs
    let target = ws.read_with(cx, |ws, cx| ws.active_editor().read(cx).run_target.clone());
    assert_eq!(target, Some(10..18));

    let messages = run(&ws, cx, "P-shift-enter");
    assert_eq!(result_rows(&ws, cx), [3]);
    assert!(
        messages.iter().any(|m| m == "2 rows affected"),
        "{messages:?}"
    );

    set_cursor(&ws, cx, 2);
    let messages = run(&ws, cx, "P-enter");
    assert_eq!(result_rows(&ws, cx), [3]);
    assert!(
        !messages.iter().any(|m| m.contains("affected")),
        "{messages:?}"
    );
}

#[gpui::test]
fn running_without_a_connection_opens_the_dialog(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    cx.simulate_input("select 1");
    press(cx, "P-enter");
    assert!(dialog(&ws, cx).is_some());
}

#[gpui::test]
fn red_connections_confirm_writes_without_where(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, Some(ColorTag::Red));
    cx.simulate_input("update 5");
    press(cx, "P-enter");
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Cancel");
    assert_eq!(
        query_tab(&ws, cx).read_with(cx, |q, _| q.output_text()),
        Vec::<String>::new()
    );

    press(cx, "P-enter");
    cx.simulate_prompt_answer("Run");
    let q = query_tab(&ws, cx);
    wait_until(cx, |cx| {
        q.read_with(cx, |q, _| {
            q.output_text().iter().any(|m| m == "5 rows affected")
        })
    });
}

#[gpui::test]
fn cancel_stops_a_running_query(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    cx.simulate_input("sleep 30000");
    press(cx, "P-enter");
    let q = query_tab(&ws, cx);
    wait_until(cx, |cx| q.read_with(cx, |q, _| q.is_running()));
    let messages = run(&ws, cx, "P-.");
    assert!(
        messages.iter().any(|m| m == "query cancelled"),
        "{messages:?}"
    );
}

#[gpui::test]
fn duplicate_and_delete(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    press(cx, "P-shift-c");
    let d = dialog(&ws, cx).unwrap();
    fill(cx, ["a", "h", "", "u", "pw", ""]);
    d.update(cx, |d, cx| d.save(cx));
    cx.run_until_parked();
    let id = d.read_with(cx, |d, _| d.selected_id().unwrap().to_string());
    d.update_in(cx, |d, window, cx| d.delete_now(&id, window, cx));
    cx.run_until_parked();
    let left = cx.update(|_, cx| {
        let state = cx.global::<Connections>();
        (
            state.store.connections().len(),
            state.secrets.get(&format!("{id}/password")).unwrap(),
        )
    });
    assert_eq!(left, (0, None));
    assert!(d.read_with(cx, |d, _| d.selected_id().is_none()));
}

fn first_grid(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<ResultGrid> {
    let q = query_tab(ws, cx);
    q.read_with(cx, |q, _| q.output.as_ref().unwrap().sets[0].clone())
}

fn clipboard(cx: &mut VisualTestContext) -> Option<String> {
    cx.read_from_clipboard().and_then(|c| c.text())
}

#[gpui::test]
fn grid_sorts_and_copies_a_block(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    cx.simulate_input("select 5");
    run(&ws, cx, "P-enter");
    let grid = first_grid(&ws, cx);

    // descending by id: 4, 3, 2, 1, 0
    grid.update(cx, |g, cx| {
        g.toggle_sort(0, cx);
        g.toggle_sort(0, cx);
    });
    assert_eq!(grid.read_with(cx, |g, _| g.sort), Some((0, false)));
    grid.update_in(cx, |g, window, cx| {
        window.focus(&g.focus_handle(cx));
        g.selection = Some(CellRange {
            anchor: (1, 0),
            head: (2, 1),
        });
    });
    press(cx, "P-c");
    assert_eq!(clipboard(cx).as_deref(), Some("3\trow 3\n2\trow 2\n"));

    // arrows move, shift extends
    press(cx, "down shift-left shift-up");
    let sel = grid.read_with(cx, |g, _| g.selection).unwrap();
    assert_eq!((sel.rows(), sel.cols()), (2..4, 0..2));

    press(cx, "P-end");
    let sel = grid.read_with(cx, |g, _| g.selection).unwrap();
    assert_eq!(sel.head.0, 4);
    press(cx, "P-home");
    let sel = grid.read_with(cx, |g, _| g.selection).unwrap();
    assert_eq!(sel.head.0, 0);

    // a third click on the header goes back to the server's order
    grid.update(cx, |g, cx| g.toggle_sort(0, cx));
    press(cx, "P-a P-c");
    assert_eq!(
        clipboard(cx).as_deref(),
        Some("0\trow 0\n1\trow 1\n2\trow 2\n3\trow 3\n4\trow 4\n")
    );
}

#[gpui::test]
fn double_click_on_a_header_edge_fits_the_column(cx: &mut TestAppContext) {
    use gpui::{px, Modifiers, MouseButton};
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    cx.simulate_input("select 5");
    run(&ws, cx, "P-enter");
    let grid = first_grid(&ws, cx);
    let width = |cx: &mut VisualTestContext, c| grid.read_with(cx, |g, _| g.width(c));
    assert_eq!(width(cx, 1), px(150.));

    let at = grid.read_with(cx, |g, _| g.header_edge(1));
    cx.simulate_click(at, Modifiers::none());
    cx.simulate_event(gpui::MouseDownEvent {
        position: at,
        button: MouseButton::Left,
        modifiers: Modifiers::none(),
        click_count: 2,
        first_mouse: false,
    });
    cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    // "row 4" and the header fit in much less than the default
    let fitted = width(cx, 1);
    assert!(fitted < px(100.) && fitted > px(40.), "{fitted:?}");
    // the other column and the selection are left alone
    assert_eq!(width(cx, 0), px(150.));
    assert!(grid.read_with(cx, |g, _| g.selection.is_none()));
}

#[gpui::test]
fn copying_results_says_so_for_a_moment(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    cx.simulate_input("select 5");
    run(&ws, cx, "P-enter");
    let q = query_tab(&ws, cx);
    let toast = |cx: &mut VisualTestContext| q.read_with(cx, |q, _| q.toast().map(String::from));
    assert_eq!(toast(cx), None);

    let at = cx.debug_bounds("CSV").expect("the copy button").center();
    cx.simulate_click(at, gpui::Modifiers::none());
    assert_eq!(toast(cx).as_deref(), Some("Copied 5 rows as CSV"));
    assert!(clipboard(cx).is_some_and(|c| c.starts_with("id,")));

    // a selection copies only its rows
    first_grid(&ws, cx).update(cx, |g, _| {
        g.selection = Some(CellRange {
            anchor: (2, 0),
            head: (2, 1),
        })
    });
    let at = cx.debug_bounds("TSV").unwrap().center();
    cx.simulate_click(at, gpui::Modifiers::none());
    assert_eq!(toast(cx).as_deref(), Some("Copied 1 row as TSV"));

    cx.executor()
        .advance_clock(std::time::Duration::from_millis(2000));
    cx.run_until_parked();
    assert_eq!(toast(cx), None);
}

#[gpui::test]
fn copy_buttons_and_the_viewer(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    cx.simulate_input("types");
    run(&ws, cx, "P-enter");
    let grid = first_grid(&ws, cx);
    let json = grid.read_with(cx, |g, _| g.export(Format::Json));
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed[0]["n"], 42);
    assert_eq!(parsed[0]["z"], serde_json::Value::Null);
    assert_eq!(parsed[0]["t"], "สวัสดี 😀");

    // enter opens the full value, escape closes it
    grid.update_in(cx, |g, window, cx| {
        window.focus(&g.focus_handle(cx));
        g.selection = Some(CellRange {
            anchor: (0, 2),
            head: (0, 2),
        });
    });
    press(cx, "enter");
    assert!(grid.read_with(cx, |g, cx| g.viewer_text(cx)).as_deref() == Some("สวัสดี 😀"));
    press(cx, "escape");
    assert!(grid.read_with(cx, |g, cx| g.viewer_text(cx)).is_none());
}

#[gpui::test]
fn results_panel_toggles(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    cx.simulate_input("select 1");
    run(&ws, cx, "P-enter");
    let q = query_tab(&ws, cx);
    press(cx, "P-j");
    assert!(q.read_with(cx, |q, _| q.panel_collapsed()));
    press(cx, "P-j");
    assert!(!q.read_with(cx, |q, _| q.panel_collapsed()));
}

fn wait_for_message(ws: &Entity<Workspace>, cx: &mut VisualTestContext, text: &str) {
    let q = query_tab(ws, cx);
    wait_until(cx, |cx| {
        q.read_with(cx, |q, _| {
            !q.is_running() && q.output_text().iter().any(|m| m == text)
        })
    });
}

#[gpui::test]
fn auto_reconnect_after_the_server_drops_a_query(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake_with(&ws, cx, None, true);
    cx.simulate_input("lose");
    press(cx, "P-enter");
    wait_for_message(
        &ws,
        cx,
        "Reconnected. The query that was cut off wasn't run again.",
    );
    let q = query_tab(&ws, cx);
    assert!(q.read_with(cx, |q, _| q.lost.is_none()));

    press(cx, "P-a");
    cx.simulate_input("select 2");
    run(&ws, cx, "P-enter");
    assert_eq!(result_rows(&ws, cx), [2]);
}

#[gpui::test]
fn without_auto_reconnect_a_lost_connection_waits_for_the_user(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    cx.simulate_input("lose");
    run(&ws, cx, "P-enter");
    let q = query_tab(&ws, cx);
    assert!(q.read_with(cx, |q, _| q.lost.is_some()));

    press(cx, "P-a");
    cx.simulate_input("select 2");
    let messages = run(&ws, cx, "P-enter");
    assert_eq!(messages, ["The connection was lost. Reconnect first."]);
}

fn open_database_list(ws: &Entity<Workspace>, cx: &mut VisualTestContext) {
    let q = query_tab(ws, cx);
    q.update(cx, |q, cx| q.open_databases(cx));
    wait_until(cx, |cx| q.read_with(cx, |q, _| q.databases_open()));
}

#[gpui::test]
fn database_list_closes_on_escape_or_a_click_outside(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    let q = query_tab(&ws, cx);

    open_database_list(&ws, cx);
    press(cx, "escape");
    assert!(!q.read_with(cx, |q, _| q.databases_open()));

    open_database_list(&ws, cx);
    cx.simulate_click(
        gpui::point(gpui::px(600.), gpui::px(400.)),
        Default::default(),
    );
    assert!(!q.read_with(cx, |q, _| q.databases_open()));

    // keyboard: app is highlighted, the next one is analytics
    open_database_list(&ws, cx);
    press(cx, "down enter");
    wait_until(cx, |cx| {
        q.read_with(cx, |q, _| q.database.as_deref() == Some("analytics"))
    });
    assert!(!q.read_with(cx, |q, _| q.databases_open()));
}

#[gpui::test]
fn database_list_reconnects_when_set_to(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    cx.simulate_input("lose");
    run(&ws, cx, "P-enter");
    let q = query_tab(&ws, cx);
    assert!(q.read_with(cx, |q, _| q.lost.is_some()));

    q.update(cx, |q, _| q.set_auto_reconnect(true));
    open_database_list(&ws, cx);
    assert!(q.read_with(cx, |q, _| q.lost.is_none()));
}

#[gpui::test]
fn tls_settings_are_saved(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    press(cx, "P-shift-c");
    let d = dialog(&ws, cx).unwrap();
    fill(cx, ["secure", "db.example.com", "", "app", "", ""]);
    d.update_in(cx, |d, window, cx| {
        d.show_tls(honcho_db::TlsMode::VerifyFull, window, cx)
    });
    // the TLS tab's fields: CA, client cert, client key
    cx.simulate_input("/etc/ca.pem");
    press(cx, "tab");
    cx.simulate_input("/home/me/client.pem");
    d.update(cx, |d, cx| d.save(cx));
    cx.run_until_parked();
    assert_eq!(
        status(&d, cx).as_deref(),
        Some("A client certificate needs both the certificate and its key.")
    );

    press(cx, "tab");
    cx.simulate_input("/home/me/client-key.pem");
    d.update(cx, |d, cx| d.save(cx));
    cx.run_until_parked();
    assert_eq!(status(&d, cx).as_deref(), Some("Saved."));
    let tls = cx.update(|_, cx| {
        cx.global::<Connections>().store.connections()[0]
            .tls
            .clone()
    });
    assert_eq!(tls.mode, honcho_db::TlsMode::VerifyFull);
    assert_eq!(
        tls.ca_file.as_deref(),
        Some(std::path::Path::new("/etc/ca.pem"))
    );
    assert_eq!(
        tls.key_file.as_deref(),
        Some(std::path::Path::new("/home/me/client-key.pem"))
    );
}

#[gpui::test]
fn ssh_settings_are_saved_with_the_secret_in_the_keyring(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    press(cx, "P-shift-c");
    let d = dialog(&ws, cx).unwrap();
    fill(
        cx,
        ["behind bastion", "db.internal", "", "app", "db pw", ""],
    );
    d.update_in(cx, |d, window, cx| {
        d.show_ssh(honcho_db::tunnel::SshAuth::Password, window, cx)
    });
    d.update(cx, |d, cx| d.save(cx));
    cx.run_until_parked();
    assert_eq!(
        status(&d, cx).as_deref(),
        Some("Give the SSH host, or turn SSH off.")
    );

    // host, port, user, password: the key file field is hidden for passwords
    cx.simulate_input("bastion");
    press(cx, "tab");
    cx.simulate_input("2222");
    press(cx, "tab");
    cx.simulate_input("ops");
    press(cx, "tab");
    cx.simulate_input("ssh pw");
    d.update(cx, |d, cx| d.save(cx));
    cx.run_until_parked();
    assert_eq!(status(&d, cx).as_deref(), Some("Saved."));

    let (saved, ssh_secret) = cx.update(|_, cx| {
        let state = cx.global::<Connections>();
        let c = state.store.connections()[0].clone();
        let secret = c.ssh_secret(state.secrets.as_ref()).unwrap();
        (c, secret)
    });
    let ssh = saved.ssh.unwrap();
    assert_eq!(
        (ssh.host.as_str(), ssh.port, ssh.user.as_str()),
        ("bastion", Some(2222), "ops")
    );
    assert_eq!(ssh_secret.as_deref(), Some("ssh pw"));
}

fn browser_labels(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
    let b = ws.read_with(cx, |ws, _| ws.browser().clone());
    b.read_with(cx, |b, _| b.labels())
}

/// Clicks a row and waits for whatever it loads.
fn browse(ws: &Entity<Workspace>, cx: &mut VisualTestContext, label: &str) {
    let b = ws.read_with(cx, |ws, _| ws.browser().clone());
    b.update(cx, |b, cx| b.expand(label, cx));
    wait_until(cx, |cx| b.read_with(cx, |b, _| b.is_idle()));
}

#[gpui::test]
fn object_browser_loads_as_nodes_open(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    assert_eq!(
        browser_labels(&ws, cx),
        [
            "fake db",
            "  app",
            "    Tables (2)",
            "    Views (1)",
            "    Procedures (1)",
            "    Functions (1)",
            "  analytics",
        ]
    );

    browse(&ws, cx, "fake db");
    browse(&ws, cx, "app");
    browse(&ws, cx, "Tables (2)");
    browse(&ws, cx, "orders");
    browse(&ws, cx, "Columns");
    browse(&ws, cx, "Indexes");
    assert_eq!(
        browser_labels(&ws, cx),
        [
            "fake db",
            "  app",
            "    Tables (2)",
            "      orders",
            "        Columns",
            "          id",
            "          user_id",
            "          amount",
            "        Indexes",
            "          PRIMARY",
            "          user_id",
            "      users",
            "    Views (1)",
            "    Procedures (1)",
            "    Functions (1)",
            "  analytics",
        ]
    );

    // a template goes into the current editor
    let b = ws.read_with(cx, |ws, _| ws.browser().clone());
    b.update(cx, |b, cx| {
        b.template_for("users", crate::browser::Template::Select, cx)
    });
    wait_until(cx, |cx| text(&ws, cx).contains("FROM `app`.`users`"));
    assert_eq!(
        text(&ws, cx),
        "SELECT `id`, `email`\nFROM `app`.`users`\nLIMIT 100;\n"
    );
}

#[gpui::test]
fn double_click_opens_a_connected_tab(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    browse(&ws, cx, "fake db");
    browse(&ws, cx, "app");
    browse(&ws, cx, "Tables (2)");
    let b = ws.read_with(cx, |ws, _| ws.browser().clone());
    b.update(cx, |b, cx| b.click("users", true, cx));
    wait_until(cx, |cx| {
        ws.read_with(cx, |ws, cx| {
            ws.tab_count() == 2 && ws.active_connection(cx).is_some()
        })
    });
    assert_eq!(text(&ws, cx), "SELECT * FROM `app`.`users` LIMIT 1000;");
}

#[gpui::test]
fn filter_searches_nodes_that_were_never_opened(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    browse(&ws, cx, "fake db");
    let b = ws.read_with(cx, |ws, _| ws.browser().clone());
    b.update(cx, |b, cx| b.set_filter("total", cx));
    // the search waits for a pause in typing
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(300));
    wait_until(cx, |cx| {
        b.read_with(cx, |b, _| {
            b.labels().iter().any(|l| l.trim() == "order_total")
        })
    });
    let labels = browser_labels(&ws, cx);
    assert!(
        labels.contains(&"    Functions (1)".to_string()),
        "{labels:?}"
    );
    assert!(!labels.iter().any(|l| l.trim() == "orders"), "{labels:?}");

    b.update(cx, |b, cx| b.set_filter("", cx));
    assert_eq!(browser_labels(&ws, cx).len(), 7);
}

#[gpui::test]
fn expanded_nodes_come_back_next_time(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    browse(&ws, cx, "fake db");
    browse(&ws, cx, "app");
    browse(&ws, cx, "Views (1)");
    let before = browser_labels(&ws, cx);
    cx.run_until_parked();

    // a fresh browser, as after a restart: opening the connection brings
    // the rest back
    let fresh = cx.update(|_, cx| {
        cx.new(|cx| {
            let mut b = crate::browser::ObjectBrowser::new(cx);
            b.restore_expanded(cx);
            b
        })
    });
    assert_eq!(fresh.read_with(cx, |b, _| b.labels()), ["fake db"]);
    fresh.update(cx, |b, cx| b.click("fake db", false, cx));
    wait_until(cx, |cx| {
        fresh.read_with(cx, |b, _| b.is_idle() && b.labels() == before)
    });
}

fn menu(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<Vec<String>> {
    ws.read_with(cx, |ws, cx| {
        ws.active_editor()
            .read(cx)
            .completion
            .as_ref()
            .map(|m| m.items.iter().map(|i| i.label.clone()).collect())
    })
}

/// Waits until the menu's first item is `first`; metadata arrives from
/// the fake driver on another thread.
fn wait_for_menu(ws: &Entity<Workspace>, cx: &mut VisualTestContext, first: &str) {
    wait_until(cx, |cx| {
        menu(ws, cx).is_some_and(|m| m.first().is_some_and(|f| f == first))
    });
}

#[gpui::test]
fn completes_tables_and_alias_columns(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    cx.simulate_input("SELECT * FROM ord");
    wait_for_menu(&ws, cx, "orders");
    press(cx, "tab");
    assert_eq!(text(&ws, cx), "SELECT * FROM orders");
    assert_eq!(menu(&ws, cx), None);

    cx.simulate_input(" o WHERE o.");
    wait_for_menu(&ws, cx, "id");
    assert_eq!(menu(&ws, cx).unwrap(), ["id", "user_id", "amount"]);
    press(cx, "down tab");
    assert_eq!(text(&ws, cx), "SELECT * FROM orders o WHERE o.user_id");
}

#[gpui::test]
fn keywords_complete_without_a_connection(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    cx.simulate_input("sel");
    assert_eq!(menu(&ws, cx).unwrap()[0], "SELECT");
    press(cx, "escape");
    assert_eq!(menu(&ws, cx), None);
    // Enter without picking an item is just a new line
    press(cx, "enter");
    assert_eq!(text(&ws, cx), "sel\n");
    press(cx, "backspace ctrl-space");
    assert!(menu(&ws, cx).is_some());
    press(cx, "down up enter");
    // typed in lowercase, so the keyword goes in lowercase
    assert_eq!(text(&ws, cx), "select");

    // moving away closes the menu
    cx.simulate_input(" fr");
    assert!(menu(&ws, cx).is_some());
    press(cx, "left");
    assert_eq!(menu(&ws, cx), None);
}

/// Tests that switch `REFUSE_FLAKY` take turns, since it's process-wide.
static FLAKY: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn browser_notes(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
    let b = ws.read_with(cx, |ws, _| ws.browser().clone());
    b.read_with(cx, |b, _| b.notes())
}

fn drop_browser_session(ws: &Entity<Workspace>, cx: &mut VisualTestContext) {
    let b = ws.read_with(cx, |ws, _| ws.browser().clone());
    let task = b.read_with(cx, |b, cx| b.drop_session(cx));
    cx.executor().allow_parking();
    cx.background_executor.block_test(task);
}

#[gpui::test]
fn tree_reconnects_when_its_session_drops(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    browse(&ws, cx, "fake db");
    browse(&ws, cx, "app");
    browse(&ws, cx, "Tables (2)");
    browse(&ws, cx, "orders");
    let before = browser_labels(&ws, cx);
    drop_browser_session(&ws, cx);

    let b = ws.read_with(cx, |ws, _| ws.browser().clone());
    b.update(cx, |b, cx| b.click("Columns", false, cx));
    // the tree doesn't fold up and reload while the session reopens: every
    // row that was there stays there throughout
    wait_until(cx, |cx| {
        let labels = b.read_with(cx, |b, _| b.labels());
        for row in &before {
            assert!(labels.contains(row), "{row} went away: {labels:?}");
        }
        b.read_with(cx, |b, _| b.is_idle()) && labels.iter().any(|l| l.trim() == "amount")
    });
    assert!(
        browser_notes(&ws, cx).is_empty(),
        "{:?}",
        browser_notes(&ws, cx)
    );
}

#[gpui::test]
fn tree_says_lost_when_reconnecting_fails(cx: &mut TestAppContext) {
    let _flaky = FLAKY.lock().unwrap_or_else(|e| e.into_inner());
    let (ws, cx) = open(cx, vec![]);
    connect_fake_as(&ws, cx, "flaky", None, false);
    browse(&ws, cx, "fake db");
    browse(&ws, cx, "app");
    drop_browser_session(&ws, cx);
    honcho_db::fake::REFUSE_FLAKY.store(true, std::sync::atomic::Ordering::SeqCst);
    browse(&ws, cx, "Tables (2)");
    browse(&ws, cx, "orders");
    browse(&ws, cx, "Columns");
    let notes = browser_notes(&ws, cx);
    honcho_db::fake::REFUSE_FLAKY.store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(
        notes
            .iter()
            .any(|n| n.starts_with("Connection lost and reconnecting failed")),
        "{notes:?}"
    );
}

#[gpui::test]
fn completion_reconnects_when_its_session_drops(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    cx.simulate_input("SELECT * FROM orders o WHERE o.");
    wait_for_menu(&ws, cx, "id");

    let catalog = query_tab(&ws, cx).read_with(cx, |q, _| q.catalog_for_test().unwrap());
    let task = catalog.read_with(cx, |c, cx| c.drop_session(cx));
    cx.background_executor.block_test(task);

    // users' columns haven't been fetched, so this needs the dropped session
    cx.simulate_input(";\nSELECT * FROM users u WHERE u.");
    wait_for_menu(&ws, cx, "id");
    let note = ws.read_with(cx, |ws, cx| {
        ws.active_editor()
            .read(cx)
            .completion
            .as_ref()
            .and_then(|m| m.note.clone())
    });
    assert_eq!(note, None);
    assert_eq!(menu(&ws, cx).unwrap(), ["id", "email"]);
}

#[gpui::test]
fn completion_notes_a_lost_connection(cx: &mut TestAppContext) {
    let _flaky = FLAKY.lock().unwrap_or_else(|e| e.into_inner());
    let (ws, cx) = open(cx, vec![]);
    connect_fake_as(&ws, cx, "flaky", None, false);
    cx.simulate_input("SELECT * FROM orders o WHERE o.");
    wait_for_menu(&ws, cx, "id");
    let catalog = query_tab(&ws, cx).read_with(cx, |q, _| q.catalog_for_test().unwrap());
    let task = catalog.read_with(cx, |c, cx| c.drop_session(cx));
    cx.background_executor.block_test(task);
    honcho_db::fake::REFUSE_FLAKY.store(true, std::sync::atomic::Ordering::SeqCst);

    cx.simulate_input(";\nSELECT * FROM users u WHERE u.");
    let note = |cx: &mut VisualTestContext| {
        ws.read_with(cx, |ws, cx| {
            ws.active_editor()
                .read(cx)
                .completion
                .as_ref()
                .and_then(|m| m.note.clone())
        })
    };
    wait_until(cx, |cx| note(cx).is_some());
    honcho_db::fake::REFUSE_FLAKY.store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(
        note(cx)
            .unwrap()
            .starts_with("Connection lost and reconnecting failed"),
        "{:?}",
        note(cx)
    );
}

#[gpui::test]
fn browser_edge_drags_to_resize(cx: &mut TestAppContext) {
    use gpui::{point, px, Modifiers, MouseButton};
    let (ws, cx) = open(cx, vec![]);
    let b = ws.read_with(cx, |ws, _| ws.browser().clone());
    let width = |cx: &mut VisualTestContext| b.read_with(cx, |b, _| b.width);
    assert_eq!(width(cx), crate::browser::DEFAULT_WIDTH);

    // the edge sits on the pane's right border
    let y = px(300.);
    cx.simulate_mouse_down(point(px(260.), y), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(point(px(330.), y), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(point(px(360.), y), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(point(px(360.), y), MouseButton::Left, Modifiers::none());
    assert_eq!(width(cx), px(360.));
    cx.run_until_parked();

    // remembered for next time
    let fresh = cx.update(|_, cx| {
        cx.new(|cx| {
            let mut b = crate::browser::ObjectBrowser::new(cx);
            b.restore_expanded(cx);
            b
        })
    });
    assert_eq!(fresh.read_with(cx, |b, _| b.width), px(360.));

    // too narrow is held at a minimum
    cx.simulate_mouse_down(point(px(360.), y), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(point(px(20.), y), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(point(px(20.), y), MouseButton::Left, Modifiers::none());
    assert_eq!(width(cx), px(160.));

    // a double click on the edge puts it back
    cx.simulate_click(point(px(160.), y), Modifiers::none());
    cx.simulate_event(gpui::MouseDownEvent {
        position: point(px(160.), y),
        button: MouseButton::Left,
        modifiers: Modifiers::none(),
        click_count: 2,
        first_mouse: false,
    });
    cx.simulate_event(gpui::MouseUpEvent {
        position: point(px(160.), y),
        button: MouseButton::Left,
        modifiers: Modifiers::none(),
        click_count: 2,
    });
    assert_eq!(width(cx), crate::browser::DEFAULT_WIDTH);
}

fn open_routine(ws: &Entity<Workspace>, cx: &mut VisualTestContext, name: &str, group: &str) {
    connect_fake(ws, cx, None);
    browse(ws, cx, "fake db");
    browse(ws, cx, "app");
    browse(ws, cx, group);
    let b = ws.read_with(cx, |ws, _| ws.browser().clone());
    b.update(cx, |b, cx| b.act(name, "definition", cx));
    wait_until(cx, |cx| {
        ws.read_with(cx, |ws, cx| {
            ws.tab_count() == 2 && ws.active_connection(cx).is_some()
        })
    });
}

#[gpui::test]
fn routines_open_as_scripts_that_replace_them(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    open_routine(&ws, cx, "refresh_stats", "Procedures (1)");
    let t = text(&ws, cx);
    assert!(
        t.starts_with(
            "DROP PROCEDURE IF EXISTS `app`.`refresh_stats`;\nDELIMITER //\nCREATE PROCEDURE"
        ),
        "{t}"
    );
    let q = query_tab(&ws, cx);
    assert!(q.read_with(cx, |q, _| q.origin.is_some()));

    // the fake can't run this CREATE, which stands in for a script that
    // would fail: Apply stops before dropping anything
    q.update_in(cx, |q, window, cx| q.apply_for_test(window, cx));
    wait_until(cx, |cx| {
        q.read_with(cx, |q, _| {
            q.output_text()
                .iter()
                .any(|m| m.contains("so nothing was dropped"))
        })
    });
    assert!(!q.read_with(cx, |q, _| q.is_running()));
}

#[gpui::test]
fn apply_warns_when_the_server_copy_changed(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    open_routine(&ws, cx, "refresh_stats", "Procedures (1)");
    let q = query_tab(&ws, cx);
    // as if someone changed it after this tab opened it
    q.update(cx, |q, _| {
        q.origin.as_mut().unwrap().definition = "CREATE PROCEDURE refresh_stats() SELECT 2".into()
    });
    q.update_in(cx, |q, window, cx| q.apply_for_test(window, cx));
    wait_until(cx, |cx| cx.has_pending_prompt());
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert_eq!(
        q.read_with(cx, |q, _| q.output_text()),
        Vec::<String>::new()
    );
}

#[gpui::test]
fn run_form_builds_the_call(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    browse(&ws, cx, "fake db");
    browse(&ws, cx, "app");
    browse(&ws, cx, "Functions (1)");
    let b = ws.read_with(cx, |ws, _| ws.browser().clone());
    b.update(cx, |b, cx| b.act("order_total", "run", cx));
    wait_until(cx, |cx| {
        ws.read_with(cx, |ws, _| ws.routine_form().is_some())
    });
    let form = ws.read_with(cx, |ws, _| ws.routine_form().unwrap().clone());
    form.update(cx, |f, cx| f.set_value(0, "7", cx));
    assert_eq!(
        form.read_with(cx, |f, cx| f.sql(cx)),
        "SELECT `app`.`order_total`(7) AS `order_total`;\n"
    );
    press(cx, "enter");
    wait_until(cx, |cx| ws.read_with(cx, |ws, _| ws.tab_count() == 2));
    assert_eq!(
        text(&ws, cx),
        "SELECT `app`.`order_total`(7) AS `order_total`;\n"
    );
    assert!(ws.read_with(cx, |ws, _| ws.routine_form().is_none()));
}

#[gpui::test]
fn filter_searches_routine_bodies(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    browse(&ws, cx, "fake db");
    let b = ws.read_with(cx, |ws, _| ws.browser().clone());
    b.update(cx, |b, cx| b.set_filter("body:orders", cx));
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(300));
    wait_until(cx, |cx| {
        b.read_with(cx, |b, _| {
            let l = b.labels();
            l.iter().any(|x| x.trim() == "refresh_stats")
                && l.iter().any(|x| x.trim() == "order_total")
        })
    });
}

#[gpui::test]
fn browser_actions_reconnect_first(cx: &mut TestAppContext) {
    let _flaky = FLAKY.lock().unwrap_or_else(|e| e.into_inner());
    let (ws, cx) = open(cx, vec![]);
    connect_fake_as(&ws, cx, "flaky", None, false);
    browse(&ws, cx, "fake db");
    browse(&ws, cx, "app");
    browse(&ws, cx, "Procedures (1)");
    let b = ws.read_with(cx, |ws, _| ws.browser().clone());
    let tabs = |cx: &mut VisualTestContext| ws.read_with(cx, |ws, _| ws.tab_count());
    let open_def = |cx: &mut VisualTestContext| {
        let before = ws.read_with(cx, |ws, _| ws.tab_count());
        b.update(cx, |b, cx| b.act("refresh_stats", "definition", cx));
        wait_until(cx, |cx| ws.read_with(cx, |ws, _| ws.tab_count() > before));
    };

    // the session dropped while idle: opening reconnects and carries on
    let task = b.read_with(cx, |b, cx| b.drop_session(cx));
    cx.background_executor.block_test(task);
    open_def(cx);
    assert_eq!(tabs(cx), 2);
    assert!(
        text(&ws, cx).starts_with("DROP PROCEDURE"),
        "{}",
        text(&ws, cx)
    );

    // the server is unreachable: say so instead of doing nothing
    let task = b.read_with(cx, |b, cx| b.drop_session(cx));
    cx.background_executor.block_test(task);
    honcho_db::fake::REFUSE_FLAKY.store(true, std::sync::atomic::Ordering::SeqCst);
    open_def(cx);
    honcho_db::fake::REFUSE_FLAKY.store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(tabs(cx), 3);
    assert!(
        text(&ws, cx).starts_with("-- couldn't connect"),
        "{}",
        text(&ws, cx)
    );

    // and once it's back, the next try reconnects from the failed state
    open_def(cx);
    assert_eq!(tabs(cx), 4);
    assert!(
        text(&ws, cx).starts_with("DROP PROCEDURE"),
        "{}",
        text(&ws, cx)
    );
}

#[gpui::test]
fn object_tabs_run_only_through_apply(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    open_routine(&ws, cx, "refresh_stats", "Procedures (1)");
    let q = query_tab(&ws, cx);
    // the cursor is on the DROP line: running it alone would lose the procedure
    for keys in ["P-enter", "P-shift-enter"] {
        press(cx, keys);
        cx.run_until_parked();
        assert!(!q.read_with(cx, |q, _| q.is_running()));
    }
    let out = q.read_with(cx, |q, _| q.output_text());
    assert!(out.iter().all(|m| m.contains("use Apply")), "{out:?}");
    assert!(!out.is_empty());
}

fn open_project(
    ws: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    connection: &str,
) -> (tempfile::TempDir, Entity<crate::project_pane::ProjectPane>) {
    use honcho_db::project::{Manifest, Project};
    let dir = tempfile::tempdir().unwrap();
    let project = Project::init(
        dir.path(),
        Manifest {
            connection: connection.into(),
            database: "app".into(),
        },
    )
    .unwrap();
    ws.update_in(cx, |ws, window, cx| {
        ws.show_project_for_test(project, window, cx)
    });
    let pane = ws.read_with(cx, |ws, _| ws.project().unwrap().clone());
    wait_until(cx, |cx| pane.read_with(cx, |p, _| !p.is_busy()));
    (dir, pane)
}

fn statuses(
    pane: &Entity<crate::project_pane::ProjectPane>,
    cx: &mut VisualTestContext,
) -> Vec<(String, honcho_db::project::Status)> {
    pane.read_with(cx, |p, _| {
        p.entries
            .iter()
            .map(|e| (e.object.name.clone(), e.status))
            .collect()
    })
}

#[gpui::test]
fn project_pulls_compares_and_opens_files(cx: &mut TestAppContext) {
    use honcho_db::project::Status::*;
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    let (dir, pane) = open_project(&ws, cx, "fake db");
    // in apply order: functions, views, procedures
    assert_eq!(
        statuses(&pane, cx),
        [
            ("order_total".to_string(), ServerOnly),
            ("active_users".to_string(), ServerOnly),
            ("refresh_stats".to_string(), ServerOnly),
        ]
    );

    pane.update_in(cx, |p, window, cx| p.pull_changes(window, cx));
    wait_until(cx, |cx| {
        pane.read_with(cx, |p, _| {
            !p.is_busy() && p.entries.iter().all(|e| e.status == Same)
        })
    });
    assert_eq!(
        pane.read_with(cx, |p, _| p.outcome().map(str::to_string))
            .as_deref(),
        Some("Pulled 3 objects.")
    );
    let file = dir.path().join("procedures/refresh_stats.sql");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(
        text.starts_with("DROP PROCEDURE IF EXISTS `refresh_stats`;"),
        "{text}"
    );

    // edited here: the diff shows the change on the folder side
    std::fs::write(&file, text.replace("COUNT(*)", "COUNT(id)")).unwrap();
    pane.update(cx, |p, cx| p.refresh(cx));
    wait_until(cx, |cx| {
        statuses(&pane, cx).contains(&("refresh_stats".to_string(), Edited))
    });
    pane.update(cx, |_, cx| {
        cx.emit(crate::project_pane::ProjectEvent::Compare(2))
    });
    let diff = ws.read_with(cx, |ws, _| ws.diff().unwrap().clone());
    let changed: Vec<_> = diff.read_with(cx, |d, _| {
        d.lines
            .iter()
            .filter(|l| l.left != l.right)
            .map(|l| (l.left.clone().unwrap().1, l.right.clone().unwrap().1))
            .collect()
    });
    assert_eq!(
        changed,
        [(
            "  SELECT COUNT(id) FROM orders;".to_string(),
            "  SELECT COUNT(*) FROM orders;".to_string()
        )]
    );
    press(cx, "escape");
    assert!(ws.read_with(cx, |ws, _| ws.diff().is_none()));

    // opening the file connects the tab to the project's database, and
    // its run shortcuts go through Apply
    pane.update(cx, |p, cx| p.open(2, cx));
    wait_until(cx, |cx| {
        ws.read_with(cx, |ws, cx| ws.active_connection(cx).is_some())
    });
    assert!(text_of_tab_is(&ws, cx, "COUNT(id)"));
    let q = query_tab(&ws, cx);
    assert!(q.read_with(cx, |q, _| q.origin.is_some()));
    assert_eq!(
        ws.read_with(cx, |ws, cx| ws.active_editor().read(cx).path.clone()),
        Some(file.canonicalize().unwrap())
    );

    // the fake can't create anything, which stands in for a file that
    // won't apply: it's reported and the status stays
    pane.update(cx, |p, cx| p.apply(vec![2], cx));
    wait_until(cx, |cx| {
        pane.read_with(cx, |p, _| {
            !p.is_busy() && p.outcome().is_some_and(|o| o.contains("failed"))
        })
    });
    assert!(statuses(&pane, cx).contains(&("refresh_stats".to_string(), Edited)));
}

fn text_of_tab_is(ws: &Entity<Workspace>, cx: &mut VisualTestContext, needle: &str) -> bool {
    text(ws, cx).contains(needle)
}

#[gpui::test]
fn project_names_a_missing_connection(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    let (_dir, pane) = open_project(&ws, cx, "staging");
    let failure = pane.read_with(cx, |p, _| p.failure().map(str::to_string));
    assert!(
        failure
            .as_deref()
            .is_some_and(|f| f.contains("no saved connection named \"staging\"")),
        "{failure:?}"
    );
}

#[test]
fn save_to_project_works_from_the_connection_and_schema_rows() {
    use crate::browser::{project_database, Item};
    let mut mysql = honcho_config::SavedConnection::new("prod", DriverKind::MySql);
    let at = |item: Item, saved: &honcho_config::SavedConnection| project_database(&item, saved);
    assert_eq!(at(Item::Connection(Box::new(mysql.clone())), &mysql), None);
    mysql.database = Some("cems".into());
    assert_eq!(
        at(Item::Connection(Box::new(mysql.clone())), &mysql).as_deref(),
        Some("cems")
    );
    assert_eq!(
        at(Item::Database("app".into()), &mysql).as_deref(),
        Some("app")
    );
    let pg = honcho_config::SavedConnection::new("pg", DriverKind::Postgres);
    assert_eq!(
        at(Item::Schema("shop".into(), "public".into()), &pg).as_deref(),
        Some("shop")
    );
    let lite = honcho_config::SavedConnection::new("shop.db", DriverKind::Sqlite);
    assert_eq!(
        at(Item::Connection(Box::new(lite.clone())), &lite).as_deref(),
        Some("main")
    );
}

fn palette(
    ws: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<Entity<crate::palette::CommandPalette>> {
    ws.read_with(cx, |ws, _| ws.palette())
}

#[gpui::test]
fn the_palette_runs_commands_and_remembers_them(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    press(cx, "P-shift-p");
    let p = palette(&ws, cx).expect("palette opens");
    let all = p.read_with(cx, |p, _| p.shown_labels());
    assert!(
        all.iter().any(|l| l == "File: Open Project Folder"),
        "{all:?}"
    );

    cx.simulate_input("newfil");
    let shown = p.read_with(cx, |p, _| p.shown_labels());
    assert_eq!(shown.first().map(String::as_str), Some("File: New File"));
    press(cx, "enter");
    assert!(palette(&ws, cx).is_none(), "running a command closes it");
    assert_eq!(ws.read_with(cx, |ws, _| ws.tab_count()), 2);

    // focus went back to the editor, so typing lands there
    cx.simulate_input("select 1");
    assert_eq!(text(&ws, cx), "select 1");

    // the last command leads next time, and escape just closes
    press(cx, "P-shift-p");
    let p = palette(&ws, cx).unwrap();
    let shown = p.read_with(cx, |p, _| p.shown_labels());
    assert_eq!(shown.first().map(String::as_str), Some("File: New File"));
    press(cx, "down");
    press(cx, "escape");
    assert!(palette(&ws, cx).is_none());
    assert_eq!(ws.read_with(cx, |ws, _| ws.tab_count()), 2);
    cx.simulate_input("!");
    assert_eq!(text(&ws, cx), "select 1!");
}

#[gpui::test]
fn the_palette_runs_editor_commands_on_the_editor(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    cx.simulate_input("select 1");
    press(cx, "f1");
    cx.simulate_input("toggle line comment");
    press(cx, "enter");
    assert_eq!(text(&ws, cx), "-- select 1");
}

fn shown_plan(
    ws: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<Entity<crate::plan_view::PlanView>> {
    let q = query_tab(ws, cx);
    q.read_with(cx, |q, _| {
        let o = q.output.as_ref()?;
        (o.shown == o.sets.len()).then(|| o.plan.clone())?
    })
}

#[gpui::test]
fn plan_details_drag_wider_up_to_half(cx: &mut TestAppContext) {
    use crate::plan_view::DETAILS_MIN;
    use gpui::{point, px, Modifiers, MouseButton};
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    cx.simulate_input("select 3");
    run(&ws, cx, "P-e");
    let plan = shown_plan(&ws, cx).expect("the plan tab shows");
    let width = |cx: &mut VisualTestContext| plan.read_with(cx, |p, _| p.details_width());
    assert_eq!(width(cx), DETAILS_MIN);

    // from the edge's centre, by dx
    let drag = |cx: &mut VisualTestContext, dx: gpui::Pixels| {
        let edge = cx
            .debug_bounds("plan-details-edge")
            .expect("the edge is drawn");
        let (x, y) = (edge.center().x, edge.center().y);
        cx.simulate_mouse_down(point(x, y), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(point(x + dx, y), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(point(x + dx, y), MouseButton::Left, Modifiers::none());
    };
    drag(cx, px(-50.));
    assert_eq!(width(cx), DETAILS_MIN + px(50.));
    // out past the view on the left stops at half of it
    let half = plan.read_with(cx, |p, _| p.width()) / 2.;
    drag(cx, px(-3000.));
    assert_eq!(width(cx), half);
    drag(cx, px(3000.));
    assert_eq!(width(cx), DETAILS_MIN);

    // the next explain keeps the width
    drag(cx, px(-40.));
    let kept = width(cx);
    run(&ws, cx, "P-e");
    let again = shown_plan(&ws, cx).unwrap();
    assert_ne!(again.entity_id(), plan.entity_id());
    assert_eq!(again.read_with(cx, |p, _| p.details_width()), kept);
}

#[gpui::test]
fn explain_shows_the_plan_and_asks_before_analyzing_a_write(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    cx.simulate_input("select 3");
    run(&ws, cx, "P-e");
    let plan = shown_plan(&ws, cx).expect("the plan tab shows");
    let ops = |cx: &mut VisualTestContext| plan.read_with(cx, |p, _| p.shown_operations());
    assert_eq!(ops(cx), ["Hash Join", "  Seq Scan", "  Index Scan"]);
    assert!(!plan.read_with(cx, |p, _| p.plan.analyzed));
    plan.update(cx, |p, cx| p.toggle(vec![], cx));
    assert_eq!(ops(cx), ["Hash Join"]);

    press(cx, "P-a");
    cx.simulate_input("update 3");
    press(cx, "P-shift-e");
    wait_until(cx, |cx| cx.has_pending_prompt());
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert!(!query_tab(&ws, cx).read_with(cx, |q, _| q.is_running()));
    press(cx, "P-shift-e");
    wait_until(cx, |cx| cx.has_pending_prompt());
    cx.simulate_prompt_answer("Run and Roll Back");
    wait_until(cx, |cx| {
        shown_plan(&ws, cx).is_some_and(|p| p.read_with(cx, |p, _| p.plan.analyzed))
    });
    // a plain run afterwards has no plan tab
    run(&ws, cx, "P-enter");
    assert!(shown_plan(&ws, cx).is_none());
}

#[gpui::test]
fn the_explain_button_and_its_menu(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    let click = |cx: &mut VisualTestContext, sel: &'static str| {
        let at = cx
            .debug_bounds(sel)
            .unwrap_or_else(|| panic!("{sel} shows"))
            .center();
        cx.simulate_click(at, Default::default());
        cx.run_until_parked();
    };
    // with no connection it asks for one, as Run does
    click(cx, "tab-explain");
    assert!(dialog(&ws, cx).is_some(), "the connections dialog opens");
    press(cx, "escape");
    assert!(dialog(&ws, cx).is_none());

    connect_fake(&ws, cx, None);
    cx.simulate_input("select 3");
    click(cx, "tab-explain");
    wait_until(cx, |cx| shown_plan(&ws, cx).is_some());

    press(cx, "P-a");
    cx.simulate_input("update 3");
    assert!(text(&ws, cx).contains("update 3"), "the editor keeps focus");
    click(cx, "tab-explain-menu");
    click(cx, "explain-analyze");
    wait_until(cx, |cx| cx.has_pending_prompt());
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("explain-analyze").is_none(),
        "the menu closes"
    );
}

#[gpui::test]
fn sqlite_explains_but_has_no_analyze(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = sqlite_file(dir.path());
    let (ws, cx) = open(cx, vec![path]);
    let q = query_tab(&ws, cx);
    wait_until(cx, |cx| q.read_with(cx, |q, _| q.connection.is_some()));
    cx.simulate_input("select name from items where id = 1");
    run(&ws, cx, "P-e");
    let plan = shown_plan(&ws, cx).expect("the plan tab shows");
    assert_eq!(
        plan.read_with(cx, |p, _| p.shown_operations()),
        ["SEARCH USING INTEGER PRIMARY KEY"]
    );

    // refused up front, without asking first
    press(cx, "P-a");
    cx.simulate_input("delete from items");
    let messages = run(&ws, cx, "P-shift-e");
    assert!(!cx.has_pending_prompt());
    assert!(
        messages
            .iter()
            .any(|m| m == "SQLite has no Explain Analyze. Explain shows the plan."),
        "{messages:?}"
    );
}
