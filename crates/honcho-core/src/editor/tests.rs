use super::*;

/// Build an editor from text where `|` marks cursors and `[...]` marks selections.
fn ed(marked: &str) -> Editor {
    let mut text = String::new();
    let mut sels = Vec::new();
    let mut open = None;
    let mut n = 0;
    for c in marked.chars() {
        match c {
            '|' => sels.push(Selection::cursor(n)),
            '[' => open = Some(n),
            ']' => sels.push(Selection::new(open.take().unwrap(), n)),
            _ => {
                text.push(c);
                n += 1;
            }
        }
    }
    let mut e = Editor::new(&text);
    if !sels.is_empty() {
        let last = sels.len() - 1;
        e.set_selections(sels, last);
    }
    e
}

/// Render back to the marked form.
fn show(e: &Editor) -> String {
    let text: Vec<char> = e.text().chars().collect();
    let mut out = String::new();
    for i in 0..=text.len() {
        for s in e.selections() {
            if s.is_empty() && s.head == i {
                out.push('|');
            } else if !s.is_empty() && s.end() == i {
                out.push(']');
            }
        }
        for s in e.selections() {
            if !s.is_empty() && s.start() == i {
                out.push('[');
            }
        }
        if i < text.len() {
            out.push(text[i]);
        }
    }
    out
}

#[test]
fn types_at_every_cursor() {
    let mut e = ed("a|\nb|\nc|");
    e.type_text("x");
    assert_eq!(show(&e), "ax|\nbx|\ncx|");
    e.backspace();
    e.backspace();
    assert_eq!(show(&e), "|\n|\n|");
}

#[test]
fn undo_groups_typing() {
    let mut e = ed("|");
    for c in "select".chars() {
        e.type_text(&c.to_string());
    }
    assert_eq!(e.text(), "select");
    e.undo();
    assert_eq!(e.text(), "");
    e.redo();
    assert_eq!(show(&e), "select|");
}

#[test]
fn add_cursor_below_keeps_column() {
    let mut e = ed("ab|cd\nx\nabcd");
    e.add_cursor_vertical(true);
    e.add_cursor_vertical(true);
    assert_eq!(show(&e), "ab|cd\nx|\nab|cd");
    e.type_text("-");
    assert_eq!(show(&e), "ab-|cd\nx-|\nab-|cd");
}

#[test]
fn cmd_d_selects_word_then_next_matches() {
    let mut e = ed("select id|, name from t where id = 1 and xid = 2 and ID = 3");
    e.select_next_occurrence();
    assert_eq!(e.selections().len(), 1);
    assert_eq!(e.slice(e.selections()[0].range()), "id");
    e.select_next_occurrence();
    e.select_next_occurrence();
    // whole-word: xid is skipped, ID matches case-insensitively
    let picked: Vec<String> = e.selections().iter().map(|s| e.slice(s.range())).collect();
    assert_eq!(picked, vec!["id", "id", "ID"]);
    e.type_text("user_id");
    assert_eq!(
        e.text(),
        "select user_id, name from t where user_id = 1 and xid = 2 and user_id = 3"
    );
}

#[test]
fn select_all_occurrences() {
    let mut e = ed("[foo] bar foo\nfoo");
    e.select_all_occurrences();
    assert_eq!(show(&e), "[foo] bar [foo]\n[foo]");
}

#[test]
fn column_select_skips_short_lines() {
    let mut e = ed("abcdef\nab\nabcdef");
    e.column_select((0, 3), (2, 5));
    assert_eq!(show(&e), "abc[de]f\nab\nabc[de]f");
    e.column_select((0, 2), (2, 2));
    assert_eq!(show(&e), "ab|cdef\nab|\nab|cdef");
}

#[test]
fn split_selection_into_lines() {
    let mut e = ed("[one\ntwo\nthree]");
    e.split_selection_into_lines();
    assert_eq!(show(&e), "[one]\n[two]\n[three]");
}

#[test]
fn auto_pairs() {
    let mut e = ed("select |");
    e.type_text("(");
    assert_eq!(show(&e), "select (|)");
    e.type_text("'");
    assert_eq!(show(&e), "select ('|')");
    e.type_text("a");
    e.type_text("'");
    e.type_text(")");
    assert_eq!(show(&e), "select ('a')|");

    let mut e = ed("it|");
    e.type_text("'");
    assert_eq!(show(&e), "it'|");

    let mut e = ed("(|)");
    e.backspace();
    assert_eq!(show(&e), "|");
}

#[test]
fn wrap_selection_in_quotes() {
    let mut e = ed("[abc] [def]");
    e.type_text("'");
    assert_eq!(show(&e), "'[abc]' '[def]'");
}

#[test]
fn newline_keeps_indent_and_splits_parens() {
    let mut e = ed("    select|");
    e.newline();
    assert_eq!(show(&e), "    select\n    |");
    let mut e = ed("count(|)");
    e.newline();
    assert_eq!(show(&e), "count(\n    |\n)");
}

#[test]
fn backspace_removes_indent_unit() {
    let mut e = ed("        |x");
    e.backspace();
    assert_eq!(show(&e), "    |x");
}

#[test]
fn toggle_comment() {
    let mut e = ed("[select 1\n  from t]");
    e.toggle_comment();
    assert_eq!(e.text(), "-- select 1\n--   from t");
    e.toggle_comment();
    assert_eq!(e.text(), "select 1\n  from t");
}

#[test]
fn move_and_duplicate_lines() {
    let mut e = ed("a\nb|\nc");
    e.move_lines(false);
    assert_eq!(show(&e), "b|\na\nc");
    e.move_lines(true);
    e.move_lines(true);
    assert_eq!(show(&e), "a\nc\nb|");

    let mut e = ed("a|\nb");
    e.duplicate_lines(true);
    assert_eq!(show(&e), "a\na|\nb");
    let mut e = ed("a|\nb");
    e.duplicate_lines(false);
    assert_eq!(show(&e), "a|\na\nb");
}

#[test]
fn delete_lines() {
    let mut e = ed("one\nt|wo\nthree");
    e.delete_lines();
    assert_eq!(show(&e), "one\nt|hree");
    let mut e = ed("one\ntw|o");
    e.delete_lines();
    assert_eq!(show(&e), "on|e");
}

#[test]
fn multi_cursor_copy_paste_distributes() {
    let e = ed("[a]=1\n[b]=2");
    let (text, lines) = e.copy_text();
    assert_eq!((text.as_str(), lines), ("a\nb", false));
    let mut f = ed("x|\ny|");
    f.paste(&text, false);
    assert_eq!(show(&f), "xa|\nyb|");
}

#[test]
fn copy_without_selection_copies_line() {
    let e = ed("one\ntw|o");
    assert_eq!(e.copy_text(), ("two\n".to_string(), true));
    let mut f = ed("x|y");
    f.paste("two\n", true);
    assert_eq!(show(&f), "two\nx|y");
}

#[test]
fn word_motion() {
    let mut e = ed("|select foo_bar, baz");
    e.move_cursors(Motion::WordRight, false);
    assert_eq!(show(&e), "select| foo_bar, baz");
    e.move_cursors(Motion::WordRight, false);
    assert_eq!(show(&e), "select foo_bar|, baz");
    e.move_cursors(Motion::WordLeft, true);
    assert_eq!(show(&e), "select [foo_bar], baz");
}

#[test]
fn thai_grapheme_motion() {
    // "ที่" is one grapheme: base + two combining marks
    let mut e = ed("|ที่นี่");
    e.move_cursors(Motion::Right, false);
    assert_eq!(show(&e), "ที่|นี่");
}

#[test]
fn smart_home() {
    let mut e = ed("    sel|ect");
    e.move_cursors(Motion::LineStart, false);
    assert_eq!(show(&e), "    |select");
    e.move_cursors(Motion::LineStart, false);
    assert_eq!(show(&e), "|    select");
}

#[test]
fn vertical_motion_remembers_goal() {
    let mut e = ed("abcdef|\nab\nabcdefgh");
    e.move_cursors(Motion::Down, false);
    assert_eq!(show(&e), "abcdef\nab|\nabcdefgh");
    e.move_cursors(Motion::Down, false);
    assert_eq!(show(&e), "abcdef\nab\nabcdef|gh");
}

#[test]
fn tab_indents_multiline_selection() {
    let mut e = ed("[a\nb]");
    e.tab();
    assert_eq!(e.text(), "    a\n    b");
    e.outdent();
    assert_eq!(e.text(), "a\nb");
}

#[test]
fn escape_collapses_to_newest() {
    let mut e = ed("a|\nb|");
    assert!(e.collapse_selections());
    assert_eq!(e.selections().len(), 1);
    assert!(!e.collapse_selections());
}

#[test]
fn typing_after_stepping_over_a_closer_can_be_undone() {
    // found by the property tests
    let mut e = ed("\n|`");
    e.type_text("`");
    e.type_text("a");
    assert_eq!(e.text(), "\n`a");
    assert!(e.undo());
    assert_eq!(e.text(), "\n`");

    // a pair typed in one go still undoes as one step
    let mut e = ed("x|");
    e.type_text(" ");
    e.type_text("(");
    e.type_text(")");
    e.type_text("y");
    assert_eq!(e.text(), "x ()y");
    assert!(e.undo());
    assert_eq!(e.text(), "x");
}

#[test]
fn undo_back_to_a_state_restores_its_revision() {
    let mut e = ed("a|");
    let saved = e.revision();
    e.type_text("b");
    e.newline();
    e.type_text("c");
    assert_ne!(e.revision(), saved);
    while e.undo() {}
    assert_eq!(e.revision(), saved);
    e.redo();
    let after_first = e.revision();
    assert_ne!(after_first, saved);
    // a new edit after undoing never reuses an old revision
    e.undo();
    e.type_text("x");
    assert_ne!(e.revision(), after_first);
    assert_ne!(e.revision(), saved);
}
