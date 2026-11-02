use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui::{
    actions, canvas, div, point, prelude::*, px, App, Bounds, Context, CursorStyle, Decorations,
    Entity, EntityId, ExternalPaths, FocusHandle, Focusable, HitboxBehavior, MouseButton, Pixels,
    Point, PromptLevel, ResizeEdge, SharedString, Size, Subscription, Task, Window,
};

use crate::browser::{self, BrowserEvent, ObjectBrowser};
use crate::connection_dialog::{ConnectionDialog, DialogEvent};
use crate::connections;
use crate::diff_view::{DiffEvent, DiffView};
use crate::editor_view::EditorView;
use crate::find::{self, FindBar};
use crate::project_pane::{ProjectEvent, ProjectPane};
use crate::query::{Origin, QueryTab, TabEvent};
use crate::routine_form::{FormEvent, RoutineForm};
use crate::theme::{self, EditorFont, Theme, FONT_SIZE};
use honcho_config::SavedConnection;
use honcho_db::project::{Manifest, Project};
use honcho_db::{ObjectRef, RoutineParam};

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
        ManageConnections,
        OpenProject,
    ]
);

#[derive(Clone, Copy, PartialEq)]
enum Sidebar {
    Database,
    Project,
}

pub struct Workspace {
    focus_handle: FocusHandle,
    tabs: Vec<Entity<EditorView>>,
    active: usize,
    next_untitled: usize,
    find: Entity<FindBar>,
    subscriptions: Vec<Subscription>,
    /// The user already agreed to lose unsaved changes on close.
    discard_confirmed: bool,
    connection_dialog: Option<Entity<ConnectionDialog>>,
    routine_form: Option<Entity<RoutineForm>>,
    browser: Entity<ObjectBrowser>,
    project: Option<Entity<ProjectPane>>,
    diff: Option<Entity<DiffView>>,
    sidebar: Sidebar,
    show_browser: bool,
    /// Pointer x and pane width when dragging the browser's edge began.
    browser_drag: Option<(Pixels, Pixels)>,
    /// Keyed by the tab's editor.
    query_tabs: HashMap<EntityId, Entity<QueryTab>>,
}

impl Workspace {
    pub fn new(paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let find = cx.new(FindBar::new);
        let browser = cx.new(|cx| {
            let mut b = ObjectBrowser::new(cx);
            b.restore_expanded(cx);
            b
        });
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            tabs: Vec::new(),
            active: 0,
            next_untitled: 1,
            subscriptions: vec![cx.observe(&find, |_, _, cx| cx.notify())],
            discard_confirmed: false,
            connection_dialog: None,
            routine_form: None,
            show_browser: true,
            project: None,
            diff: None,
            sidebar: Sidebar::Database,
            browser_drag: None,
            browser,
            query_tabs: HashMap::new(),
            find,
        };
        this.subscribe_browser(window, cx);
        this.new_tab(String::new(), None, window, cx);
        if let Some(root) = last_project(cx) {
            if let Ok(project) = Project::open(&root) {
                this.show_project(project, false, window, cx);
            }
        }
        this.open_paths(paths, window, cx);

        let handle = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            handle
                .update(cx, |this, cx| this.should_close(window, cx))
                .unwrap_or(true)
        });
        this
    }

    #[cfg(test)]
    pub(crate) fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    #[cfg(test)]
    pub(crate) fn find_bar(&self) -> &Entity<FindBar> {
        &self.find
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
    pub(crate) fn should_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.discard_confirmed || self.dirty_count(cx) == 0 {
            return true;
        }
        self.confirm_discard("Close", window, cx, |_, window, _| window.remove_window());
        false
    }

    pub(crate) fn active_editor(&self) -> &Entity<EditorView> {
        &self.tabs[self.active]
    }

    pub(crate) fn active_query_tab(&self) -> &Entity<QueryTab> {
        &self.query_tabs[&self.active_editor().entity_id()]
    }

    #[cfg(test)]
    pub(crate) fn active_connection(&self, cx: &App) -> Option<connections::ActiveConnection> {
        self.active_query_tab().read(cx).connection.clone()
    }

    #[cfg(test)]
    pub(crate) fn connection_dialog(&self) -> Option<&Entity<ConnectionDialog>> {
        self.connection_dialog.as_ref()
    }

    fn subscribe_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let browser = self.browser.clone();
        self.subscriptions.push(cx.subscribe_in(
            &browser,
            window,
            |this, _, event: &BrowserEvent, window, cx| match event {
                BrowserEvent::Open {
                    saved,
                    database,
                    text,
                    run,
                    origin,
                } => {
                    this.new_tab(text.clone(), None, window, cx);
                    let (saved, database, run) = ((**saved).clone(), database.clone(), *run);
                    let origin = origin.as_ref().map(|o| Origin {
                        object: o.0.clone(),
                        definition: o.1.clone(),
                    });
                    this.active_query_tab().clone().update(cx, |q, cx| {
                        q.origin = origin;
                        q.open_saved(saved, database, run, cx)
                    });
                }
                BrowserEvent::RunRoutine {
                    saved,
                    object,
                    params,
                } => this.ask_routine(
                    (**saved).clone(),
                    object.clone(),
                    params.clone(),
                    window,
                    cx,
                ),
                BrowserEvent::Insert(text) => {
                    let editor = this.active_editor().clone();
                    editor.update(cx, |e, cx| {
                        e.editor.insert(text);
                        e.request_autoscroll(cx);
                    });
                    window.focus(&editor.focus_handle(cx));
                }
                BrowserEvent::SaveProject { saved, database } => {
                    this.save_project(saved.name.clone(), database.clone(), window, cx)
                }
            },
        ));
    }

    fn open_project(&mut self, _: &OpenProject, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open project".into()),
        });
        cx.spawn_in(window, async move |this, cx| match rx.await {
            Ok(Ok(Some(paths))) => {
                let Some(root) = paths.into_iter().next() else {
                    return;
                };
                match Project::open(&root) {
                    Ok(project) => {
                        this.update_in(cx, |this, window, cx| {
                            this.show_project(project, true, window, cx)
                        })
                        .ok();
                    }
                    Err(e) => {
                        let detail = format!(
                            "{e}\n\nTo start one, right-click a database in the browser and pick Save to project folder…"
                        );
                        let answer = this.update_in(cx, |_, window, cx| {
                            window.prompt(
                                PromptLevel::Warning,
                                "This folder isn't a sql-honcho project.",
                                Some(&detail),
                                &["OK"],
                                cx,
                            )
                        });
                        if let Ok(answer) = answer {
                            answer.await.ok();
                        }
                    }
                }
            }
            Ok(Err(err)) => dialog_failed(err, cx).await,
            _ => {}
        })
        .detach();
    }

    /// Makes a folder the project for this connection and database and
    /// pulls everything into it.
    fn save_project(
        &mut self,
        connection: String,
        database: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Use folder".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let root = match rx.await {
                Ok(Ok(Some(paths))) => match paths.into_iter().next() {
                    Some(root) => root,
                    None => return,
                },
                Ok(Err(err)) => return dialog_failed(err, cx).await,
                _ => return,
            };
            let manifest = Manifest {
                connection,
                database,
            };
            // a folder that's already a project for something else is
            // only repointed if the user says so
            if let Ok(existing) = Project::open(&root) {
                if existing.manifest != manifest {
                    let message = format!(
                        "This folder is the project for {} on {}.",
                        existing.manifest.database, existing.manifest.connection
                    );
                    let Ok(answer) = this.update_in(cx, |_, window, cx| {
                        window.prompt(
                            PromptLevel::Warning,
                            &message,
                            Some("Pointing it at another database makes every file compare against that one."),
                            &["Cancel", "Point it here"],
                            cx,
                        )
                    }) else {
                        return;
                    };
                    if answer.await != Ok(1) {
                        return;
                    }
                }
            }
            match Project::init(&root, manifest) {
                Ok(project) => {
                    this.update_in(cx, |this, window, cx| {
                        this.show_project(project, true, window, cx);
                        if let Some(pane) = &this.project {
                            // pull what isn't in the folder yet once compared
                            let pane = pane.clone();
                            this.subscriptions.push(cx.observe(&pane, {
                                let mut pulled = false;
                                move |_, pane, cx| {
                                    if pulled || pane.read(cx).is_busy() {
                                        return;
                                    }
                                    pulled = true;
                                    pane.update(cx, |p, cx| {
                                        let which = (0..p.entries.len())
                                            .filter(|&i| {
                                                p.entries[i].status
                                                    == honcho_db::project::Status::ServerOnly
                                            })
                                            .collect();
                                        p.pull(which, cx)
                                    });
                                }
                            }));
                        }
                    })
                    .ok();
                }
                Err(e) => dialog_failed(e, cx).await,
            }
        })
        .detach();
    }

    fn show_project(
        &mut self,
        project: Project,
        remember: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if remember {
            remember_project(&project.root, cx);
        }
        let pane = cx.new(|cx| ProjectPane::new(project, cx));
        self.subscriptions
            .push(cx.observe(&pane, |_, _, cx| cx.notify()));
        self.subscriptions.push(cx.subscribe_in(
            &pane,
            window,
            |this, pane, event: &ProjectEvent, window, cx| match event {
                ProjectEvent::OpenFile {
                    path,
                    saved,
                    database,
                    origin,
                } => {
                    this.open_paths(vec![path.clone()], window, cx);
                    let q = this.active_query_tab().clone();
                    let (saved, database) = ((**saved).clone(), database.clone());
                    let origin = origin.as_ref().map(|o| Origin {
                        object: o.0.clone(),
                        definition: o.1.clone(),
                    });
                    q.update(cx, |q, cx| {
                        q.origin = origin;
                        if q.connection.is_none() {
                            q.open_saved(saved, Some(database), false, cx);
                        }
                        cx.notify();
                    });
                }
                ProjectEvent::Compare(ix) => {
                    let diff = cx.new(|cx| DiffView::new(pane.clone(), *ix, window, cx));
                    this.subscriptions.push(cx.subscribe_in(
                        &diff,
                        window,
                        |this, _, _: &DiffEvent, window, cx| {
                            this.diff = None;
                            let editor = this.active_editor().clone();
                            window.focus(&editor.focus_handle(cx));
                            cx.notify();
                        },
                    ));
                    this.diff = Some(diff);
                    cx.notify();
                }
                ProjectEvent::Close => {
                    this.project = None;
                    this.sidebar = Sidebar::Database;
                    forget_project(cx);
                    cx.notify();
                }
            },
        ));
        self.project = Some(pane);
        self.sidebar = Sidebar::Project;
        self.show_browser = true;
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn project(&self) -> Option<&Entity<ProjectPane>> {
        self.project.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn diff(&self) -> Option<&Entity<DiffView>> {
        self.diff.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn show_project_for_test(
        &mut self,
        project: Project,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_project(project, false, window, cx);
    }

    fn ask_routine(
        &mut self,
        saved: SavedConnection,
        object: ObjectRef,
        params: Vec<RoutineParam>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let form = cx.new(|cx| RoutineForm::new(saved, object, params, window, cx));
        self.subscriptions.push(cx.subscribe_in(
            &form,
            window,
            |this, _, event: &FormEvent, window, cx| {
                if let FormEvent::Run {
                    saved,
                    database,
                    sql,
                } = event
                {
                    this.new_tab(sql.clone(), None, window, cx);
                    let (saved, database) = ((**saved).clone(), database.clone());
                    this.active_query_tab()
                        .clone()
                        .update(cx, |q, cx| q.open_saved(saved, database, true, cx));
                }
                this.routine_form = None;
                let editor = this.active_editor().clone();
                window.focus(&editor.focus_handle(cx));
                cx.notify();
            },
        ));
        self.routine_form = Some(form);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn routine_form(&self) -> Option<&Entity<RoutineForm>> {
        self.routine_form.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn browser(&self) -> &Entity<ObjectBrowser> {
        &self.browser
    }

    pub(crate) fn open_connections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dialog) = &self.connection_dialog {
            window.focus(&dialog.focus_handle(cx));
            return;
        }
        let dialog = cx.new(|cx| ConnectionDialog::new(window, cx));
        self.subscriptions.push(cx.subscribe_in(
            &dialog,
            window,
            |this, _, event: &DialogEvent, window, cx| {
                if let DialogEvent::Connected(conn) = event {
                    let conn = (**conn).clone();
                    this.active_query_tab()
                        .clone()
                        .update(cx, |q, cx| q.set_connection(conn, cx));
                }
                this.connection_dialog = None;
                // names, colors and the list itself may have changed
                this.browser.update(cx, |b, cx| b.connections_changed(cx));
                let editor = this.active_editor().clone();
                window.focus(&editor.focus_handle(cx));
                cx.notify();
            },
        ));
        self.connection_dialog = Some(dialog);
        cx.notify();
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
        let query = cx.new(|cx| QueryTab::new(editor.clone(), cx));
        self.subscriptions
            .push(cx.observe(&editor, |_, _, cx| cx.notify()));
        self.subscriptions
            .push(cx.observe(&query, |_, _, cx| cx.notify()));
        self.subscriptions.push(cx.subscribe_in(
            &query,
            window,
            |this, _, event: &TabEvent, window, cx| match event {
                TabEvent::NeedConnection => this.open_connections(window, cx),
                TabEvent::Applied { conn_id, database } => {
                    if let Some(db) = database {
                        this.browser
                            .update(cx, |b, cx| b.reload_database(conn_id, db, cx));
                    }
                }
                TabEvent::OpenScript(text) => this.new_tab(text.clone(), None, window, cx),
            },
        ));
        self.query_tabs.insert(editor.entity_id(), query);
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
                // hand the replaced tab's number back so ctrl-n starts at 1 again
                if self.tabs[ix].read(cx).untitled_index + 1 == self.next_untitled {
                    self.next_untitled -= 1;
                }
                // load into the same editor so the tab keeps its connection
                self.tabs[ix].update(cx, |e, cx| {
                    *e = EditorView::new(&text, Some(path), 0, cx);
                    cx.notify();
                });
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
        cx.spawn_in(window, async move |this, cx| match rx.await {
            Ok(Ok(Some(paths))) => {
                this.update_in(cx, |this, window, cx| this.open_paths(paths, window, cx))
                    .ok();
            }
            Ok(Err(err)) => dialog_failed(err, cx).await,
            _ => {}
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
                    Ok(Err(err)) => {
                        dialog_failed(err, cx).await;
                        return false;
                    }
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
        let editor = self.tabs.remove(ix);
        if let Some(query) = self.query_tabs.remove(&editor.entity_id()) {
            query.update(cx, |q, cx| q.disconnect(cx));
        }
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
                let query = self.query_tabs[&tab.entity_id()].read(cx);
                let color = query.color().map(connections::color);
                let running = query.is_running();
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_2()
                    .pl_3()
                    .pr_2()
                    .h_full()
                    .border_t_2()
                    .border_color(match color {
                        Some(c) => c,
                        None if active => Theme::accent(),
                        None => Theme::panel(),
                    })
                    .when(active, |d| d.bg(Theme::tab_active()))
                    // a tinted tab makes it hard to forget which server it's on
                    .when_some(color, |d, c| {
                        d.bg(c.opacity(if active { 0.18 } else { 0.1 }))
                    })
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
                    .when(running, |d| {
                        d.child(div().text_color(Theme::accent()).child("▶"))
                    })
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
            .on_action(cx.listener(|this, _: &browser::ToggleBrowser, _, cx| {
                this.show_browser = !this.show_browser;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ManageConnections, window, cx| {
                this.open_connections(window, cx)
            }))
            .on_action(cx.listener(Self::open_project))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom(Some(1.), cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom(Some(-1.), cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, _, cx| this.zoom(None, cx)))
            .on_mouse_move(cx.listener(|this, e: &gpui::MouseMoveEvent, window, cx| {
                if let Some((x, width)) = this.browser_drag {
                    // leave the editor room, and keep the tree usable
                    let max = window.viewport_size().width - px(320.);
                    let w = (width + e.position.x - x).max(px(160.)).min(max);
                    this.browser.update(cx, |b, cx| {
                        b.width = w;
                        cx.notify();
                    });
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.browser_drag.take().is_some() {
                        let w = this.browser.read(cx).width;
                        this.browser.update(cx, |b, cx| b.set_width(w, cx));
                    }
                }),
            )
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.open_paths(paths.paths().to_vec(), window, cx)
            }))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .when(self.show_browser, |d| {
                        d.child(
                            div()
                                .relative()
                                .w(self.browser.read(cx).width)
                                .flex_none()
                                .border_r_1()
                                .border_color(Theme::border())
                                .flex()
                                .flex_col()
                                .child(self.render_sidebar_tabs(cx))
                                .child(div().flex_1().min_h_0().child(
                                    match (self.sidebar, &self.project) {
                                        (Sidebar::Project, Some(p)) => p.clone().into_any_element(),
                                        (Sidebar::Project, None) => {
                                            self.render_no_project(cx).into_any_element()
                                        }
                                        _ => self.browser.clone().into_any_element(),
                                    },
                                ))
                                // the right edge drags to resize; a double
                                // click puts it back
                                .child(
                                    div()
                                        .id("browser-edge")
                                        .absolute()
                                        .top_0()
                                        .bottom_0()
                                        .right(px(-3.))
                                        .w(px(6.))
                                        .cursor(CursorStyle::ResizeLeftRight)
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, e: &gpui::MouseDownEvent, _, cx| {
                                                cx.stop_propagation();
                                                if e.click_count == 2 {
                                                    this.browser.update(cx, |b, cx| {
                                                        b.set_width(browser::DEFAULT_WIDTH, cx)
                                                    });
                                                    return;
                                                }
                                                let width = this.browser.read(cx).width;
                                                this.browser_drag = Some((e.position.x, width));
                                            }),
                                        ),
                                ),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(self.render_tabs(cx))
                            .child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    .relative()
                                    .child(self.active_query_tab().clone())
                                    .when(self.find.read(cx).open, |d| {
                                        d.child(
                                            div()
                                                .absolute()
                                                // below the tab's toolbar
                                                .top(px(36.))
                                                .right(px(22.))
                                                .child(self.find.clone()),
                                        )
                                    }),
                            ),
                    ),
            )
            .child(self.render_status(cx))
            .children(self.connection_dialog.clone())
            .children(self.routine_form.clone())
            .children(self.diff.clone())
    }
}

impl Workspace {
    fn render_sidebar_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = |id: &'static str, text: &'static str, which: Sidebar| {
            let active = self.sidebar == which;
            div()
                .id(id)
                .px_2()
                .py_1()
                .cursor_pointer()
                .border_b_2()
                .border_color(if active {
                    Theme::accent()
                } else {
                    gpui::transparent_black()
                })
                .text_color(if active {
                    Theme::text()
                } else {
                    Theme::muted()
                })
                .hover(|d| d.text_color(Theme::text()))
                .child(text)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.sidebar = which;
                    cx.notify();
                }))
        };
        div()
            .flex_none()
            .flex()
            .gap_1()
            .px_1()
            .bg(Theme::panel())
            .border_b_1()
            .border_color(Theme::border())
            .text_size(px(12.))
            .child(tab("sidebar-database", "Database", Sidebar::Database))
            .child(tab("sidebar-project", "Project", Sidebar::Project))
    }

    fn render_no_project(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .bg(Theme::panel())
            .text_color(Theme::muted())
            .text_size(px(12.))
            .child("No project open. A project is a folder of SQL files, one per routine, trigger or view, kept in step with a database.")
            .child(
                div()
                    .id("project-open")
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(Theme::border())
                    .text_color(Theme::text())
                    .cursor_pointer()
                    .hover(|d| d.bg(Theme::border()))
                    .child("Open project folder…")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_project(&OpenProject, window, cx)
                    })),
            )
            .child("To start one, right-click a database in the Database tab and pick Save to project folder….")
    }
}

fn project_state_path(cx: &App) -> PathBuf {
    cx.global::<connections::Connections>()
        .store
        .path()
        .with_file_name("project.json")
}

fn last_project(cx: &App) -> Option<PathBuf> {
    let text = std::fs::read_to_string(project_state_path(cx)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("root")?.as_str().map(PathBuf::from)
}

fn remember_project(root: &Path, cx: &App) {
    let path = project_state_path(cx);
    let text = serde_json::json!({ "root": root }).to_string();
    cx.background_spawn(async move {
        let _ = std::fs::write(path, text);
    })
    .detach();
}

fn forget_project(cx: &App) {
    let path = project_state_path(cx);
    cx.background_spawn(async move {
        let _ = std::fs::remove_file(path);
    })
    .detach();
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

/// On Linux the file dialogs go through xdg-desktop-portal, which some
/// desktops don't run; say so instead of doing nothing.
async fn dialog_failed(err: impl std::fmt::Display, cx: &mut gpui::AsyncWindowContext) {
    let detail = format!("{err}\n\nOn Linux this needs xdg-desktop-portal.");
    if let Ok(rx) = cx.update(|window, cx| {
        window.prompt(
            PromptLevel::Critical,
            "Couldn't open the file dialog",
            Some(&detail),
            &["OK"],
            cx,
        )
    }) {
        rx.await.ok();
    }
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
