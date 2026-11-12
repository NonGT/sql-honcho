//! A query plan as a tree table, with the selected node's details beside it.

use std::collections::HashSet;

use gpui::{div, prelude::*, px, AnyElement, Context, Hsla, MouseButton, SharedString, Window};
use honcho_db::plan::count;
use honcho_db::{Plan, PlanNode};

use crate::theme::Theme;

const INDENT: f32 = 14.;
const BAR: f32 = 64.;

/// A node, as the child indexes leading to it from the root.
type Path = Vec<usize>;

pub struct PlanView {
    pub plan: Plan,
    collapsed: HashSet<Path>,
    selected: Path,
}

impl PlanView {
    pub fn new(plan: Plan) -> Self {
        Self {
            plan,
            collapsed: HashSet::new(),
            selected: Vec::new(),
        }
    }

    fn node(&self, path: &[usize]) -> Option<&PlanNode> {
        path.iter()
            .try_fold(&self.plan.root, |n, &i| n.children.get(i))
    }

    /// The rows on screen: every node whose parents are all open.
    fn rows(&self) -> Vec<(Path, &PlanNode)> {
        fn walk<'a>(
            n: &'a PlanNode,
            path: Path,
            collapsed: &HashSet<Path>,
            out: &mut Vec<(Path, &'a PlanNode)>,
        ) {
            let open = !collapsed.contains(&path);
            out.push((path.clone(), n));
            if open {
                for (i, c) in n.children.iter().enumerate() {
                    let mut p = path.clone();
                    p.push(i);
                    walk(c, p, collapsed, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.plan.root, Vec::new(), &self.collapsed, &mut out);
        out
    }

    pub fn toggle(&mut self, path: Path, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&path) {
            // keep the selection on screen
            if self.selected.starts_with(&path) {
                self.selected = path.clone();
            }
            self.collapsed.insert(path);
        }
        cx.notify();
    }

    #[cfg(test)]
    pub fn shown_operations(&self) -> Vec<String> {
        self.rows()
            .into_iter()
            .map(|(p, n)| format!("{}{}", "  ".repeat(p.len()), n.operation))
            .collect()
    }

    fn render_row(&self, path: Path, n: &PlanNode, cx: &mut Context<Self>) -> AnyElement {
        let analyzed = self.plan.analyzed;
        let selected = path == self.selected;
        let has_children = !n.children.is_empty();
        let open = !self.collapsed.contains(&path);
        let misestimated = n.warnings.iter().any(|w| w.starts_with("Estimated"));
        let share = self.plan.share(n);
        let id = SharedString::from(format!("plan-row-{path:?}"));

        let disclosure = {
            let path = path.clone();
            div()
                .w(px(12.))
                .flex_none()
                .text_color(Theme::muted())
                .when(has_children, |d| {
                    d.child(if open { "▾" } else { "▸" }).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle(path.clone(), cx);
                        }),
                    )
                })
        };
        let operation = div()
            .flex_1()
            .min_w(px(140.))
            .overflow_hidden()
            .flex()
            .items_center()
            .gap_1()
            .pl(px(INDENT * path.len() as f32))
            .child(disclosure)
            .child(
                div()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(n.operation.clone()),
            )
            .when(!n.warnings.is_empty(), |d| {
                d.child(div().flex_none().text_color(Theme::warning()).child("⚠"))
            });
        let object = match (&n.object, &n.index) {
            (Some(o), Some(i)) => format!("{o} using {i}"),
            (Some(o), None) => o.clone(),
            (None, Some(i)) => i.clone(),
            (None, None) => String::new(),
        };
        let actual = n.rows_per_loop().map(count).unwrap_or_default();
        let measure = if analyzed {
            n.actual_ms.map(ms)
        } else {
            n.est_cost.map(|c| format!("{c:.2}"))
        };

        div()
            .id(id)
            .flex()
            .items_center()
            .h(px(22.))
            .px_2()
            .gap_2()
            .when(selected, |d| d.bg(Theme::selection()))
            .when(!selected, |d| d.hover(|d| d.bg(Theme::current_line())))
            .child(operation)
            .child(object_cell().text_color(Theme::muted()).child(object))
            .child(number(n.est_rows.map(count).unwrap_or_default(), None))
            .when(analyzed, |d| {
                d.child(number(actual, misestimated.then(Theme::warning)))
            })
            .child(number(measure.unwrap_or_default(), None))
            .child(bar(share))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.selected = path.clone();
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    fn render_details(&self) -> impl IntoElement {
        let Some(n) = self.node(&self.selected) else {
            return div();
        };
        let line = |k: String, v: String| {
            div()
                .flex()
                .gap_2()
                .child(
                    div()
                        .flex_none()
                        .w(px(110.))
                        .text_color(Theme::muted())
                        .child(k),
                )
                .child(div().flex_1().min_w_0().child(v))
        };
        let mut lines = Vec::new();
        let mut add = |k: &str, v: Option<String>| {
            if let Some(v) = v {
                lines.push(line(k.into(), v));
            }
        };
        add("Object", n.object.clone());
        add("Index", n.index.clone());
        add("Condition", n.condition.clone());
        add("Estimated rows", n.est_rows.map(count));
        add("Estimated cost", n.est_cost.map(|c| format!("{c:.2}")));
        add("Actual rows", n.actual_rows.map(count));
        add("Loops", n.loops.map(count));
        add("Time", n.actual_ms.map(ms));
        for (k, v) in &n.details {
            lines.push(line(k.clone(), v.clone()));
        }
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .font_weight(gpui::FontWeight::BOLD)
                    .child(n.operation.clone()),
            )
            .children(
                n.warnings
                    .iter()
                    .map(|w| div().text_color(Theme::warning()).child(format!("⚠ {w}"))),
            )
            .children(lines)
    }
}

fn ms(v: f64) -> String {
    if v < 10.0 {
        format!("{v:.3} ms")
    } else {
        format!("{v:.1} ms")
    }
}

fn cell(width: f32) -> gpui::Div {
    div()
        .flex_none()
        .w(px(width))
        .overflow_hidden()
        .whitespace_nowrap()
}

fn object_cell() -> gpui::Div {
    div()
        .flex_1()
        .min_w(px(60.))
        .overflow_hidden()
        .whitespace_nowrap()
}

fn number(text: String, color: Option<Hsla>) -> gpui::Div {
    cell(72.)
        .flex()
        .justify_end()
        .when_some(color, |d, c| d.text_color(c))
        .child(text)
}

fn bar(share: Option<f64>) -> gpui::Div {
    let Some(share) = share else {
        return cell(BAR + 40.);
    };
    let color = if share >= 0.3 {
        Theme::warning()
    } else {
        Theme::accent()
    };
    cell(BAR + 40.)
        .flex()
        .items_center()
        .gap_1()
        .child(
            div()
                .w(px(BAR))
                .h(px(6.))
                .rounded_sm()
                .bg(Theme::border())
                .child(
                    div()
                        .h_full()
                        .rounded_sm()
                        .w(px(BAR * share as f32))
                        .bg(color),
                ),
        )
        .child(
            div()
                .flex_1()
                .flex()
                .justify_end()
                .text_color(Theme::muted())
                .child(format!("{:.0}%", share * 100.0)),
        )
}

impl Render for PlanView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let analyzed = self.plan.analyzed;
        let header =
            |label: &'static str, width: f32| cell(width).flex().justify_end().child(label);
        let head = div()
            .flex()
            .items_center()
            .h(px(22.))
            .px_2()
            .gap_2()
            .text_color(Theme::muted())
            .border_b_1()
            .border_color(Theme::border())
            .child(div().flex_1().min_w(px(140.)).child("Operation"))
            .child(object_cell().child("Object"))
            .child(header("Est. rows", 72.))
            .when(analyzed, |d| d.child(header("Rows", 72.)))
            .child(header(if analyzed { "Time" } else { "Cost" }, 72.))
            .child(cell(BAR + 40.).child("Share"));
        let totals = (!self.plan.totals.is_empty()).then(|| {
            div()
                .px_2()
                .py_1()
                .flex()
                .gap_4()
                .text_color(Theme::muted())
                .children(self.plan.totals.iter().map(|(k, v)| format!("{k}: {v}")))
        });
        let rows: Vec<_> = self
            .rows()
            .into_iter()
            .map(|(p, n)| self.render_row(p, n, cx))
            .collect();
        div()
            .size_full()
            .flex()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .children(totals)
                    .child(head)
                    .child(
                        div()
                            .id("plan-rows")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .children(rows),
                    ),
            )
            .child(
                div()
                    .id("plan-details")
                    .flex_none()
                    .w(px(260.))
                    .p_2()
                    .border_l_1()
                    .border_color(Theme::border())
                    .overflow_y_scroll()
                    .child(self.render_details()),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_naturally() {
        assert_eq!(ms(0.0123), "0.012 ms");
        assert_eq!(ms(42.25), "42.2 ms");
    }
}
