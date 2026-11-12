//! The sidebar's Project tab: a folder of SQL files compared with its
//! database, with pull and apply.

use std::path::PathBuf;

use gpui::{
    anchored, deferred, div, prelude::*, px, App, Context, EventEmitter, FocusHandle, Focusable,
    MouseButton, Pixels, Point, PromptLevel, Task, Window,
};
use honcho_config::{ColorTag, SavedConnection};
use honcho_db::project::{self, Entry, Project, Status};
use honcho_db::{DbError, ObjectKind, ObjectRef, Session};

use crate::connections::{self, Connections};
use crate::editor_view;
use crate::theme::{self, Theme};

pub enum ProjectEvent {
    OpenFile {
        path: PathBuf,
        saved: Box<SavedConnection>,
        database: String,
        /// The object and its server definition, when it's on the server.
        origin: Option<Box<(ObjectRef, String)>>,
    },
    Compare(usize),
    Close,
}

enum State {
    Idle,
    Busy(&'static str),
    Failed(String),
}

#[derive(Clone, Copy)]
enum MenuAction {
    Open,
    Compare,
    Pull,
    Apply,
}

pub struct ProjectPane {
    focus: FocusHandle,
    pub project: Project,
    session: Option<Session>,
    pub entries: Vec<Entry>,
    state: State,
    /// What the last pull or apply did, and whether anything failed.
    outcome: Option<(String, bool)>,
    selected: Option<usize>,
    menu: Option<(usize, Point<Pixels>)>,
    /// The entries are from this session's comparison, not the last one
    /// saved; only then can they be pulled or applied.
    fresh: bool,
}

impl EventEmitter<ProjectEvent> for ProjectPane {}

fn label(status: Status) -> (&'static str, gpui::Hsla) {
    match status {
        Status::Same => ("", Theme::muted()),
        Status::Edited => ("edited", Theme::accent()),
        Status::ServerChanged => ("changed on server", Theme::warning()),
        Status::BothChanged => ("changed on both", Theme::error()),
        Status::Differs => ("differs", Theme::warning()),
        Status::FileOnly => ("not on server", Theme::accent()),
        Status::ServerOnly => ("not in folder", Theme::muted()),
    }
}

fn heading(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Function => "Functions",
        ObjectKind::View => "Views",
        ObjectKind::Procedure => "Procedures",
        _ => "Triggers",
    }
}

impl ProjectPane {
    pub fn new(project: Project, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            focus: cx.focus_handle(),
            project,
            session: None,
            entries: Vec::new(),
            state: State::Idle,
            outcome: None,
            selected: None,
            menu: None,
            fresh: false,
        };
        // what it was last time, while it's compared again
        if let Some(entries) = this.project.last_status() {
            this.entries = entries;
        }
        this.refresh(cx);
        this
    }

    /// The connection and database, once the project has connected.
    pub fn open_target(&self, cx: &App) -> Option<(SavedConnection, String)> {
        self.session.as_ref()?;
        Some((self.saved(cx)?, self.project.manifest.database.clone()))
    }

    /// The saved connection the manifest names.
    pub fn saved(&self, cx: &App) -> Option<SavedConnection> {
        let name = &self.project.manifest.connection;
        cx.global::<Connections>()
            .store
            .connections()
            .iter()
            .find(|c| &c.name == name)
            .cloned()
    }

    fn session(&self, cx: &App) -> Task<Result<Session, DbError>> {
        if let Some(s) = &self.session {
            return Task::ready(Ok(s.clone()));
        }
        let Some(saved) = self.saved(cx) else {
            return Task::ready(Err(DbError::Other(format!(
                "There's no saved connection named \"{}\". Add one, or rename yours to match sql-honcho.json.",
                self.project.manifest.connection
            ))));
        };
        connections::open_session(saved, Some(self.project.manifest.database.clone()), cx)
    }

    pub fn is_busy(&self) -> bool {
        matches!(self.state, State::Busy(_))
    }

    /// Compares the folder with the database again.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        self.state = State::Busy("Comparing…");
        cx.notify();
        let project = self.project.clone();
        cx.spawn(async move |this, cx| {
            let mut retried = false;
            loop {
                let Ok(opening) = this.update(cx, |this, cx| this.session(cx)) else {
                    return;
                };
                let result = match opening.await {
                    Ok(session) => {
                        this.update(cx, |this, _| this.session = Some(session.clone()))
                            .ok();
                        project::status(&session, &project).await
                    }
                    Err(e) => Err(e),
                };
                // a session that dropped while idle gets one more go
                if matches!(&result, Err(e) if e.is_fatal()) && !retried {
                    retried = true;
                    this.update(cx, |this, _| this.session = None).ok();
                    continue;
                }
                this.update(cx, |this, cx| {
                    match result {
                        Ok(entries) => {
                            let project = this.project.clone();
                            let saved = entries.clone();
                            cx.background_spawn(async move {
                                let _ = project.save_status(&saved);
                            })
                            .detach();
                            this.entries = entries;
                            this.fresh = true;
                            this.state = State::Idle;
                        }
                        Err(e) => {
                            this.session = None;
                            this.state = State::Failed(e.to_string());
                        }
                    }
                    this.selected = this.selected.filter(|&i| i < this.entries.len());
                    cx.notify();
                })
                .ok();
                return;
            }
        })
        .detach();
    }

    /// Writes the server's version of these entries into their files.
    pub fn pull(&mut self, which: Vec<usize>, cx: &mut Context<Self>) {
        if !self.fresh {
            return;
        }
        let driver = self
            .saved(cx)
            .map(|s| s.driver)
            .unwrap_or(honcho_db::DriverKind::MySql);
        let mut failed = Vec::new();
        let mut done = 0;
        for ix in which {
            let Some(e) = self.entries.get(ix) else {
                continue;
            };
            match project::pull(&self.project, driver, e) {
                Ok(()) => done += 1,
                Err(err) => failed.push(format!("{}: {err}", e.object.name)),
            }
        }
        self.outcome = Some(summary("Pulled", done, &failed));
        self.refresh(cx);
    }

    /// Runs these entries' files against the database, in order, carrying
    /// on past failures.
    pub fn apply(&mut self, which: Vec<usize>, cx: &mut Context<Self>) {
        if !self.fresh {
            return;
        }
        let Some(saved) = self.saved(cx) else {
            return;
        };
        let entries: Vec<Entry> = which
            .iter()
            .filter_map(|&i| self.entries.get(i).cloned())
            .collect();
        let project = self.project.clone();
        self.state = State::Busy("Applying…");
        cx.notify();
        cx.spawn(async move |this, cx| {
            let Ok(opening) = this.update(cx, |this, cx| this.session(cx)) else {
                return;
            };
            let (mut done, mut failed) = (0, Vec::new());
            match opening.await {
                Ok(session) => {
                    for e in &entries {
                        match project::apply(&session, &project, saved.driver, e).await {
                            Ok(_) => done += 1,
                            Err(err) => failed.push(format!("{}: {err}", e.object.name)),
                        }
                    }
                    this.update(cx, |this, _| this.session = Some(session)).ok();
                }
                Err(e) => failed.push(e.to_string()),
            }
            this.update(cx, |this, cx| {
                this.outcome = Some(summary("Applied", done, &failed));
                this.state = State::Idle;
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    fn indices(&self, statuses: &[Status]) -> Vec<usize> {
        (0..self.entries.len())
            .filter(|&i| statuses.contains(&self.entries[i].status))
            .collect()
    }

    fn names(&self, which: &[usize]) -> String {
        let mut names: Vec<&str> = which
            .iter()
            .map(|&i| self.entries[i].object.name.as_str())
            .collect();
        if names.len() > 12 {
            let more = names.len() - 12;
            names.truncate(12);
            return format!("{}, and {more} more", names.join(", "));
        }
        names.join(", ")
    }

    /// Pulls whatever changed on the server. Files edited here are only
    /// replaced if the user says so.
    pub fn pull_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let safe = self.indices(&[Status::ServerChanged, Status::ServerOnly]);
        let risky = self.indices(&[Status::BothChanged, Status::Differs]);
        if risky.is_empty() {
            if safe.is_empty() {
                self.outcome = Some(("Nothing to pull.".into(), false));
                cx.notify();
            } else {
                self.pull(safe, cx);
            }
            return;
        }
        let message = format!(
            "{} differ{} from the server and may have edits of yours.",
            self.names(&risky),
            if risky.len() == 1 { "s" } else { "" }
        );
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            Some("Pulling them replaces the files with the server's version."),
            &["Cancel", "Skip those", "Pull them too"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            let which = match answer.await {
                Ok(1) => safe,
                Ok(2) => safe.into_iter().chain(risky).collect(),
                _ => return,
            };
            this.update(cx, |this, cx| this.pull(which, cx)).ok();
        })
        .detach();
    }

    /// Applies the files edited here, after saying what will change where.
    pub fn apply_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mine = self.indices(&[Status::Edited, Status::FileOnly]);
        let risky = self.indices(&[Status::BothChanged, Status::Differs]);
        if mine.is_empty() && risky.is_empty() {
            self.outcome = Some(("Nothing to apply.".into(), false));
            cx.notify();
            return;
        }
        let target = self.target(cx);
        let mut buttons = vec!["Cancel"];
        let message;
        let detail;
        if risky.is_empty() {
            message = format!("Apply {} to {target}?", self.names(&mine));
            detail = None;
            buttons.push("Apply");
        } else {
            message = format!(
                "{} also changed on the server, or can't be told apart.",
                self.names(&risky)
            );
            detail = Some(format!(
                "Applying them replaces the server's version. Edited here: {}.",
                if mine.is_empty() {
                    "none".into()
                } else {
                    self.names(&mine)
                }
            ));
            if !mine.is_empty() {
                buttons.push("Apply mine only");
            }
            buttons.push("Apply all");
        }
        let all_index = buttons.len() - 1;
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            detail.as_deref(),
            &buttons,
            cx,
        );
        cx.spawn(async move |this, cx| {
            let which = match answer.await {
                Ok(i) if i == all_index => {
                    let mut all: Vec<usize> = mine.into_iter().chain(risky).collect();
                    // keep the dependency order the list is in
                    all.sort();
                    all
                }
                Ok(1) => mine,
                _ => return,
            };
            this.update(cx, |this, cx| this.apply(which, cx)).ok();
        })
        .detach();
    }

    /// Asks before applying one file.
    pub fn apply_one(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(e) = self.entries.get(ix) else {
            return;
        };
        if e.file.is_none() {
            return;
        }
        let mut message = format!("Apply {} to {}?", e.object.name, self.target(cx));
        if matches!(e.status, Status::BothChanged | Status::ServerChanged) {
            message
                .push_str(" It changed on the server since it was pulled, and this replaces that.");
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            None,
            &["Cancel", "Apply"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await == Ok(1) {
                this.update(cx, |this, cx| this.apply(vec![ix], cx)).ok();
            }
        })
        .detach();
    }

    fn target(&self, cx: &App) -> String {
        let db = &self.project.manifest.database;
        match self.saved(cx) {
            Some(s) if s.color == Some(ColorTag::Red) => {
                format!("{db} on {} (marked production)", s.name)
            }
            Some(s) => format!("{db} on {}", s.name),
            None => db.clone(),
        }
    }

    pub fn open(&mut self, ix: usize, cx: &mut Context<Self>) {
        let (Some(e), Some(saved)) = (self.entries.get(ix), self.saved(cx)) else {
            return;
        };
        if e.file.is_none() {
            // nothing to open yet: show what pulling would give
            cx.emit(ProjectEvent::Compare(ix));
            return;
        }
        cx.emit(ProjectEvent::OpenFile {
            path: self.project.root.join(&e.path),
            saved: Box::new(saved),
            database: self.project.manifest.database.clone(),
            origin: e.server.clone().map(|d| Box::new((e.object.clone(), d))),
        });
    }

    fn activate(&mut self, ix: usize, cx: &mut Context<Self>) {
        match self.entries[ix].status {
            Status::Same | Status::FileOnly => self.open(ix, cx),
            _ => cx.emit(ProjectEvent::Compare(ix)),
        }
    }

    fn menu_action(&mut self, action: MenuAction, window: &mut Window, cx: &mut Context<Self>) {
        let Some((ix, _)) = self.menu.take() else {
            return;
        };
        match action {
            MenuAction::Open => self.open(ix, cx),
            MenuAction::Compare => cx.emit(ProjectEvent::Compare(ix)),
            MenuAction::Pull => self.pull(vec![ix], cx),
            MenuAction::Apply => self.apply_one(ix, window, cx),
        }
        cx.notify();
    }

    #[cfg(test)]
    pub fn is_fresh(&self) -> bool {
        self.fresh
    }

    #[cfg(test)]
    pub fn outcome(&self) -> Option<&str> {
        self.outcome.as_ref().map(|(s, _)| s.as_str())
    }

    #[cfg(test)]
    pub fn failure(&self) -> Option<&str> {
        match &self.state {
            State::Failed(e) => Some(e),
            _ => None,
        }
    }
}

fn summary(verb: &str, done: usize, failed: &[String]) -> (String, bool) {
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    if failed.is_empty() {
        return (format!("{verb} {done} object{}.", plural(done)), false);
    }
    (
        format!(
            "{verb} {done}, {} failed:\n{}",
            failed.len(),
            failed.join("\n")
        ),
        true,
    )
}

impl Focusable for ProjectPane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn button(id: &'static str, text: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py_0p5()
        .rounded_sm()
        .border_1()
        .border_color(Theme::border())
        .cursor_pointer()
        .hover(|d| d.bg(Theme::border()))
        .child(text)
}

impl Render for ProjectPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let folder = self
            .project
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let manifest = &self.project.manifest;
        let busy = self.is_busy();

        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        let mut kind = None;
        for (ix, e) in self.entries.iter().enumerate() {
            if kind != Some(e.object.kind) {
                kind = Some(e.object.kind);
                rows.push(
                    div()
                        .px_2()
                        .pt_2()
                        .pb_0p5()
                        .text_color(Theme::muted())
                        .text_size(px(11.))
                        .child(heading(e.object.kind))
                        .into_any_element(),
                );
            }
            let (status, color) = label(e.status);
            let selected = self.selected == Some(ix);
            rows.push(
                div()
                    .id(("project-row", ix))
                    .h(px(22.))
                    .px_2()
                    .pl_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .whitespace_nowrap()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(Theme::accent_dim()))
                    .hover(|d| d.bg(Theme::border()))
                    .child(
                        div()
                            .overflow_hidden()
                            .when(e.status == Status::ServerOnly, |d| {
                                d.text_color(Theme::muted())
                            })
                            .child(e.object.name.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.))
                            .text_color(color)
                            .child(status),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &gpui::MouseDownEvent, window, cx| {
                            window.focus(&this.focus);
                            this.selected = Some(ix);
                            this.menu = None;
                            if ev.click_count == 2 {
                                this.activate(ix, cx);
                            }
                            cx.notify();
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, ev: &gpui::MouseDownEvent, _, cx| {
                            this.selected = Some(ix);
                            this.menu = Some((ix, ev.position));
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
            );
        }

        let menu = self.menu.and_then(|(ix, position)| {
            let e = self.entries.get(ix)?;
            let mut actions = Vec::new();
            if e.file.is_some() {
                actions.push(("Open file", MenuAction::Open));
            }
            actions.push(("Compare with server", MenuAction::Compare));
            let fresh = self.fresh && !busy;
            if e.server.is_some() && fresh {
                actions.push(("Pull from server", MenuAction::Pull));
            }
            if e.file.is_some() && fresh {
                actions.push(("Apply to server…", MenuAction::Apply));
            }
            Some(
                deferred(
                    anchored().position(position).child(
                        div()
                            .occlude()
                            .min_w(px(180.))
                            .py_1()
                            .bg(Theme::panel())
                            .border_1()
                            .border_color(Theme::border())
                            .rounded_md()
                            .shadow_lg()
                            .children(actions.into_iter().enumerate().map(
                                |(i, (text, action))| {
                                    div()
                                        .id(("project-menu", i))
                                        .px_3()
                                        .py_0p5()
                                        .cursor_pointer()
                                        .hover(|d| d.bg(Theme::accent_dim()))
                                        .child(text)
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.menu_action(action, window, cx)
                                        }))
                                },
                            )),
                    ),
                )
                .with_priority(1),
            )
        });

        let state = match &self.state {
            State::Idle if self.entries.is_empty() => Some((
                "Nothing here yet: no routines, triggers or views on the server or in the folder."
                    .to_string(),
                false,
            )),
            State::Idle => None,
            State::Busy(what) => Some((what.to_string(), false)),
            State::Failed(e) => Some((e.clone(), true)),
        };

        div()
            .key_context("ProjectPane")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(Theme::panel())
            .text_color(Theme::text())
            .font_family(theme::font_family(cx))
            .text_size(px(12.))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                if this.menu.take().is_some() {
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &editor_view::Cancel, _, cx| {
                if this.menu.take().is_some() {
                    cx.notify();
                }
            }))
            .child(
                div()
                    .flex_none()
                    .px_2()
                    .py_1p5()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .border_b_1()
                    .border_color(Theme::border())
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .child(div().overflow_hidden().child(folder))
                            .child(
                                div()
                                    .id("project-close")
                                    .px_1()
                                    .cursor_pointer()
                                    .text_color(Theme::muted())
                                    .hover(|d| d.text_color(Theme::text()))
                                    .child("×")
                                    .on_click(
                                        cx.listener(|_, _, _, cx| cx.emit(ProjectEvent::Close)),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .text_color(Theme::muted())
                            .text_size(px(11.))
                            .child(format!("{} · {}", manifest.connection, manifest.database)),
                    )
                    .child(div().flex().gap_1().when(!busy, |d| {
                        d.child(
                            button("project-refresh", "Refresh")
                                .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                        )
                        .when(self.fresh, |d| {
                            d.child(button("project-pull", "Pull").on_click(
                                cx.listener(|this, _, window, cx| this.pull_changes(window, cx)),
                            ))
                            .child(
                                button("project-apply", "Apply").on_click(cx.listener(
                                    |this, _, window, cx| this.apply_changes(window, cx),
                                )),
                            )
                        })
                    })),
            )
            .children(state.map(|(text, err)| {
                div()
                    .px_2()
                    .py_1()
                    .text_color(if err { Theme::error() } else { Theme::muted() })
                    .child(text)
            }))
            .child(
                div()
                    .id("project-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows),
            )
            .children(self.outcome.clone().map(|(text, err)| {
                div()
                    .flex_none()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(Theme::border())
                    .text_size(px(11.))
                    .text_color(if err { Theme::error() } else { Theme::muted() })
                    .child(text)
            }))
            .children(menu)
    }
}
