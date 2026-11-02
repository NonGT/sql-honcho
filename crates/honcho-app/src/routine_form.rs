//! A form for a routine's parameters. It writes the CALL out for the user
//! to see, and running puts that script in a new tab.

use gpui::{
    actions, div, prelude::*, px, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Window,
};
use honcho_config::SavedConnection;
use honcho_db::{ObjectKind, ObjectRef, ParamMode, RoutineParam};

use crate::editor_view::{self, EditorView};
use crate::theme::{self, Theme};
use honcho_db::routine;

actions!(routine_form, [RunRoutineNow, NextParam, PrevParam]);

pub enum FormEvent {
    Run {
        saved: Box<SavedConnection>,
        database: Option<String>,
        sql: String,
    },
    Closed,
}

pub struct RoutineForm {
    focus: FocusHandle,
    saved: SavedConnection,
    object: ObjectRef,
    params: Vec<RoutineParam>,
    /// One per parameter; OUT parameters have none.
    fields: Vec<Option<Entity<EditorView>>>,
}

impl EventEmitter<FormEvent> for RoutineForm {}

impl RoutineForm {
    pub fn new(
        saved: SavedConnection,
        object: ObjectRef,
        params: Vec<RoutineParam>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let fields: Vec<Option<Entity<EditorView>>> = params
            .iter()
            .map(|p| {
                (p.mode != ParamMode::Out).then(|| {
                    let f = cx.new(|cx| EditorView::single_line("RoutineParam", "NULL", cx));
                    cx.observe(&f, |_, _, cx| cx.notify()).detach();
                    f
                })
            })
            .collect();
        let focus = cx.focus_handle();
        match fields.iter().flatten().next() {
            Some(first) => window.focus(&first.focus_handle(cx)),
            None => window.focus(&focus),
        }
        Self {
            focus,
            saved,
            object,
            params,
            fields,
        }
    }

    fn values(&self, cx: &App) -> Vec<String> {
        self.fields
            .iter()
            .map(|f| f.as_ref().map(|f| f.read(cx).text()).unwrap_or_default())
            .collect()
    }

    pub fn sql(&self, cx: &App) -> String {
        routine::call(
            self.saved.driver,
            &self.object,
            &self.params,
            &self.values(cx),
        )
    }

    fn run(&mut self, cx: &mut Context<Self>) {
        cx.emit(FormEvent::Run {
            saved: Box::new(self.saved.clone()),
            database: self.object.database.clone(),
            sql: self.sql(cx),
        });
    }

    fn step(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let fields: Vec<&Entity<EditorView>> = self.fields.iter().flatten().collect();
        if fields.is_empty() {
            return;
        }
        let current = fields
            .iter()
            .position(|f| f.focus_handle(cx).is_focused(window))
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(fields.len() as isize) as usize;
        let field = fields[next].clone();
        window.focus(&field.focus_handle(cx));
    }

    #[cfg(test)]
    pub fn set_value(&mut self, ix: usize, text: &str, cx: &mut Context<Self>) {
        if let Some(Some(f)) = self.fields.get(ix) {
            f.update(cx, |f, cx| f.set_text(text, cx));
        }
    }
}

impl Focusable for RoutineForm {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn mode_name(mode: ParamMode) -> &'static str {
    match mode {
        ParamMode::In => "IN",
        ParamMode::Out => "OUT",
        ParamMode::InOut => "INOUT",
    }
}

fn button(id: &'static str, label: &'static str, primary: bool) -> gpui::Stateful<gpui::Div> {
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
        .when(primary, |d| d.bg(Theme::accent_dim()))
        .cursor_pointer()
        .hover(|d| d.bg(Theme::border()))
        .child(label)
}

impl Render for RoutineForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let what = match self.object.kind {
            ObjectKind::Function => "function",
            _ => "procedure",
        };
        let title = format!(
            "Run {what} {}",
            routine::qualified(self.saved.driver, &self.object)
        );
        let rows = self.params.iter().zip(&self.fields).map(|(p, field)| {
            let input = match field {
                Some(f) => {
                    let focused = f.focus_handle(cx).is_focused(window);
                    div()
                        .flex_1()
                        .h(px(26.))
                        .px_1p5()
                        .flex()
                        .items_center()
                        .bg(Theme::input_bg())
                        .rounded_sm()
                        .border_1()
                        .border_color(if focused {
                            Theme::accent()
                        } else {
                            Theme::border()
                        })
                        .child(div().flex_1().h(px(18.)).overflow_hidden().child(f.clone()))
                        .into_any_element()
                }
                None => div()
                    .flex_1()
                    .text_color(Theme::muted())
                    .child("comes back in the result")
                    .into_any_element(),
            };
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .w(px(150.))
                        .flex_none()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(p.name.clone()),
                )
                .child(
                    div()
                        .w(px(150.))
                        .flex_none()
                        .text_color(Theme::muted())
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(format!("{} {}", mode_name(p.mode), p.data_type)),
                )
                .child(input)
        });
        let sql = self.sql(cx);

        let form = div()
            .key_context("RoutineForm")
            .track_focus(&self.focus)
            .occlude()
            .w(px(640.))
            .p_4()
            .flex()
            .flex_col()
            .gap_2()
            .bg(Theme::panel())
            .border_1()
            .border_color(Theme::border())
            .rounded_lg()
            .shadow_lg()
            .text_color(Theme::text())
            .font_family(theme::font_family(cx))
            .text_size(px(12.))
            .on_action(cx.listener(|this, _: &RunRoutineNow, _, cx| this.run(cx)))
            .on_action(cx.listener(|this, _: &NextParam, window, cx| this.step(1, window, cx)))
            .on_action(cx.listener(|this, _: &PrevParam, window, cx| this.step(-1, window, cx)))
            .on_action(cx.listener(|_, _: &editor_view::Cancel, _, cx| cx.emit(FormEvent::Closed)))
            .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(title))
            .when(self.params.is_empty(), |d| {
                d.child(div().text_color(Theme::muted()).child("No parameters."))
            })
            .children(rows)
            .child(
                div()
                    .mt_1()
                    .p_2()
                    .rounded_sm()
                    .bg(Theme::bg())
                    .text_color(Theme::muted())
                    .whitespace_normal()
                    .child(sql),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        button("form-cancel", "Cancel", false)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(FormEvent::Closed))),
                    )
                    .child(
                        button("form-run", "Run in a new tab", true)
                            .on_click(cx.listener(|this, _, _, cx| this.run(cx))),
                    ),
            );
        div()
            .absolute()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.4))
            .occlude()
            .child(form)
    }
}
