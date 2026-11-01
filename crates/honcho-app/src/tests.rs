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
    for _ in 0..300 {
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
    press(cx, "P-shift-c");
    let d = dialog(ws, cx).unwrap();
    d.update(cx, |d, _| {
        d.set_driver(DriverKind::Fake);
        d.set_color(color);
        d.set_auto_reconnect(auto_reconnect);
    });
    fill(cx, ["fake db", "", "", "dev", "", "app"]);
    press(cx, "enter");
    wait_until(cx, |cx| dialog(ws, cx).is_none());
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
    b.update(cx, |b, cx| b.click(label, false, cx));
    wait_until(cx, |cx| b.read_with(cx, |b, _| b.is_idle()));
}

#[gpui::test]
fn object_browser_loads_as_nodes_open(cx: &mut TestAppContext) {
    let (ws, cx) = open(cx, vec![]);
    connect_fake(&ws, cx, None);
    assert_eq!(browser_labels(&ws, cx), ["fake db"]);

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
    assert_eq!(browser_labels(&ws, cx), ["fake db", "  app", "  analytics"]);
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
