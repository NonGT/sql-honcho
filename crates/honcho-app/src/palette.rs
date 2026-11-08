//! The command palette: every menu command, found by typing part of its
//! name. Recently run commands come first and are remembered between runs.

use std::ops::Range;
use std::path::PathBuf;

use gpui::{
    actions, div, prelude::*, px, Action, App, Context, Entity, EventEmitter, FocusHandle,
    Focusable, HighlightStyle, MouseButton, ScrollHandle, StyledText, Subscription, Window,
};

use crate::commands::{self, Command};
use crate::connections::Connections;
use crate::editor_view::{self, EditorView};
use crate::keymap::Platform;
use crate::theme::{self, Theme};

actions!(palette, [PaletteUp, PaletteDown, PaletteRun]);

const RECENT_KEPT: usize = 8;

pub enum PaletteEvent {
    Run(Box<dyn Action>),
    Dismissed,
}

pub struct CommandPalette {
    input: Entity<EditorView>,
    commands: Vec<Command>,
    /// Labels, most recent first.
    recent: Vec<String>,
    /// Indexes into `commands`, best first, with the matched ranges.
    shown: Vec<(usize, Vec<Range<usize>>)>,
    selected: usize,
    scroll: ScrollHandle,
    _sub: Subscription,
}

impl EventEmitter<PaletteEvent> for CommandPalette {}

impl Focusable for CommandPalette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl CommandPalette {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| EditorView::single_line("PaletteQuery", "Type a command", cx));
        let sub = cx.observe(&input, |this, _, cx| this.refilter(cx));
        let mut this = Self {
            input,
            commands: commands::palette_commands(Platform::current()),
            recent: load_recent(cx),
            shown: Vec::new(),
            selected: 0,
            scroll: ScrollHandle::new(),
            _sub: sub,
        };
        this.refilter(cx);
        this
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let query = self.input.read(cx).text();
        let recent_rank = |label: &str| {
            self.recent
                .iter()
                .position(|r| r == label)
                .unwrap_or(usize::MAX)
        };
        let mut shown: Vec<(i32, usize, usize, Vec<Range<usize>>)> = self
            .commands
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                let (score, ranges) = commands::fuzzy(&query, &c.label)?;
                Some((score, recent_rank(&c.label), i, ranges))
            })
            .collect();
        // recent ones lead only while nothing is typed; after that the best
        // match does, with recency breaking ties
        if query.trim().is_empty() {
            shown.sort_by_key(|(_, recent, i, _)| (*recent, *i));
        } else {
            shown.sort_by_key(|(score, recent, i, _)| (-score, *recent, *i));
        }
        self.shown = shown.into_iter().map(|(_, _, i, r)| (i, r)).collect();
        self.selected = 0;
        self.scroll.scroll_to_item(0);
        cx.notify();
    }

    fn step(&mut self, by: isize, cx: &mut Context<Self>) {
        if self.shown.is_empty() {
            return;
        }
        let n = self.shown.len() as isize;
        self.selected = (self.selected as isize + by).rem_euclid(n) as usize;
        self.scroll.scroll_to_item(self.selected);
        cx.notify();
    }

    fn run(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some((i, _)) = self.shown.get(ix) else {
            return;
        };
        let command = &self.commands[*i];
        self.recent.retain(|r| *r != command.label);
        self.recent.insert(0, command.label.clone());
        self.recent.truncate(RECENT_KEPT);
        save_recent(&self.recent, cx);
        cx.emit(PaletteEvent::Run((command.action)()));
    }

    #[cfg(test)]
    pub fn shown_labels(&self) -> Vec<String> {
        self.shown
            .iter()
            .map(|(i, _)| self.commands[*i].label.clone())
            .collect()
    }
}

fn recent_path(cx: &App) -> PathBuf {
    cx.global::<Connections>()
        .store
        .path()
        .with_file_name("recent-commands.json")
}

fn load_recent(cx: &App) -> Vec<String> {
    std::fs::read_to_string(recent_path(cx))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_recent(recent: &[String], cx: &App) {
    let path = recent_path(cx);
    let text = serde_json::to_string(recent).unwrap_or_default();
    cx.background_spawn(async move {
        let _ = std::fs::write(path, text);
    })
    .detach();
}

impl Render for CommandPalette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let accent = HighlightStyle {
            color: Some(Theme::accent()),
            font_weight: Some(gpui::FontWeight::BOLD),
            ..Default::default()
        };
        let rows = self.shown.iter().enumerate().map(|(row, (i, ranges))| {
            let c = &self.commands[*i];
            let label = StyledText::new(c.label.clone())
                .with_highlights(ranges.iter().map(|r| (r.clone(), accent)));
            div()
                .id(row)
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .py_1()
                .rounded_sm()
                .when(row == self.selected, |d| d.bg(Theme::selection()))
                .hover(|d| d.bg(Theme::border()))
                .child(
                    div()
                        .flex_1()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(label),
                )
                .children(
                    c.shortcut
                        .clone()
                        .map(|s| div().flex_none().text_color(Theme::muted()).child(s)),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.run(row, cx);
                    }),
                )
        });
        let panel = div()
            .key_context("CommandPalette")
            .occlude()
            .w(px(560.))
            .mt(px(64.))
            .flex()
            .flex_col()
            .bg(Theme::panel())
            .border_1()
            .border_color(Theme::border())
            .rounded_lg()
            .shadow_lg()
            .text_color(Theme::text())
            .font_family(theme::font_family(cx))
            .text_size(px(13.))
            .on_action(cx.listener(|this, _: &PaletteUp, _, cx| this.step(-1, cx)))
            .on_action(cx.listener(|this, _: &PaletteDown, _, cx| this.step(1, cx)))
            .on_action(cx.listener(|this, _: &PaletteRun, _, cx| {
                let ix = this.selected;
                this.run(ix, cx)
            }))
            .on_action(
                cx.listener(|_, _: &editor_view::Cancel, _, cx| cx.emit(PaletteEvent::Dismissed)),
            )
            .child(
                div()
                    .h(px(34.))
                    .px_3()
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(Theme::border())
                    .child(div().flex_1().child(self.input.clone())),
            )
            .child(
                div()
                    .id("palette-list")
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .p_1()
                    .when(self.shown.is_empty(), |d| {
                        d.child(
                            div()
                                .px_3()
                                .py_2()
                                .text_color(Theme::muted())
                                .child("No matching commands"),
                        )
                    })
                    .children(rows),
            );
        div()
            .absolute()
            .size_full()
            .flex()
            .justify_center()
            .items_start()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.emit(PaletteEvent::Dismissed)),
            )
            .child(panel)
    }
}
