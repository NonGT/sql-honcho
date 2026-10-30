//! Property tests: random operation sequences must keep the editor's
//! invariants, and the lexer / splitter / search must hold up on any input.

use honcho_core::search::{find_all, SearchQuery};
use honcho_core::sql::{lex_line, statements, LineState};
use honcho_core::{Editor, Motion, Selection};
use proptest::prelude::*;
use ropey::Rope;

/// Characters that tend to break things: combining Thai marks, wide and
/// astral chars, tabs, newlines, brackets and quotes.
fn tricky_char() -> impl Strategy<Value = char> {
    prop::sample::select(vec![
        'a', 'b', 'Z', '_', ' ', ' ', '\t', '\n', '\n', '(', ')', '[', ']', '\'', '"', '`', ';',
        '-', '/', '*', '$', 'ท', 'ี', '่', 'ก', '中', '😀', 'é',
    ])
}

fn tricky_text(max: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(tricky_char(), 0..max).prop_map(|v| v.into_iter().collect())
}

#[derive(Clone, Debug)]
enum Op {
    Type(String),
    Insert(String),
    Paste(String, bool),
    Backspace,
    Delete,
    DeleteWordLeft,
    DeleteWordRight,
    DeleteToLineStart,
    Newline,
    Tab,
    Outdent,
    Move(Motion, bool),
    SetCursor(f64),
    SelectRange(f64, f64),
    ToggleCursor(f64),
    ColumnSelect((usize, usize), (usize, usize)),
    AddCursor(bool),
    SelectNext,
    SelectAllOccurrences,
    SplitLines,
    SelectAll,
    SelectLine,
    Collapse,
    ToggleComment,
    MoveLines(bool),
    DuplicateLines(bool),
    DeleteLines,
    InsertLine(bool),
    Cut,
    Undo,
    Redo,
}

fn motion() -> impl Strategy<Value = Motion> {
    prop_oneof![
        Just(Motion::Left),
        Just(Motion::Right),
        Just(Motion::Up),
        Just(Motion::Down),
        Just(Motion::WordLeft),
        Just(Motion::WordRight),
        Just(Motion::LineStart),
        Just(Motion::LineEnd),
        Just(Motion::DocStart),
        Just(Motion::DocEnd),
        (1usize..5).prop_map(Motion::PageUp),
        (1usize..5).prop_map(Motion::PageDown),
    ]
}

fn op(with_history: bool) -> BoxedStrategy<Op> {
    let frac = 0.0..=1.0f64;
    let point = (0usize..8, 0usize..12);
    let edits = prop_oneof![
        4 => tricky_char().prop_map(|c| Op::Type(c.to_string())),
        1 => tricky_text(12).prop_map(Op::Insert),
        1 => (tricky_text(12), any::<bool>()).prop_map(|(t, l)| Op::Paste(t, l)),
        2 => Just(Op::Backspace),
        1 => Just(Op::Delete),
        1 => Just(Op::DeleteWordLeft),
        1 => Just(Op::DeleteWordRight),
        1 => Just(Op::DeleteToLineStart),
        1 => Just(Op::Newline),
        1 => Just(Op::Tab),
        1 => Just(Op::Outdent),
        3 => (motion(), any::<bool>()).prop_map(|(m, e)| Op::Move(m, e)),
        1 => frac.clone().prop_map(Op::SetCursor),
        1 => (frac.clone(), frac.clone()).prop_map(|(a, b)| Op::SelectRange(a, b)),
        1 => frac.prop_map(Op::ToggleCursor),
        1 => (point.clone(), point).prop_map(|(a, b)| Op::ColumnSelect(a, b)),
        1 => any::<bool>().prop_map(Op::AddCursor),
        1 => Just(Op::SelectNext),
        1 => Just(Op::SelectAllOccurrences),
        1 => Just(Op::SplitLines),
        1 => Just(Op::SelectAll),
        1 => Just(Op::SelectLine),
        1 => Just(Op::Collapse),
        1 => Just(Op::ToggleComment),
        1 => any::<bool>().prop_map(Op::MoveLines),
        1 => any::<bool>().prop_map(Op::DuplicateLines),
        1 => Just(Op::DeleteLines),
        1 => any::<bool>().prop_map(Op::InsertLine),
        1 => Just(Op::Cut),
    ];
    if with_history {
        prop_oneof![8 => edits, 1 => Just(Op::Undo), 1 => Just(Op::Redo)].boxed()
    } else {
        edits.boxed()
    }
}

fn at(e: &Editor, f: f64) -> usize {
    (e.len_chars() as f64 * f).round() as usize
}

fn run(e: &mut Editor, op: &Op) {
    match op.clone() {
        Op::Type(t) => e.type_text(&t),
        Op::Insert(t) => e.insert(&t),
        Op::Paste(t, lines) => e.paste(&t, lines),
        Op::Backspace => e.backspace(),
        Op::Delete => e.delete_forward(),
        Op::DeleteWordLeft => e.delete_word_left(),
        Op::DeleteWordRight => e.delete_word_right(),
        Op::DeleteToLineStart => e.delete_to_line_start(),
        Op::Newline => e.newline(),
        Op::Tab => e.tab(),
        Op::Outdent => e.outdent(),
        Op::Move(m, extend) => e.move_cursors(m, extend),
        Op::SetCursor(f) => e.set_cursor(at(e, f)),
        Op::SelectRange(a, b) => {
            let s = Selection::new(at(e, a), at(e, b));
            e.set_selections(vec![s], 0);
        }
        Op::ToggleCursor(f) => e.toggle_cursor(at(e, f)),
        Op::ColumnSelect(a, b) => e.column_select(a, b),
        Op::AddCursor(down) => e.add_cursor_vertical(down),
        Op::SelectNext => e.select_next_occurrence(),
        Op::SelectAllOccurrences => e.select_all_occurrences(),
        Op::SplitLines => e.split_selection_into_lines(),
        Op::SelectAll => e.select_all(),
        Op::SelectLine => e.select_line(),
        Op::Collapse => {
            e.collapse_selections();
        }
        Op::ToggleComment => e.toggle_comment(),
        Op::MoveLines(down) => e.move_lines(down),
        Op::DuplicateLines(down) => e.duplicate_lines(down),
        Op::DeleteLines => e.delete_lines(),
        Op::InsertLine(below) => e.insert_line(below),
        Op::Cut => {
            e.cut();
        }
        Op::Undo => {
            e.undo();
        }
        Op::Redo => {
            e.redo();
        }
    }
}

fn check_invariants(e: &Editor) -> Result<(), TestCaseError> {
    let len = e.len_chars();
    let sels = e.selections();
    prop_assert!(!sels.is_empty(), "no selections");
    for s in sels {
        prop_assert!(
            s.anchor <= len && s.head <= len,
            "selection {s:?} past end {len}"
        );
    }
    for w in sels.windows(2) {
        let (a, b) = (w[0], w[1]);
        prop_assert!(a.start() <= b.start(), "unsorted: {a:?} {b:?}");
        prop_assert!(b.start() >= a.end(), "overlapping: {a:?} {b:?}");
        prop_assert!(
            !(a.start() == b.start() && a.is_empty() && b.is_empty()),
            "duplicate cursor: {a:?}"
        );
    }
    let newest = e.newest_selection();
    prop_assert!(sels.contains(&newest) || sels.iter().any(|s| s.range() == newest.range()));
    prop_assert_eq!(e.rope().len_chars(), len);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn edits_keep_selections_valid(initial in tricky_text(40), ops in prop::collection::vec(op(true), 1..60)) {
        let mut e = Editor::new(&initial);
        check_invariants(&e)?;
        for op in &ops {
            run(&mut e, op);
            check_invariants(&e)?;
        }
    }

    #[test]
    fn undo_all_restores_the_original_and_redo_all_the_result(
        initial in tricky_text(40),
        ops in prop::collection::vec(op(false), 1..50),
    ) {
        let mut e = Editor::new(&initial);
        for op in &ops {
            run(&mut e, op);
        }
        let result = e.text();
        while e.undo() {
            check_invariants(&e)?;
        }
        prop_assert_eq!(e.text(), initial);
        while e.redo() {
            check_invariants(&e)?;
        }
        prop_assert_eq!(e.text(), result);
    }

    #[test]
    fn display_columns_round_trip(text in tricky_text(60), line_frac in 0.0..=1.0f64, col_frac in 0.0..=1.0f64) {
        let e = Editor::new(&text);
        let line = ((e.line_count() - 1) as f64 * line_frac).round() as usize;
        let col = (e.line_len(line) as f64 * col_frac).round() as usize;
        let d = e.display_col(line, col);
        let back = e.col_for_display(line, d);
        prop_assert_eq!(e.display_col(line, back), d);
        // display columns never go backwards
        if col > 0 {
            prop_assert!(e.display_col(line, col - 1) <= d);
        }
    }
}

fn line_state() -> impl Strategy<Value = LineState> {
    prop_oneof![
        Just(LineState::Normal),
        Just(LineState::BlockComment),
        Just(LineState::Str('\'')),
        Just(LineState::Str('"')),
        Just(LineState::Str('`')),
        "[a-z]{0,3}".prop_map(|t| LineState::Dollar(format!("${t}$"))),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn lexer_tokens_are_ordered_and_on_char_boundaries(
        line in "[^\n]{0,80}",
        tricky in tricky_text(40),
        state in line_state(),
    ) {
        for line in [line, tricky.replace('\n', " ")] {
            let mut tokens = Vec::new();
            lex_line(&line, state.clone(), |t| tokens.push(t));
            let mut prev_end = 0;
            for t in &tokens {
                prop_assert!(t.range.start < t.range.end, "empty token {t:?}");
                prop_assert!(t.range.start >= prev_end, "token {t:?} overlaps previous end {prev_end}");
                prop_assert!(t.range.end <= line.len());
                prop_assert!(line.is_char_boundary(t.range.start) && line.is_char_boundary(t.range.end));
                prev_end = t.range.end;
            }
        }
    }

    #[test]
    fn statement_splitting_keeps_all_code(text in tricky_text(120)) {
        let rope = Rope::from_str(&text);
        let ranges = statements(&rope);
        let chars: Vec<char> = text.chars().collect();
        let mut covered = vec![false; chars.len()];
        let mut prev_end = 0;
        for r in &ranges {
            prop_assert!(r.start < r.end && r.end <= chars.len(), "bad range {r:?}");
            prop_assert!(r.start >= prev_end, "ranges out of order");
            prev_end = r.end;
            for c in &mut covered[r.clone()] {
                *c = true;
            }
        }
        for (i, c) in chars.iter().enumerate() {
            prop_assert!(
                covered[i] || c.is_whitespace() || *c == ';',
                "char {c:?} at {i} dropped from {text:?}"
            );
        }
    }

    #[test]
    fn plain_search_matches_a_naive_search(
        hay in "[abAB ;\n]{0,80}",
        needle in "[abAB]{1,3}",
        case_sensitive in any::<bool>(),
    ) {
        let rope = Rope::from_str(&hay);
        let query = SearchQuery { text: needle.clone(), case_sensitive, ..Default::default() };
        let re = query.compile().unwrap().unwrap();
        let got = find_all(&rope, &re);

        let (h, n) = if case_sensitive {
            (hay.clone(), needle.clone())
        } else {
            (hay.to_ascii_lowercase(), needle.to_ascii_lowercase())
        };
        // ASCII only, so byte offsets are char offsets
        let expected: Vec<_> = h.match_indices(&n).map(|(i, m)| i..i + m.len()).collect();
        prop_assert_eq!(got, expected);
    }
}
