//! In-window dialog for platforms without native ones (Linux). gpui's
//! fallback cuts long text off, ignores the keyboard and is always white.

use gpui::{
    div, prelude::*, px, App, Context, EventEmitter, FocusHandle, Focusable, KeyDownEvent,
    PromptButton, PromptHandle, PromptLevel, PromptResponse, RenderablePromptHandle, Window,
};

use crate::theme::Theme;

pub fn build(
    _level: PromptLevel,
    message: &str,
    detail: Option<&str>,
    actions: &[PromptButton],
    handle: PromptHandle,
    window: &mut Window,
    cx: &mut App,
) -> RenderablePromptHandle {
    let view = cx.new(|cx| Dialog {
        message: message.to_string(),
        detail: detail.map(ToString::to_string),
        actions: actions.to_vec(),
        focus: cx.focus_handle(),
    });
    handle.with_view(view, window, cx)
}

struct Dialog {
    message: String,
    detail: Option<String>,
    actions: Vec<PromptButton>,
    focus: FocusHandle,
}

impl Dialog {
    fn cancel_index(&self) -> Option<usize> {
        self.actions
            .iter()
            .position(|a| matches!(a, PromptButton::Cancel(_)) || a.label().as_ref() == "Cancel")
    }

    fn key_down(&mut self, e: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let answer = match e.keystroke.key.as_str() {
            "enter" => Some(0),
            "escape" => self.cancel_index(),
            _ => None,
        };
        if let Some(ix) = answer {
            cx.stop_propagation();
            cx.emit(PromptResponse(ix));
        }
    }
}

impl EventEmitter<PromptResponse> for Dialog {}

impl Focusable for Dialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Dialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let buttons = self.actions.iter().enumerate().map(|(ix, action)| {
            div()
                .id(ix)
                .px_3()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(Theme::border())
                .when(ix == 0, |d| {
                    d.bg(Theme::accent_dim()).border_color(Theme::accent())
                })
                .hover(|d| d.bg(Theme::border()))
                .cursor_pointer()
                .child(action.label().clone())
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(PromptResponse(ix))))
        });
        let dialog = div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .w(px(380.))
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
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(self.message.clone()),
            )
            .children(
                self.detail
                    .clone()
                    .map(|d| div().text_sm().text_color(Theme::muted()).child(d)),
            )
            .child(
                div()
                    .mt_2()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .justify_end()
                    .gap_2()
                    .text_sm()
                    .children(buttons),
            );
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.4))
            .child(dialog)
    }
}
