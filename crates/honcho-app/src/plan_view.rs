//! A query plan as a tree table, with the selected node's details beside it.

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;

use gpui::{
    canvas, div, prelude::*, px, AnyElement, Bounds, Context, CursorStyle, DispatchPhase, Hsla,
    MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, SharedString, Window,
};
use honcho_db::plan::count;
use honcho_db::{Plan, PlanNode};

use crate::theme::Theme;

const INDENT: f32 = 14.;
const BAR: f32 = 40.;
/// The details column never gets narrower than this, nor wider than half the view.
pub const DETAILS_MIN: Pixels = px(260.);

/// A node, as the child indexes leading to it from the root.
type Path = Vec<usize>;

pub struct PlanView {
    pub plan: Plan,
    collapsed: HashSet<Path>,
    selected: Path,
    /// Columns some step has a figure for; SQLite has none at all.
    est_rows: bool,
    measure: bool,
    details_width: Rc<Cell<Pixels>>,
    /// Pointer x and width when a drag of the details edge started.
    drag: Option<(Pixels, Pixels)>,
    bounds: Bounds<Pixels>,
}

impl PlanView {
    pub fn new(plan: Plan) -> Self {
        fn any(n: &PlanNode, f: &dyn Fn(&PlanNode) -> bool) -> bool {
            f(n) || n.children.iter().any(|c| any(c, f))
        }
        let analyzed = plan.analyzed;
        Self {
            est_rows: any(&plan.root, &|n| n.est_rows.is_some()),
            measure: any(&plan.root, &|n| {
                if analyzed {
                    n.actual_ms.is_some()
                } else {
                    n.est_cost.is_some()
                }
            }),
            plan,
            collapsed: HashSet::new(),
            selected: Vec::new(),
            details_width: Rc::new(Cell::new(DETAILS_MIN)),
            drag: None,
            bounds: Bounds::default(),
        }
    }

    pub fn with_details_width(mut self, width: Rc<Cell<Pixels>>) -> Self {
        self.details_width = width;
        self
    }

    #[cfg(test)]
    pub fn details_width(&self) -> Pixels {
        self.clamp_details(self.details_width.get())
    }

    #[cfg(test)]
    pub fn width(&self) -> Pixels {
        self.bounds.size.width
    }

    fn clamp_details(&self, w: Pixels) -> Pixels {
        w.min(self.bounds.size.width / 2.).max(DETAILS_MIN)
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
            .when(self.est_rows, |d| {
                d.child(number(n.est_rows.map(count).unwrap_or_default(), None))
            })
            .when(analyzed, |d| {
                d.child(number(actual, misestimated.then(Theme::warning)))
            })
            .when(self.measure, |d| {
                d.child(number(measure.unwrap_or_default(), None))
                    .child(bar(share))
            })
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
        return cell(BAR + 36.);
    };
    let color = if share >= 0.3 {
        Theme::warning()
    } else {
        Theme::accent()
    };
    cell(BAR + 36.)
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
            .when(self.est_rows, |d| d.child(header("Est. rows", 72.)))
            .when(analyzed, |d| d.child(header("Rows", 72.)))
            .when(self.measure, |d| {
                d.child(header(if analyzed { "Time" } else { "Cost" }, 72.))
                    .child(cell(BAR + 36.).child("Share"))
            });
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
        let width = self.clamp_details(self.details_width.get());
        let this = cx.entity().downgrade();
        div()
            .size_full()
            .relative()
            .flex()
            .child(
                canvas(
                    {
                        let this = this.clone();
                        move |b, _, cx| {
                            this.update(cx, |v, _| v.bounds = b).ok();
                        }
                    },
                    move |_, _, window, _| {
                        // on the window, so the drag follows the pointer out of the view
                        let view = this.clone();
                        window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, cx| {
                            if phase != DispatchPhase::Bubble {
                                return;
                            }
                            view.update(cx, |v, cx| {
                                let Some((x, w)) = v.drag else { return };
                                if e.pressed_button != Some(MouseButton::Left) {
                                    v.drag = None;
                                    return;
                                }
                                let w = v.clamp_details(w + x - e.position.x);
                                v.details_width.set(w);
                                cx.notify();
                            })
                            .ok();
                        });
                        let view = this.clone();
                        window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                            if phase == DispatchPhase::Bubble {
                                view.update(cx, |v, _| v.drag = None).ok();
                            }
                        });
                    },
                )
                .absolute()
                .size_full(),
            )
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
                    .relative()
                    .flex_none()
                    .w(width)
                    .border_l_1()
                    .border_color(Theme::border())
                    .child(
                        div()
                            .id("plan-details")
                            .size_full()
                            .p_2()
                            .overflow_y_scroll()
                            .child(self.render_details()),
                    )
                    // the left edge drags to resize; a double click puts it back
                    .child(
                        div()
                            .id("plan-details-edge")
                            .debug_selector(|| "plan-details-edge".into())
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left(px(-3.))
                            .w(px(6.))
                            .cursor(CursorStyle::ResizeLeftRight)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, e: &gpui::MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    if e.click_count == 2 {
                                        this.details_width.set(DETAILS_MIN);
                                        this.drag = None;
                                        cx.notify();
                                    } else {
                                        let w = this.clamp_details(this.details_width.get());
                                        this.drag = Some((e.position.x, w));
                                    }
                                }),
                            ),
                    ),
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
