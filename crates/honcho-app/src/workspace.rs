use std::path::{Path, PathBuf};

use gpui::{
    actions, canvas, div, point, prelude::*, px, App, Bounds, Context, CursorStyle, Decorations,
    Entity, ExternalPaths, FocusHandle, Focusable, HitboxBehavior, MouseButton, Pixels, Point,
    PromptLevel, ResizeEdge, SharedString, Size, Subscription, Task, Window,
};

use crate::editor_view::EditorView;
use crate::find::{self, FindBar};
use crate::theme::{self, EditorFont, Theme, FONT_SIZE};

actions!(
    workspace,
    [
        NewFile, OpenFile, Save, SaveAs, CloseTab, NextTab, PrevTab, ZoomIn, ZoomOut, ZoomReset,
        Quit,
    ]
);

pub struct Workspace {
    focus_handle: FocusHandle,
    tabs: Vec<Entity<EditorView>>,
    active: usize,
    next_untitled: usize,
    find: Entity<FindBar>,
    subscriptions: Vec<Subscription>,
    /// The user already agreed to lose unsaved changes on close.
    discard_confirmed: bool,
}

impl Workspace {
    pub fn new(paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let find = cx.new(FindBar::new);
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            tabs: Vec::new(),
            active: 0,
            next_untitled: 1,
            subscriptions: vec![cx.observe(&find, |_, _, cx| cx.notify())],
            discard_confirmed: false,
            find,
        };
        this.new_tab(String::new(), None, window, cx);
        this.open_paths(paths, window, cx);

        let handle = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            handle
                .update(cx, |this, cx| this.should_close(window, cx))
                .unwrap_or(true)
        });
        this
    }

    fn dirty_count(&self, cx: &App) -> usize {
        self.tabs.iter().filter(|t| t.read(cx).is_dirty()).count()
    }

    /// Run `then` right away if nothing is unsaved, otherwise after the
    /// user confirms losing the changes.
    fn confirm_discard(
        &mut self,
        verb: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let dirty = self.dirty_count(cx);
        if dirty == 0 || self.discard_confirmed {
            then(self, window, cx);
            return;
        }
        let msg = format!(
            "{dirty} file{} not saved. {verb} anyway?",
            if dirty == 1 { " is" } else { "s are" }
        );
        let answer = window.prompt(PromptLevel::Warning, &msg, None, &[verb, "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                this.update_in(cx, |this, window, cx| {
                    this.discard_confirmed = true;
                    then(this, window, cx);
                })
                .ok();
            }
        })
        .detach();
    }

    /// Window close button. Returns whether the window may close now.
    fn should_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.discard_confirmed || self.dirty_count(cx) == 0 {
            return true;
        }
        self.confirm_discard("Close", window, cx, |_, window, _| window.remove_window());
        false
    }

    fn active_editor(&self) -> &Entity<EditorView> {
        &self.tabs[self.active]
    }

    fn new_tab(
        &mut self,
        text: String,
        path: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let untitled = if path.is_none() {
            self.next_untitled += 1;
            self.next_untitled - 1
        } else {
            0
        };
        let editor = cx.new(|cx| EditorView::new(&text, path, untitled, cx));
        self.subscriptions
            .push(cx.observe(&editor, |_, _, cx| cx.notify()));
        self.tabs.push(editor);
        self.activate(self.tabs.len() - 1, window, cx);
    }

    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.active = ix.min(self.tabs.len() - 1);
        let editor = self.active_editor().clone();
        self.find
            .update(cx, |f, cx| f.set_target(editor.clone(), cx));
        window.focus(&editor.focus_handle(cx));
        cx.notify();
    }

    pub fn open_paths(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        for path in paths {
            let path = path.canonicalize().unwrap_or(path);
            if let Some(ix) = self
                .tabs
                .iter()
                .position(|t| t.read(cx).path.as_deref() == Some(path.as_path()))
            {
                self.activate(ix, window, cx);
                continue;
            }
            let text = match std::fs::read(&path) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(err) => {
                    let msg = format!("Couldn't open {}", path.display());
                    drop(window.prompt(
                        PromptLevel::Critical,
                        &msg,
                        Some(&err.to_string()),
                        &["OK"],
                        cx,
                    ));
                    continue;
                }
            };
            if self.active_editor().read(cx).is_pristine() {
                let ix = self.active;
                self.tabs[ix] = cx.new(|cx| EditorView::new(&text, Some(path), 0, cx));
                self.subscriptions
                    .push(cx.observe(&self.tabs[ix], |_, _, cx| cx.notify()));
                self.activate(ix, window, cx);
            } else {
                self.new_tab(text, Some(path), window, cx);
            }
        }
    }

    fn new_file(&mut self, _: &NewFile, window: &mut Window, cx: &mut Context<Self>) {
        self.new_tab(String::new(), None, window, cx);
    }

    fn open_file(&mut self, _: &OpenFile, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                this.update_in(cx, |this, window, cx| this.open_paths(paths, window, cx))
                    .ok();
            }
        })
        .detach();
    }

    fn save(&mut self, _: &Save, window: &mut Window, cx: &mut Context<Self>) {
        self.save_tab(self.active, false, window, cx).detach();
    }

    fn save_as(&mut self, _: &SaveAs, window: &mut Window, cx: &mut Context<Self>) {
        self.save_tab(self.active, true, window, cx).detach();
    }

    /// Resolves to true once the file is written.
    fn save_tab(
        &mut self,
        ix: usize,
        save_as: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<bool> {
        let editor = self.tabs[ix].clone();
        let (existing, title) = {
            let e = editor.read(cx);
            (e.path.clone().filter(|_| !save_as), e.title())
        };
        let new_path_rx = existing.is_none().then(|| {
            let dir = editor
                .read(cx)
                .path
                .as_deref()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .or_else(std::env::home_dir)
                .unwrap_or_else(|| PathBuf::from("/"));
            let name = if title.ends_with(".sql") {
                title.clone()
            } else {
                format!("{title}.sql")
            };
            cx.prompt_for_new_path(&dir, Some(&name))
        });
        cx.spawn_in(window, async move |_, cx| {
            let path = match (existing, new_path_rx) {
                (Some(p), _) => p,
                (None, Some(rx)) => match rx.await {
                    Ok(Ok(Some(p))) => p,
                    _ => return false,
                },
                (None, None) => return false,
            };
            let Ok(contents) = editor.read_with(cx, |e, _| e.contents_for_save()) else {
                return false;
            };
            match std::fs::write(&path, contents) {
                Ok(()) => {
                    editor.update(cx, |e, cx| e.mark_saved(path, cx)).ok();
                    true
                }
                Err(err) => {
                    let msg = format!("Couldn't save {}", path.display());
                    if let Ok(rx) = cx.update(|window, cx| {
                        window.prompt(
                            PromptLevel::Critical,
                            &msg,
                            Some(&err.to_string()),
                            &["OK"],
                            cx,
                        )
                    }) {
                        rx.await.ok();
                    }
                    false
                }
            }
        })
    }

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        self.close(self.active, window, cx);
    }

    fn close(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.tabs[ix].clone();
        if !editor.read(cx).is_dirty() {
            self.remove_tab(ix, window, cx);
            return;
        }
        let msg = format!("Save changes to {}?", editor.read(cx).title());
        let answer = window.prompt(
            PromptLevel::Warning,
            &msg,
            Some("Your changes will be lost if you don't save them."),
            &["Save", "Don't Save", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let close = match answer.await {
                Ok(0) => {
                    let Ok(task) = this.update_in(cx, |this, window, cx| {
                        let ix = this.tabs.iter().position(|t| *t == editor).unwrap_or(ix);
                        this.save_tab(ix, false, window, cx)
                    }) else {
                        return;
                    };
                    task.await
                }
                Ok(1) => true,
                _ => false,
            };
            if close {
                this.update_in(cx, |this, window, cx| {
                    if let Some(ix) = this.tabs.iter().position(|t| *t == editor) {
                        this.remove_tab(ix, window, cx);
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    fn remove_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.tabs.remove(ix);
        if self.tabs.is_empty() {
            self.new_tab(String::new(), None, window, cx);
            return;
        }
        let next = if self.active > ix || self.active >= self.tabs.len() {
            self.active.saturating_sub(1)
        } else {
            self.active
        };
        self.activate(next, window, cx);
    }

    fn cycle_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.tabs.len() as isize;
        let ix = (self.active as isize + delta).rem_euclid(n) as usize;
        self.activate(ix, window, cx);
    }

    fn zoom(&mut self, size: Option<f32>, cx: &mut Context<Self>) {
        let current = cx.global::<EditorFont>().size;
        let next = match size {
            Some(delta) => (current + px(delta)).max(px(8.)).min(px(40.)),
            None => px(FONT_SIZE),
        };
        cx.global_mut::<EditorFont>().size = next;
        cx.notify();
    }

    fn quit(&mut self, _: &Quit, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_discard("Quit", window, cx, |_, _, cx| cx.quit());
    }

    /// Our own title bar, for when the window manager doesn't draw one
    /// (client-side decorations on Wayland).
    fn render_titlebar(&self, title: String, cx: &mut Context<Self>) -> impl IntoElement {
        let button = |label: &'static str| {
            div()
                .flex_none()
                .w(px(36.))
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(Theme::muted())
                .hover(|d| d.bg(Theme::border()).text_color(Theme::text()))
                .child(label)
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .h(px(32.))
            .pl_3()
            .bg(Theme::panel())
            .border_b_1()
            .border_color(Theme::border())
            .on_mouse_down(MouseButton::Left, |e, window, _| {
                if e.click_count == 2 {
                    window.zoom_window();
                } else {
                    window.start_window_move();
                }
            })
            .on_mouse_down(MouseButton::Right, |e, window, _| {
                window.show_window_menu(e.position)
            })
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(Theme::muted())
                    .child(title),
            )
            .child(
                button("—").on_mouse_down(MouseButton::Left, |_, window, cx| {
                    cx.stop_propagation();
                    window.minimize_window();
                }),
            )
            .child(
                button("□").on_mouse_down(MouseButton::Left, |_, window, cx| {
                    cx.stop_propagation();
                    window.zoom_window();
                }),
            )
            .child(button("✕").on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    if this.should_close(window, cx) {
                        window.remove_window();
                    }
                }),
            ))
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_none()
            .h(px(34.))
            .bg(Theme::panel())
            .border_b_1()
            .border_color(Theme::border())
            .overflow_hidden()
            .children(self.tabs.iter().enumerate().map(|(ix, tab)| {
                let t = tab.read(cx);
                let active = ix == self.active;
                let dirty = t.is_dirty();
                let label: SharedString = t.title().into();
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_2()
                    .pl_3()
                    .pr_2()
                    .h_full()
                    .border_t_2()
                    .border_color(if active {
                        Theme::accent()
                    } else {
                        Theme::panel()
                    })
                    .when(active, |d| d.bg(Theme::tab_active()))
                    .text_color(if active {
                        Theme::text()
                    } else {
                        Theme::muted()
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| this.activate(ix, window, cx)),
                    )
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |this, _, window, cx| this.close(ix, window, cx)),
                    )
                    .child(label)
                    .child(
                        div()
                            .w(px(16.))
                            .h(px(16.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_sm()
                            .text_color(Theme::muted())
                            .hover(|d| d.bg(Theme::border()).text_color(Theme::text()))
                            .child(if dirty { "●" } else { "×" })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.close(ix, window, cx);
                                }),
                            ),
                    )
            }))
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = self.active_editor().read(cx);
        let ed = &view.editor;
        let newest = ed.newest_selection();
        let (line, col) = ed.line_col(newest.head);
        let sels = ed.selections();
        let selected: usize = sels.iter().map(|s| s.end() - s.start()).sum();
        let mut left = format!("Ln {}, Col {}", line + 1, ed.display_col(line, col) + 1);
        if selected > 0 {
            left.push_str(&format!("  ({selected} selected)"));
        }
        if sels.len() > 1 {
            left.push_str(&format!("  {} cursors", sels.len()));
        }
        let ending = match view.line_ending {
            honcho_core::LineEnding::Lf => "LF",
            honcho_core::LineEnding::CrLf => "CRLF",
        };
        let right = format!("Spaces: {}    UTF-8    {ending}    SQL", ed.tab_size);
        div()
            .flex()
            .flex_none()
            .justify_between()
            .items_center()
            .h(px(24.))
            .px_3()
            .bg(Theme::panel())
            .border_t_1()
            .border_color(Theme::border())
            .text_color(Theme::muted())
            .text_size(px(11.5))
            .child(left)
            .child(right)
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (title, dirty) = {
            let e = self.active_editor().read(cx);
            (e.title(), e.is_dirty())
        };
        let full_title = format!("{title}{} — sql-honcho", if dirty { " ●" } else { "" });
        window.set_window_title(&full_title);
        let client_decorations = matches!(window.window_decorations(), Decorations::Client { .. });

        div()
            .relative()
            .when(client_decorations, |d| {
                d.child(resize_cursor_overlay())
                    .on_mouse_down(MouseButton::Left, |e, window, _| {
                        let size = window.window_bounds().get_bounds().size;
                        if let Some(edge) = resize_edge(e.position, size) {
                            window.start_window_resize(edge);
                        }
                    })
                    .child(self.render_titlebar(full_title.clone(), cx))
            })
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(Theme::bg())
            .text_color(Theme::text())
            .font_family(theme::font_family(cx))
            .text_size(px(12.))
            .on_action(cx.listener(Self::new_file))
            .on_action(cx.listener(Self::open_file))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::save_as))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::quit))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| this.cycle_tab(1, window, cx)))
            .on_action(cx.listener(|this, _: &PrevTab, window, cx| this.cycle_tab(-1, window, cx)))
            .on_action(cx.listener(|this, _: &find::Find, window, cx| {
                this.find.update(cx, |f, cx| f.show(false, window, cx))
            }))
            .on_action(cx.listener(|this, _: &find::FindReplace, window, cx| {
                this.find.update(cx, |f, cx| f.show(true, window, cx))
            }))
            .on_action(cx.listener(|this, _: &find::FindNext, window, cx| {
                this.find.update(cx, |f, cx| f.next(true, window, cx))
            }))
            .on_action(cx.listener(|this, _: &find::FindPrev, window, cx| {
                this.find.update(cx, |f, cx| f.next(false, window, cx))
            }))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom(Some(1.), cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom(Some(-1.), cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, _, cx| this.zoom(None, cx)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.open_paths(paths.paths().to_vec(), window, cx)
            }))
            .child(self.render_tabs(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(self.active_editor().clone())
                    .when(self.find.read(cx).open, |d| {
                        d.child(
                            div()
                                .absolute()
                                .top(px(6.))
                                .right(px(22.))
                                .child(self.find.clone()),
                        )
                    }),
            )
            .child(self.render_status(cx))
    }
}

const RESIZE_BORDER: Pixels = px(5.);

fn resize_edge(pos: Point<Pixels>, size: Size<Pixels>) -> Option<ResizeEdge> {
    let b = RESIZE_BORDER;
    let (left, right) = (pos.x < b, pos.x > size.width - b);
    let (top, bottom) = (pos.y < b, pos.y > size.height - b);
    Some(match (top, bottom, left, right) {
        (true, _, true, _) => ResizeEdge::TopLeft,
        (true, _, _, true) => ResizeEdge::TopRight,
        (_, true, true, _) => ResizeEdge::BottomLeft,
        (_, true, _, true) => ResizeEdge::BottomRight,
        (true, ..) => ResizeEdge::Top,
        (_, true, ..) => ResizeEdge::Bottom,
        (_, _, true, _) => ResizeEdge::Left,
        (_, _, _, true) => ResizeEdge::Right,
        _ => return None,
    })
}

/// Shows resize cursors along the window edges.
fn resize_cursor_overlay() -> impl IntoElement {
    canvas(
        |_, window, _| {
            let size = window.window_bounds().get_bounds().size;
            window.insert_hitbox(
                Bounds::new(point(px(0.), px(0.)), size),
                HitboxBehavior::Normal,
            )
        },
        |_, hitbox, window, _| {
            let size = window.window_bounds().get_bounds().size;
            let Some(edge) = resize_edge(window.mouse_position(), size) else {
                return;
            };
            let style = match edge {
                ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
                ResizeEdge::Left | ResizeEdge::Right => CursorStyle::ResizeLeftRight,
                ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorStyle::ResizeUpLeftDownRight,
                ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorStyle::ResizeUpRightDownLeft,
            };
            window.set_cursor_style(style, &hitbox);
        },
    )
    .absolute()
    .size_full()
}

#[cfg(test)]
mod tests {
    use gpui::size;

    use super::*;

    #[test]
    fn resize_edges() {
        let s = size(px(800.), px(600.));
        assert_eq!(
            resize_edge(point(px(2.), px(2.)), s),
            Some(ResizeEdge::TopLeft)
        );
        assert_eq!(
            resize_edge(point(px(400.), px(598.)), s),
            Some(ResizeEdge::Bottom)
        );
        assert_eq!(
            resize_edge(point(px(799.), px(300.)), s),
            Some(ResizeEdge::Right)
        );
        assert_eq!(resize_edge(point(px(400.), px(300.)), s), None);
    }
}
