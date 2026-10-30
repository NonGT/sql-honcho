use std::ops::Range;

use gpui::{
    actions, div, prelude::*, px, App, Context, Entity, FocusHandle, Focusable, Hsla, KeyBinding,
    MouseButton, Subscription, Window,
};
use honcho_core::search::{self, SearchQuery};
use honcho_core::Selection;

use crate::editor_view::{self, EditorView};
use crate::theme::{Theme, FONT_FAMILY};

actions!(
    find,
    [
        Find,
        FindReplace,
        FindNext,
        FindPrev,
        SelectAllMatches,
        ReplaceNext,
        ReplaceAll,
        ToggleCase,
        ToggleWholeWord,
        ToggleRegex,
        FocusFind,
        FocusReplace,
    ]
);

/// Must run after the editor bindings so Enter etc. win inside the inputs.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-f", Find, None),
        KeyBinding::new("ctrl-f", Find, None),
        KeyBinding::new("cmd-alt-f", FindReplace, None),
        KeyBinding::new("ctrl-h", FindReplace, None),
        KeyBinding::new("cmd-g", FindNext, None),
        KeyBinding::new("f3", FindNext, None),
        KeyBinding::new("cmd-shift-g", FindPrev, None),
        KeyBinding::new("shift-f3", FindPrev, None),
        KeyBinding::new("alt-cmd-c", ToggleCase, Some("FindBar")),
        KeyBinding::new("alt-cmd-w", ToggleWholeWord, Some("FindBar")),
        KeyBinding::new("alt-cmd-r", ToggleRegex, Some("FindBar")),
        KeyBinding::new("enter", FindNext, Some("FindQuery")),
        KeyBinding::new("shift-enter", FindPrev, Some("FindQuery")),
        KeyBinding::new("alt-enter", SelectAllMatches, Some("FindQuery")),
        KeyBinding::new("tab", FocusReplace, Some("FindQuery")),
        KeyBinding::new("enter", ReplaceNext, Some("ReplaceQuery")),
        KeyBinding::new("cmd-enter", ReplaceAll, Some("ReplaceQuery")),
        KeyBinding::new("cmd-alt-enter", ReplaceAll, Some("FindBar")),
        KeyBinding::new("shift-tab", FocusFind, Some("ReplaceQuery")),
    ]);
}

pub struct FindBar {
    focus_handle: FocusHandle,
    pub open: bool,
    show_replace: bool,
    query_input: Entity<EditorView>,
    replace_input: Entity<EditorView>,
    case_sensitive: bool,
    whole_word: bool,
    regex: bool,
    matches: Vec<Range<usize>>,
    error: Option<String>,
    target: Option<Entity<EditorView>>,
    /// What the current matches were computed from: query and target version.
    searched: Option<(SearchQuery, u64)>,
    /// Where the cursor was when the bar opened; live search starts from here.
    origin: usize,
    target_sub: Option<Subscription>,
    _subs: Vec<Subscription>,
}

impl FindBar {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let query_input = cx.new(|cx| EditorView::single_line("FindQuery", "Find", cx));
        let replace_input = cx.new(|cx| EditorView::single_line("ReplaceQuery", "Replace", cx));
        let subs = vec![cx.observe(&query_input, |this, _, cx| this.research(true, cx))];
        Self {
            focus_handle: cx.focus_handle(),
            open: false,
            show_replace: false,
            query_input,
            replace_input,
            case_sensitive: false,
            whole_word: false,
            regex: false,
            matches: Vec::new(),
            error: None,
            target: None,
            searched: None,
            origin: 0,
            target_sub: None,
            _subs: subs,
        }
    }

    pub fn set_target(&mut self, target: Entity<EditorView>, cx: &mut Context<Self>) {
        if self.target.as_ref() == Some(&target) {
            return;
        }
        self.clear_highlights(cx);
        self.target_sub = Some(cx.observe(&target, |this, _, cx| this.research(false, cx)));
        self.origin = target.read(cx).editor.newest_selection().start();
        self.target = Some(target);
        self.searched = None;
        self.research(false, cx);
    }

    fn query(&self, cx: &App) -> SearchQuery {
        SearchQuery {
            text: self.query_input.read(cx).text(),
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            regex: self.regex,
        }
    }

    pub fn show(&mut self, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else { return };
        let seed = {
            let ed = &target.read(cx).editor;
            let sel = ed.newest_selection();
            self.origin = sel.start();
            if !sel.is_empty() {
                let text = ed.slice(sel.range());
                (!text.contains('\n')).then_some(text)
            } else if !self.open || self.query_input.read(cx).text().is_empty() {
                ed.word_at(sel.head).map(|r| ed.slice(r))
            } else {
                None
            }
        };
        if let Some(seed) = seed {
            let seed = if self.regex { regex_escape(&seed) } else { seed };
            self.query_input.update(cx, |input, cx| input.set_text(&seed, cx));
        } else {
            self.query_input.update(cx, |input, cx| input.set_text(&input.text(), cx));
        }
        self.open = true;
        self.show_replace = replace;
        let has_query = !self.query_input.read(cx).text().is_empty();
        let focus = if replace && has_query { &self.replace_input } else { &self.query_input };
        window.focus(&focus.focus_handle(cx));
        self.searched = None;
        self.research(true, cx);
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = false;
        self.clear_highlights(cx);
        if let Some(target) = &self.target {
            window.focus(&target.focus_handle(cx));
        }
        cx.notify();
    }

    fn clear_highlights(&mut self, cx: &mut Context<Self>) {
        if let Some(target) = &self.target {
            target.update(cx, |t, cx| {
                if !t.search_matches.is_empty() {
                    t.search_matches.clear();
                    cx.notify();
                }
            });
        }
        self.matches.clear();
    }

    /// Recompute matches if the query or the target text changed.
    /// With `jump`, select the first match from where the search started.
    fn research(&mut self, jump: bool, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        let Some(target) = self.target.clone() else { return };
        let query = self.query(cx);
        let version = target.read(cx).editor.version();
        let key = (query.clone(), version);
        if self.searched.as_ref() == Some(&key) {
            return;
        }
        let query_changed = self.searched.as_ref().is_none_or(|(q, _)| *q != query);
        self.searched = Some(key);

        self.error = None;
        self.matches = match query.compile() {
            Ok(Some(re)) => search::find_all(target.read(cx).editor.rope(), &re),
            Ok(None) => Vec::new(),
            Err(e) => {
                self.error = Some(e);
                Vec::new()
            }
        };
        let matches = self.matches.clone();
        let origin = self.origin;
        target.update(cx, |t, cx| {
            t.search_matches = matches;
            if jump && query_changed {
                if let Some(r) = t
                    .search_matches
                    .iter()
                    .find(|r| r.start >= origin)
                    .or(t.search_matches.first())
                    .cloned()
                {
                    t.editor.set_selections(vec![Selection::new(r.start, r.end)], 0);
                    t.request_autoscroll(cx);
                }
            }
            cx.notify();
        });
        cx.notify();
    }

    fn current_index(&self, cx: &App) -> Option<usize> {
        let sel = self.target.as_ref()?.read(cx).editor.newest_selection();
        self.matches
            .binary_search_by(|r| r.start.cmp(&sel.start()))
            .ok()
            .filter(|&i| self.matches[i].end == sel.end())
    }

    pub fn next(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.open {
            self.show(false, window, cx);
            return;
        }
        self.research(false, cx);
        let Some(target) = self.target.clone() else { return };
        if self.matches.is_empty() {
            return;
        }
        let sel = target.read(cx).editor.newest_selection();
        let r = if forward {
            let i = self.matches.partition_point(|r| r.start < sel.end().max(sel.start() + 1));
            self.matches.get(i).unwrap_or(&self.matches[0]).clone()
        } else {
            let i = self.matches.partition_point(|r| r.start < sel.start());
            if i == 0 {
                self.matches.last().unwrap().clone()
            } else {
                self.matches[i - 1].clone()
            }
        };
        target.update(cx, |t, cx| {
            t.editor.set_selections(vec![Selection::new(r.start, r.end)], 0);
            t.request_autoscroll(cx);
        });
        cx.notify();
    }

    fn select_all_matches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else { return };
        if self.matches.is_empty() {
            return;
        }
        let sels = self.matches.iter().map(|r| Selection::new(r.start, r.end)).collect();
        target.update(cx, |t, cx| {
            t.editor.set_selections(sels, 0);
            t.request_autoscroll(cx);
        });
        self.close(window, cx);
    }

    fn replace_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else { return };
        let query = self.query(cx);
        let Ok(Some(re)) = query.compile() else { return };
        let Some(i) = self.current_index(cx) else {
            self.next(true, window, cx);
            return;
        };
        let range = self.matches[i].clone();
        let replacement = self.replace_input.read(cx).text();
        target.update(cx, |t, cx| {
            let with = search::replacement_for(t.editor.rope(), &re, query.regex, &range, &replacement);
            let len = with.chars().count();
            t.editor.replace_ranges(vec![(range.clone(), with)]);
            t.editor.set_cursor(range.start + len);
            cx.notify();
        });
        self.next(true, window, cx);
    }

    fn replace_all(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else { return };
        let query = self.query(cx);
        let Ok(Some(re)) = query.compile() else { return };
        let replacement = self.replace_input.read(cx).text();
        target.update(cx, |t, cx| {
            let edits = search::replace_all(t.editor.rope(), &re, query.regex, &replacement);
            t.editor.replace_ranges(edits);
            cx.notify();
        });
        self.research(false, cx);
    }

    fn toggle(&mut self, which: fn(&mut Self) -> &mut bool, cx: &mut Context<Self>) {
        let flag = which(self);
        *flag = !*flag;
        self.research(true, cx);
    }

    fn status(&self, cx: &App) -> (String, Hsla) {
        if let Some(e) = &self.error {
            return (e.clone(), Theme::error());
        }
        if self.query_input.read(cx).text().is_empty() {
            return (String::new(), Theme::muted());
        }
        let n = self.matches.len();
        if n == 0 {
            return ("No results".into(), Theme::error());
        }
        let more = if n >= search::MAX_MATCHES { "+" } else { "" };
        match self.current_index(cx) {
            Some(i) => (format!("{} of {n}{more}", i + 1), Theme::muted()),
            None => (format!("{n}{more} results"), Theme::muted()),
        }
    }
}

fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn icon_button(label: &'static str, active: bool) -> gpui::Div {
    div()
        .flex_none()
        .h(px(20.))
        .min_w(px(22.))
        .px_1()
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .text_color(if active { Theme::text() } else { Theme::muted() })
        .when(active, |d| d.bg(Theme::accent_dim()).border_1().border_color(Theme::accent()))
        .hover(|d| d.bg(Theme::border()).text_color(Theme::text()))
        .child(label)
}

impl Focusable for FindBar {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FindBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (status, status_color) = self.status(cx);
        let query_focused = self.query_input.focus_handle(cx).is_focused(window);
        let replace_focused = self.replace_input.focus_handle(cx).is_focused(window);
        let has_matches = !self.matches.is_empty();

        let input_box = |input: &Entity<EditorView>, focused: bool, invalid: bool| {
            div()
                .flex_1()
                .h(px(24.))
                .px_1p5()
                .flex()
                .items_center()
                .bg(Theme::input_bg())
                .rounded_sm()
                .border_1()
                .border_color(if invalid {
                    Theme::error()
                } else if focused {
                    Theme::accent()
                } else {
                    Theme::border()
                })
                .overflow_hidden()
                .child(div().flex_1().h(px(18.)).overflow_hidden().child(input.clone()))
        };

        let find_row = div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                icon_button(if self.show_replace { "▾" } else { "▸" }, false).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.show_replace = !this.show_replace;
                        if this.show_replace {
                            window.focus(&this.replace_input.focus_handle(cx));
                        }
                        cx.notify();
                    }),
                ),
            )
            .child(
                input_box(&self.query_input, query_focused, self.error.is_some())
                    .child(icon_button("Aa", self.case_sensitive).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.toggle(|s| &mut s.case_sensitive, cx)),
                    ))
                    .child(icon_button("ab", self.whole_word).underline().on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.toggle(|s| &mut s.whole_word, cx)),
                    ))
                    .child(icon_button(".*", self.regex).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.toggle(|s| &mut s.regex, cx)),
                    )),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(110.))
                    .px_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(status_color)
                    .child(status),
            )
            .child(icon_button("↑", false).on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.next(false, window, cx)),
            ))
            .child(icon_button("↓", false).on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.next(true, window, cx)),
            ))
            .child(icon_button("×", false).on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close(window, cx)),
            ));

        let replace_row = div()
            .flex()
            .items_center()
            .gap_1()
            .child(div().flex_none().w(px(22.)))
            .child(input_box(&self.replace_input, replace_focused, false))
            .child(
                icon_button("Replace", false)
                    .when(!has_matches, |d| d.opacity(0.4))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| this.replace_next(window, cx)),
                    ),
            )
            .child(
                icon_button("All", false)
                    .when(!has_matches, |d| d.opacity(0.4))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.replace_all(cx)),
                    ),
            )
            .child(div().flex_none().w(px(46.)));

        div()
            .key_context("FindBar")
            .track_focus(&self.focus_handle)
            .occlude()
            .w(px(560.))
            .p_1p5()
            .flex()
            .flex_col()
            .gap_1()
            .bg(Theme::panel())
            .border_1()
            .border_color(Theme::border())
            .rounded_md()
            .shadow_lg()
            .font_family(FONT_FAMILY)
            .text_size(px(12.))
            .on_action(cx.listener(|this, _: &FindNext, window, cx| this.next(true, window, cx)))
            .on_action(cx.listener(|this, _: &FindPrev, window, cx| this.next(false, window, cx)))
            .on_action(cx.listener(|this, _: &SelectAllMatches, window, cx| this.select_all_matches(window, cx)))
            .on_action(cx.listener(|this, _: &ReplaceNext, window, cx| this.replace_next(window, cx)))
            .on_action(cx.listener(|this, _: &ReplaceAll, _, cx| this.replace_all(cx)))
            .on_action(cx.listener(|this, _: &ToggleCase, _, cx| this.toggle(|s| &mut s.case_sensitive, cx)))
            .on_action(cx.listener(|this, _: &ToggleWholeWord, _, cx| this.toggle(|s| &mut s.whole_word, cx)))
            .on_action(cx.listener(|this, _: &ToggleRegex, _, cx| this.toggle(|s| &mut s.regex, cx)))
            .on_action(cx.listener(|this, _: &FocusReplace, window, cx| {
                this.show_replace = true;
                window.focus(&this.replace_input.focus_handle(cx));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &FocusFind, window, cx| {
                window.focus(&this.query_input.focus_handle(cx));
                cx.notify();
            }))
            // Escape in an input that has nothing to collapse bubbles up as Cancel
            .on_action(cx.listener(|this, _: &editor_view::Cancel, window, cx| this.close(window, cx)))
            .child(find_row)
            .when(self.show_replace, |d| d.child(replace_row))
    }
}
