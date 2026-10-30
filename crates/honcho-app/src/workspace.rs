use std::path::{Path, PathBuf};

use gpui::{
    actions, div, prelude::*, px, App, Context, Entity, ExternalPaths, FocusHandle, Focusable,
    KeyBinding, MouseButton, PromptLevel, SharedString, Subscription, Task, Window,
};

use crate::editor_view::EditorView;
use crate::find::{self, FindBar};
use crate::theme::{Theme, FONT_FAMILY, FONT_SIZE};
use crate::FontSize;

actions!(
    workspace,
    [
        NewFile,
        OpenFile,
        Save,
        SaveAs,
        CloseTab,
        NextTab,
        PrevTab,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        Quit,
    ]
);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-n", NewFile, None),
        KeyBinding::new("cmd-o", OpenFile, None),
        KeyBinding::new("cmd-s", Save, None),
        KeyBinding::new("cmd-shift-s", SaveAs, None),
        KeyBinding::new("cmd-w", CloseTab, None),
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("ctrl-shift-tab", PrevTab, None),
        KeyBinding::new("cmd-shift-]", NextTab, None),
        KeyBinding::new("cmd-shift-[", PrevTab, None),
        KeyBinding::new("cmd-=", ZoomIn, None),
        KeyBinding::new("cmd-+", ZoomIn, None),
        KeyBinding::new("cmd--", ZoomOut, None),
        KeyBinding::new("cmd-0", ZoomReset, None),
        KeyBinding::new("cmd-q", Quit, None),
    ]);
}

pub struct Workspace {
    focus_handle: FocusHandle,
    tabs: Vec<Entity<EditorView>>,
    active: usize,
    next_untitled: usize,
    find: Entity<FindBar>,
    subscriptions: Vec<Subscription>,
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
            find,
        };
        this.new_tab(String::new(), None, window, cx);
        this.open_paths(paths, window, cx);
        this
    }

    fn active_editor(&self) -> &Entity<EditorView> {
        &self.tabs[self.active]
    }

    fn new_tab(&mut self, text: String, path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let untitled = if path.is_none() {
            self.next_untitled += 1;
            self.next_untitled - 1
        } else {
            0
        };
        let editor = cx.new(|cx| EditorView::new(&text, path, untitled, cx));
        self.subscriptions.push(cx.observe(&editor, |_, _, cx| cx.notify()));
        self.tabs.push(editor);
        self.activate(self.tabs.len() - 1, window, cx);
    }

    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.active = ix.min(self.tabs.len() - 1);
        let editor = self.active_editor().clone();
        self.find.update(cx, |f, cx| f.set_target(editor.clone(), cx));
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
                    drop(window.prompt(PromptLevel::Critical, &msg, Some(&err.to_string()), &["OK"], cx));
                    continue;
                }
            };
            if self.active_editor().read(cx).is_pristine() {
                let ix = self.active;
                self.tabs[ix] = cx.new(|cx| EditorView::new(&text, Some(path), 0, cx));
                self.subscriptions.push(cx.observe(&self.tabs[ix], |_, _, cx| cx.notify()));
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
                this.update_in(cx, |this, window, cx| this.open_paths(paths, window, cx)).ok();
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
    fn save_tab(&mut self, ix: usize, save_as: bool, window: &mut Window, cx: &mut Context<Self>) -> Task<bool> {
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
                .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from("/"));
            let name = if title.ends_with(".sql") { title.clone() } else { format!("{title}.sql") };
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
                        window.prompt(PromptLevel::Critical, &msg, Some(&err.to_string()), &["OK"], cx)
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
        let current = cx.global::<FontSize>().0;
        let next = match size {
            Some(delta) => (current + px(delta)).max(px(8.)).min(px(40.)),
            None => px(FONT_SIZE),
        };
        cx.set_global(FontSize(next));
        cx.notify();
    }

    fn quit(&mut self, _: &Quit, window: &mut Window, cx: &mut Context<Self>) {
        let dirty = self.tabs.iter().filter(|t| t.read(cx).is_dirty()).count();
        if dirty == 0 {
            cx.quit();
            return;
        }
        let msg = format!(
            "{dirty} file{} not saved. Quit anyway?",
            if dirty == 1 { " is" } else { "s are" }
        );
        let answer = window.prompt(PromptLevel::Warning, &msg, None, &["Quit", "Cancel"], cx);
        cx.spawn(async move |_, cx| {
            if answer.await == Ok(0) {
                cx.update(|cx| cx.quit()).ok();
            }
        })
        .detach();
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
                    .border_color(if active { Theme::accent() } else { Theme::panel() })
                    .when(active, |d| d.bg(Theme::tab_active()))
                    .text_color(if active { Theme::text() } else { Theme::muted() })
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
        window.set_window_title(&format!("{title}{} — sql-honcho", if dirty { " ●" } else { "" }));

        div()
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(Theme::bg())
            .text_color(Theme::text())
            .font_family(FONT_FAMILY)
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
                        d.child(div().absolute().top(px(6.)).right(px(22.)).child(self.find.clone()))
                    }),
            )
            .child(self.render_status(cx))
    }
}
