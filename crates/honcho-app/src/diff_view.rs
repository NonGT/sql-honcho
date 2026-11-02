//! A project file side by side with what the server has.

use gpui::{
    div, prelude::*, px, uniform_list, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Hsla, Window,
};
use honcho_db::project::Status;
use similar::{ChangeTag, TextDiff};

use crate::editor_view;
use crate::project_pane::ProjectPane;
use crate::theme::{self, Theme};

pub enum DiffEvent {
    Closed,
}

/// A line and its 1-based number.
type Numbered = (usize, String);

/// One line across: either side may be blank where the other has lines
/// the first lacks.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub left: Option<(usize, String)>,
    pub right: Option<(usize, String)>,
}

impl Line {
    fn changed(&self) -> bool {
        match (&self.left, &self.right) {
            (Some((_, a)), Some((_, b))) => a != b,
            _ => true,
        }
    }
}

/// Lines of `left` and `right` paired up, changed runs side by side.
pub fn side_by_side(left: &str, right: &str) -> Vec<Line> {
    let diff = TextDiff::from_lines(left, right);
    let mut out = Vec::new();
    let mut dels: Vec<Numbered> = Vec::new();
    let mut adds: Vec<Numbered> = Vec::new();
    let flush = |out: &mut Vec<Line>, dels: &mut Vec<_>, adds: &mut Vec<_>| {
        let n = dels.len().max(adds.len());
        let mut d = std::mem::take(dels).into_iter();
        let mut a = std::mem::take(adds).into_iter();
        for _ in 0..n {
            out.push(Line {
                left: d.next(),
                right: a.next(),
            });
        }
    };
    for change in diff.iter_all_changes() {
        let text = change.value().trim_end_matches(['\n', '\r']).to_string();
        match change.tag() {
            ChangeTag::Equal => {
                flush(&mut out, &mut dels, &mut adds);
                out.push(Line {
                    left: Some((change.old_index().unwrap() + 1, text.clone())),
                    right: Some((change.new_index().unwrap() + 1, text)),
                });
            }
            ChangeTag::Delete => dels.push((change.old_index().unwrap() + 1, text)),
            ChangeTag::Insert => adds.push((change.new_index().unwrap() + 1, text)),
        }
    }
    flush(&mut out, &mut dels, &mut adds);
    out
}

pub struct DiffView {
    focus: FocusHandle,
    pane: Entity<ProjectPane>,
    ix: usize,
    title: String,
    status: Status,
    has_file: bool,
    has_server: bool,
    pub lines: Vec<Line>,
}

impl EventEmitter<DiffEvent> for DiffView {}

impl DiffView {
    pub fn new(
        pane: Entity<ProjectPane>,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let p = pane.read(cx);
        let driver = p
            .saved(cx)
            .map(|s| s.driver)
            .unwrap_or(honcho_db::DriverKind::MySql);
        let e = &p.entries[ix];
        let file = e.file.clone().unwrap_or_default();
        let server = e.pulled(driver).unwrap_or_default();
        let title = format!(
            "{} · {} on {}",
            e.path.to_string_lossy(),
            p.project.manifest.database,
            p.project.manifest.connection
        );
        let (status, has_file, has_server) = (e.status, e.file.is_some(), e.server.is_some());
        let focus = cx.focus_handle();
        window.focus(&focus);
        Self {
            focus,
            pane,
            ix,
            title,
            status,
            has_file,
            has_server,
            lines: side_by_side(&file, &server),
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(DiffEvent::Closed);
    }
}

impl Focusable for DiffView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn button(id: &'static str, text: &'static str, primary: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(if primary {
            Theme::accent()
        } else {
            Theme::border()
        })
        .cursor_pointer()
        .hover(|d| d.bg(Theme::border()))
        .child(text)
}

fn side(cell: Option<(usize, String)>, changed: bool, tint: Hsla) -> impl IntoElement {
    let (no, text) = match cell {
        Some((n, t)) => (n.to_string(), t),
        None => (String::new(), String::new()),
    };
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .when(changed && !no.is_empty(), |d| d.bg(tint))
        .child(
            div()
                .w(px(40.))
                .flex_none()
                .pr_2()
                .flex()
                .justify_end()
                .text_color(Theme::gutter_text())
                .child(no),
        )
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .whitespace_nowrap()
                .child(text),
        )
}

impl Render for DiffView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lines = self.lines.clone();
        let identical = lines.iter().all(|l| !l.changed());
        let list = uniform_list(
            "diff-lines",
            lines.len(),
            cx.processor(move |_, range: std::ops::Range<usize>, _, _| {
                range
                    .map(|i| {
                        let l = &lines[i];
                        let changed = l.changed();
                        div()
                            .h(px(18.))
                            .flex()
                            .child(side(l.left.clone(), changed, Theme::removed()))
                            .child(div().w(px(1.)).h_full().bg(Theme::border()))
                            .child(side(l.right.clone(), changed, Theme::added()))
                    })
                    .collect()
            }),
        )
        .flex_1();

        let pane = self.pane.clone();
        let ix = self.ix;
        let note = match self.status {
            Status::Same if identical => Some("No difference."),
            Status::Same => Some("Only formatting differs: the server rewrote what was applied."),
            Status::BothChanged => {
                Some("Changed in the folder and on the server since the last pull.")
            }
            Status::Differs => {
                Some("No record of the last pull here, so it can't tell which side changed.")
            }
            _ => None,
        };

        let dialog =
            div()
                .key_context("DiffView")
                .track_focus(&self.focus)
                .occlude()
                .w_5_6()
                .h_5_6()
                .flex()
                .flex_col()
                .bg(Theme::bg())
                .border_1()
                .border_color(Theme::border())
                .rounded_lg()
                .shadow_lg()
                .text_color(Theme::text())
                .font_family(theme::font_family(cx))
                .text_size(px(12.))
                .on_action(cx.listener(|this, _: &editor_view::Cancel, _, cx| this.close(cx)))
                .child(
                    div()
                        .flex_none()
                        .px_3()
                        .py_2()
                        .border_b_1()
                        .border_color(Theme::border())
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child(self.title.clone()),
                        )
                        .children(note.map(|n| div().text_color(Theme::muted()).child(n))),
                )
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .text_color(Theme::muted())
                        .text_size(px(11.))
                        .border_b_1()
                        .border_color(Theme::border())
                        .child(div().flex_1().px_3().py_0p5().child(if self.has_file {
                            "Folder"
                        } else {
                            "Folder (no file)"
                        }))
                        .child(div().flex_1().px_3().py_0p5().child(if self.has_server {
                            "Server"
                        } else {
                            "Server (not there)"
                        })),
                )
                .child(list)
                .child(
                    div()
                        .flex_none()
                        .p_2()
                        .border_t_1()
                        .border_color(Theme::border())
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            button("diff-close", "Close", false)
                                .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                        )
                        .when(self.has_file, |d| {
                            let pane = pane.clone();
                            d.child(
                                button("diff-open", "Open file", false).on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        pane.update(cx, |p, cx| p.open(ix, cx));
                                        this.close(cx);
                                    },
                                )),
                            )
                        })
                        .when(self.has_server, |d| {
                            let pane = pane.clone();
                            d.child(button("diff-pull", "Pull server version", false).on_click(
                                cx.listener(move |this, _, _, cx| {
                                    pane.update(cx, |p, cx| p.pull(vec![ix], cx));
                                    this.close(cx);
                                }),
                            ))
                        })
                        .when(self.has_file, |d| {
                            let pane = pane.clone();
                            d.child(button("diff-apply", "Apply file…", true).on_click(
                                cx.listener(move |this, _, window, cx| {
                                    pane.update(cx, |p, cx| p.apply_one(ix, window, cx));
                                    this.close(cx);
                                }),
                            ))
                        }),
                );

        div()
            .absolute()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.4))
            .occlude()
            .child(dialog)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_runs_pair_up() {
        let lines = side_by_side("a\nb\nc\nd\n", "a\nB\nc\nd\ne\n");
        let pairs: Vec<(Option<&str>, Option<&str>)> = lines
            .iter()
            .map(|l| {
                (
                    l.left.as_ref().map(|(_, t)| t.as_str()),
                    l.right.as_ref().map(|(_, t)| t.as_str()),
                )
            })
            .collect();
        assert_eq!(
            pairs,
            [
                (Some("a"), Some("a")),
                (Some("b"), Some("B")),
                (Some("c"), Some("c")),
                (Some("d"), Some("d")),
                (None, Some("e")),
            ]
        );
        assert_eq!(lines[4].right, Some((5, "e".into())));
        assert!(lines[1].changed() && !lines[0].changed());
    }
}
