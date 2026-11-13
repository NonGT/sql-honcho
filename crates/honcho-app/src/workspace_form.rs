//! Adds or edits a workspace: its name, colour and connections.

use std::collections::HashSet;

use gpui::{
    actions, div, prelude::*, px, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    SharedString, Window,
};
use honcho_config::{ColorTag, SavedWorkspace};

use crate::connection_dialog::color_dot;
use crate::connections::{self, Connections};
use crate::editor_view::{self, EditorView};
use crate::theme::{self, Theme};

actions!(workspace_form, [SaveWorkspace]);

pub enum WorkspaceFormEvent {
    /// Saved, and now the active workspace.
    Saved,
    Closed,
}

pub struct WorkspaceForm {
    focus: FocusHandle,
    draft: SavedWorkspace,
    name: Entity<EditorView>,
    chosen: HashSet<String>,
    is_new: bool,
    error: Option<String>,
}

impl EventEmitter<WorkspaceFormEvent> for WorkspaceForm {}

impl WorkspaceForm {
    /// Edits `workspace`, or starts a new one with nothing in it.
    pub fn new(
        workspace: Option<SavedWorkspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let is_new = workspace.is_none();
        let draft = workspace.unwrap_or_else(|| SavedWorkspace::new(""));
        let name = cx.new(|cx| {
            let mut f = EditorView::single_line("WorkspaceName", "e.g. a customer's name", cx);
            f.set_text(&draft.name, cx);
            f
        });
        cx.observe(&name, |_, _, cx| cx.notify()).detach();
        window.focus(&name.focus_handle(cx));
        Self {
            focus: cx.focus_handle(),
            chosen: draft.connections.iter().cloned().collect(),
            draft,
            name,
            is_new,
            error: None,
        }
    }

    fn toggle(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.chosen.remove(id) {
            self.chosen.insert(id.to_string());
        }
        cx.notify();
    }

    pub fn save(&mut self, cx: &mut Context<Self>) {
        let name = self.name.read(cx).text().trim().to_string();
        if name.is_empty() {
            self.error = Some("Give it a name.".into());
            cx.notify();
            return;
        }
        let state = cx.global_mut::<Connections>();
        // in the saved order, so the browser lists them the same way
        let connections = state
            .store
            .connections()
            .iter()
            .filter(|c| self.chosen.contains(&c.id))
            .map(|c| c.id.clone())
            .collect();
        let workspace = SavedWorkspace {
            name,
            connections,
            ..self.draft.clone()
        };
        state.workspaces.upsert(workspace.clone());
        state.workspaces.set_active(Some(&workspace.id));
        match state.save_workspaces() {
            Ok(()) => cx.emit(WorkspaceFormEvent::Saved),
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }

    #[cfg(test)]
    pub fn fill(&mut self, name: &str, connections: &[&str], cx: &mut Context<Self>) {
        self.name.update(cx, |f, cx| f.set_text(name, cx));
        self.chosen = connections.iter().map(|c| c.to_string()).collect();
    }
}

impl Focusable for WorkspaceForm {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
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

fn tick(on: bool) -> gpui::Div {
    div()
        .size(px(14.))
        .flex_none()
        .rounded_sm()
        .border_1()
        .border_color(Theme::muted())
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(10.))
        .when(on, |d| {
            d.bg(Theme::accent())
                .border_color(Theme::accent())
                .child("✓")
        })
}

impl Render for WorkspaceForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.name.focus_handle(cx).is_focused(window);
        let saved = cx.global::<Connections>().store.connections().to_vec();
        let label = |text: &'static str| {
            div()
                .w(px(84.))
                .flex_none()
                .text_color(Theme::muted())
                .child(text)
        };

        let swatches = std::iter::once(None)
            .chain(ColorTag::ALL.map(Some))
            .map(|tag| {
                let active = self.draft.color == tag;
                div()
                    .id(SharedString::from(format!("ws-color-{tag:?}")))
                    .size(px(18.))
                    .rounded_full()
                    .border_2()
                    .border_color(if active {
                        Theme::text()
                    } else {
                        Theme::panel()
                    })
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .justify_center()
                    .map(|d| match tag {
                        Some(t) => d.bg(connections::color(t)),
                        None => d
                            .bg(Theme::input_bg())
                            .text_color(Theme::muted())
                            .text_size(px(10.))
                            .child("∅"),
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.draft.color = tag;
                        cx.notify();
                    }))
            });

        let rows = saved.into_iter().map(|c| {
            let on = self.chosen.contains(&c.id);
            let id = c.id.clone();
            div()
                .id(SharedString::from(format!("ws-conn-{}", c.id)))
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_0p5()
                .cursor_pointer()
                .hover(|d| d.bg(Theme::border()))
                .child(tick(on))
                .child(color_dot(c.color))
                .child(
                    div()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(c.name.clone()),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.toggle(&id, cx)))
        });

        let form =
            div()
                .key_context("WorkspaceForm")
                .track_focus(&self.focus)
                .occlude()
                .w(px(420.))
                .p_4()
                .flex()
                .flex_col()
                .gap_3()
                .bg(Theme::panel())
                .border_1()
                .border_color(Theme::border())
                .rounded_lg()
                .shadow_lg()
                .text_color(Theme::text())
                .font_family(theme::font_family(cx))
                .text_size(px(12.))
                .on_action(cx.listener(|this, _: &SaveWorkspace, _, cx| this.save(cx)))
                .on_action(cx.listener(|_, _: &editor_view::Cancel, _, cx| {
                    cx.emit(WorkspaceFormEvent::Closed)
                }))
                .child(
                    div()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(if self.is_new {
                            "New workspace"
                        } else {
                            "Edit workspace"
                        }),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(label("Name"))
                        .child(
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
                                .child(
                                    div()
                                        .flex_1()
                                        .h(px(18.))
                                        .overflow_hidden()
                                        .child(self.name.clone()),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(label("Colour"))
                        .child(div().flex().gap_1p5().children(swatches)),
                )
                .child(
                    div().flex().gap_2().child(label("Connections")).child(
                        div()
                            .id("ws-connections")
                            .flex_1()
                            .max_h(px(260.))
                            .overflow_y_scroll()
                            .py_1()
                            .bg(Theme::bg())
                            .rounded_sm()
                            .border_1()
                            .border_color(Theme::border())
                            .children(rows)
                            .when(
                                cx.global::<Connections>().store.connections().is_empty(),
                                |d| {
                                    d.child(
                                        div()
                                            .px_2()
                                            .text_color(Theme::muted())
                                            .child("No saved connections yet."),
                                    )
                                },
                            ),
                    ),
                )
                .children(
                    self.error
                        .clone()
                        .map(|e| div().text_color(Theme::error()).child(e)),
                )
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(button("ws-cancel", "Cancel", false).on_click(
                            cx.listener(|_, _, _, cx| cx.emit(WorkspaceFormEvent::Closed)),
                        ))
                        .child(
                            button("ws-save", "Save", true)
                                .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
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
