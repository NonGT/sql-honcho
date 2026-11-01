use std::time::Duration;

use gpui::{
    actions, div, prelude::*, px, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    PromptLevel, SharedString, Window,
};
use honcho_config::{ColorTag, SavedConnection};
use honcho_db::{ConnectConfig, DriverKind, SessionOptions};

use crate::connections::{self, ActiveConnection, Connections};
use crate::editor_view::{self, EditorView};
use crate::theme::{self, Theme};

actions!(connection_dialog, [NextField, PrevField, Submit]);

const TEST_TIMEOUT: Duration = Duration::from_secs(10);
const DRIVERS: [DriverKind; 4] = [
    DriverKind::MySql,
    DriverKind::SqlServer,
    DriverKind::Postgres,
    DriverKind::Sqlite,
];

pub enum DialogEvent {
    Connected(ActiveConnection),
    Closed,
}

struct Fields {
    name: Entity<EditorView>,
    host: Entity<EditorView>,
    port: Entity<EditorView>,
    user: Entity<EditorView>,
    password: Entity<EditorView>,
    database: Entity<EditorView>,
}

impl Fields {
    fn in_order(&self) -> [&Entity<EditorView>; 6] {
        [
            &self.name,
            &self.host,
            &self.port,
            &self.user,
            &self.password,
            &self.database,
        ]
    }
}

#[derive(Clone)]
struct Status {
    text: String,
    error: bool,
}

pub struct ConnectionDialog {
    focus: FocusHandle,
    /// Holds what the fields don't: id, driver, color, read-only, options.
    draft: SavedConnection,
    /// The draft isn't in the store yet.
    is_new: bool,
    fields: Fields,
    status: Option<Status>,
    busy: bool,
}

impl EventEmitter<DialogEvent> for ConnectionDialog {}

impl ConnectionDialog {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let field = |placeholder: &str, cx: &mut Context<Self>| {
            cx.new(|cx| EditorView::single_line("ConnectionField", placeholder, cx))
        };
        let fields = Fields {
            name: field("Name", cx),
            host: field("localhost", cx),
            port: field("", cx),
            user: field("", cx),
            password: field("", cx),
            database: field("optional", cx),
        };
        fields.password.update(cx, |p, _| p.masked = true);
        for f in fields.in_order() {
            cx.observe(f, |_, _, cx| cx.notify()).detach();
        }
        let mut this = Self {
            focus: cx.focus_handle(),
            draft: SavedConnection::new("", DriverKind::MySql),
            is_new: true,
            fields,
            status: None,
            busy: false,
        };
        match cx
            .global::<Connections>()
            .store
            .connections()
            .first()
            .cloned()
        {
            Some(first) => this.load(first, false, window, cx),
            None => this.new_connection(window, cx),
        }
        if let Some(e) = cx.global::<Connections>().load_error.clone() {
            this.set_status(format!("Saved connections couldn't be read: {e}"), true);
        }
        this
    }

    pub fn selected_id(&self) -> Option<&str> {
        (!self.is_new).then_some(self.draft.id.as_str())
    }

    fn set_status(&mut self, text: impl Into<String>, error: bool) {
        self.status = Some(Status {
            text: text.into(),
            error,
        });
    }

    fn load(
        &mut self,
        saved: SavedConnection,
        is_new: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let password = if is_new {
            Ok(None)
        } else {
            saved.password(cx.global::<Connections>().secrets.as_ref())
        };
        self.status = None;
        let password = match password {
            Ok(p) => p.unwrap_or_default(),
            Err(e) => {
                self.set_status(e.to_string(), true);
                String::new()
            }
        };
        let port = saved.port.map(|p| p.to_string()).unwrap_or_default();
        let values = [
            saved.name.as_str(),
            saved.host.as_str(),
            port.as_str(),
            saved.user.as_str(),
            password.as_str(),
            saved.database.as_deref().unwrap_or(""),
        ];
        for (field, value) in self.fields.in_order().into_iter().zip(values) {
            field.update(cx, |f, cx| {
                f.set_text(value, cx);
                // an unfocused field shouldn't show a selection
                f.editor.set_cursor(f.editor.len_chars());
            });
        }
        self.draft = saved;
        self.is_new = is_new;
        self.driver_changed(cx);
        window.focus(&self.fields.name.focus_handle(cx));
        cx.notify();
    }

    fn driver_changed(&mut self, cx: &mut Context<Self>) {
        let port = connections::default_port(self.draft.driver)
            .map(|p| p.to_string())
            .unwrap_or_default();
        self.fields
            .port
            .update(cx, |f, cx| f.set_placeholder(&port, cx));
        cx.notify();
    }

    pub fn new_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load(
            SavedConnection::new("", DriverKind::MySql),
            true,
            window,
            cx,
        );
    }

    fn select(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(saved) = cx.global::<Connections>().store.get(id).cloned() {
            self.load(saved, false, window, cx);
        }
    }

    /// The connection as the form describes it, without the password.
    fn form(&self, cx: &App) -> Result<SavedConnection, String> {
        let text = |f: &Entity<EditorView>| f.read(cx).text().trim().to_string();
        let name = text(&self.fields.name);
        if name.is_empty() {
            return Err("Give the connection a name.".into());
        }
        let port = match text(&self.fields.port) {
            p if p.is_empty() => None,
            p => Some(
                p.parse::<u16>()
                    .map_err(|_| format!("Port {p} isn't a number from 1 to 65535."))?,
            ),
        };
        let database = Some(text(&self.fields.database)).filter(|d| !d.is_empty());
        Ok(SavedConnection {
            name,
            host: text(&self.fields.host),
            port,
            user: text(&self.fields.user),
            database,
            ..self.draft.clone()
        })
    }

    fn password(&self, cx: &App) -> Option<String> {
        Some(self.fields.password.read(cx).text()).filter(|p| !p.is_empty())
    }

    pub fn save(&mut self, cx: &mut Context<Self>) -> Option<SavedConnection> {
        match self.try_save(cx) {
            Ok(saved) => {
                self.set_status("Saved.", false);
                cx.notify();
                Some(saved)
            }
            Err(e) => {
                self.set_status(e, true);
                cx.notify();
                None
            }
        }
    }

    fn try_save(&mut self, cx: &mut Context<Self>) -> Result<SavedConnection, String> {
        let saved = self.form(cx)?;
        let password = self.password(cx);
        let state = cx.global_mut::<Connections>();
        if let Some(e) = &state.load_error {
            return Err(format!(
                "Not saving over {}, which couldn't be read: {e}",
                state.store.path().display()
            ));
        }
        saved
            .set_password(state.secrets.as_ref(), password.as_deref())
            .map_err(|e| e.to_string())?;
        state.store.upsert(saved.clone());
        state.store.save().map_err(|e| e.to_string())?;
        self.draft = saved.clone();
        self.is_new = false;
        Ok(saved)
    }

    fn duplicate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected_id().map(str::to_string) else {
            return;
        };
        let state = cx.global_mut::<Connections>();
        let copy = state
            .store
            .duplicate(&id, state.secrets.as_ref())
            .map_err(|e| e.to_string())
            .map(|c| c.cloned());
        let saved = state.store.save().map_err(|e| e.to_string());
        match (copy, saved) {
            (Ok(Some(copy)), Ok(())) => self.load(copy, false, window, cx),
            (Err(e), _) | (_, Err(e)) => self.set_status(e, true),
            (Ok(None), _) => {}
        }
        cx.notify();
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected_id().map(str::to_string) else {
            return;
        };
        let msg = format!("Delete the connection \"{}\"?", self.draft.name);
        let answer = window.prompt(
            PromptLevel::Warning,
            &msg,
            Some("Its saved password is deleted too."),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update_in(cx, |this, window, cx| this.delete_now(&id, window, cx))
                .ok();
        })
        .detach();
    }

    pub fn delete_now(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let state = cx.global_mut::<Connections>();
        let result = state
            .store
            .remove(id, state.secrets.as_ref())
            .map_err(|e| e.to_string())
            .and_then(|_| state.store.save().map_err(|e| e.to_string()));
        let next = state.store.connections().first().cloned();
        match result {
            Ok(()) => match next {
                Some(next) => self.load(next, false, window, cx),
                None => self.new_connection(window, cx),
            },
            Err(e) => self.set_status(e, true),
        }
        cx.notify();
    }

    fn connect_config(&self, saved: &SavedConnection, cx: &App) -> ConnectConfig {
        ConnectConfig {
            host: saved.host.clone(),
            port: saved.port,
            user: saved.user.clone(),
            password: self.password(cx),
            database: saved.database.clone(),
            read_only: saved.read_only,
            options: saved.options.clone(),
            connect_timeout: Some(TEST_TIMEOUT),
        }
    }

    /// Opens a session from the form. `keep` connects for real (after
    /// saving); otherwise it's only a test and the session is closed.
    fn open(&mut self, keep: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let saved = if keep {
            self.save(cx)
        } else {
            match self.form(cx) {
                Ok(s) => Some(s),
                Err(e) => {
                    self.set_status(e, true);
                    None
                }
            }
        };
        let Some(saved) = saved else {
            cx.notify();
            return;
        };
        let Some(driver) = connections::driver(saved.driver) else {
            self.set_status(
                format!(
                    "{} isn't supported yet.",
                    connections::driver_name(saved.driver)
                ),
                true,
            );
            cx.notify();
            return;
        };
        let config = self.connect_config(&saved, cx);
        let runtime = cx.global::<Connections>().runtime.clone();
        let connecting = runtime.connect(driver, config, SessionOptions::default());
        self.busy = true;
        self.set_status(if keep { "Connecting…" } else { "Testing…" }, false);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = match connecting.await {
                Ok(session) => match session.ping().await {
                    Ok(()) => Ok(session),
                    Err(e) => Err(e),
                },
                Err(e) => Err(e),
            };
            let session = match (&result, keep) {
                (Ok(s), false) => {
                    s.close().await;
                    None
                }
                (Ok(s), true) => Some(s.clone()),
                _ => None,
            };
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Err(e) => this.set_status(e.to_string(), true),
                    Ok(_) => this.set_status("Connected.", false),
                }
                if let Some(session) = session {
                    cx.emit(DialogEvent::Connected(ActiveConnection { saved, session }));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn focus_field(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let fields = self.fields.in_order();
        let current = fields
            .iter()
            .position(|f| f.focus_handle(cx).is_focused(window))
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(fields.len() as isize) as usize;
        let field = fields[next].clone();
        window.focus(&field.focus_handle(cx));
        field.update(cx, |f, cx| f.set_text(&f.text(), cx));
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(DialogEvent::Closed);
    }

    #[cfg(test)]
    pub fn status_text(&self) -> Option<&str> {
        self.status.as_ref().map(|s| s.text.as_str())
    }

    #[cfg(test)]
    pub fn set_driver(&mut self, driver: DriverKind) {
        self.draft.driver = driver;
    }
}

impl Focusable for ConnectionDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn button(label: &'static str, primary: bool, enabled: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(label)
        .px_2p5()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(Theme::border())
        .when(primary, |d| {
            d.bg(Theme::accent_dim()).border_color(Theme::accent())
        })
        .when(enabled, |d| {
            d.cursor_pointer().hover(|d| d.bg(Theme::border()))
        })
        .when(!enabled, |d| d.opacity(0.4))
        .child(label)
}

impl Render for ConnectionDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = cx.global::<Connections>();
        let selected = self.selected_id().map(str::to_string);
        let saved: Vec<SavedConnection> = state.store.connections().to_vec();

        let list = div()
            .w(px(230.))
            .flex_none()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(Theme::border())
            .child(
                div()
                    .id("connection-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .py_1()
                    .children(saved.into_iter().map(|c| {
                        let active = selected.as_deref() == Some(c.id.as_str());
                        let id = c.id.clone();
                        div()
                            .id(SharedString::from(c.id.clone()))
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .py_1()
                            .cursor_pointer()
                            .when(active, |d| d.bg(Theme::accent_dim()))
                            .hover(|d| d.bg(Theme::border()))
                            .child(color_dot(c.color))
                            .child(
                                div()
                                    .flex_1()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .child(c.name.clone()),
                            )
                            .on_click(
                                cx.listener(move |this, _, window, cx| {
                                    this.select(&id, window, cx)
                                }),
                            )
                    }))
                    .when(self.is_new, |d| {
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_3()
                                .py_1()
                                .bg(Theme::accent_dim())
                                .child(color_dot(self.draft.color))
                                .child(div().text_color(Theme::muted()).child("New connection")),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .gap_1()
                    .p_2()
                    .border_t_1()
                    .border_color(Theme::border())
                    .child(button("New", false, true).on_click(
                        cx.listener(|this, _, window, cx| this.new_connection(window, cx)),
                    ))
                    .child(
                        button("Duplicate", false, !self.is_new).on_click(
                            cx.listener(|this, _, window, cx| this.duplicate(window, cx)),
                        ),
                    )
                    .child(
                        button("Delete", false, !self.is_new)
                            .on_click(cx.listener(|this, _, window, cx| this.delete(window, cx))),
                    ),
            );

        let field = |label: &'static str, input: &Entity<EditorView>, width: Option<f32>| {
            let focused = input.focus_handle(cx).is_focused(window);
            let input_box = div()
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
                .overflow_hidden()
                .child(
                    div()
                        .flex_1()
                        .h(px(18.))
                        .overflow_hidden()
                        .child(input.clone()),
                );
            let input_box = match width {
                Some(w) => input_box.w(px(w)).flex_none(),
                None => input_box.flex_1(),
            };
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .when(width.is_none(), |d| d.w(px(72.)).flex_none())
                        .text_color(Theme::muted())
                        .child(label),
                )
                .child(input_box)
        };

        let drivers = div().flex().gap_1().children(DRIVERS.map(|kind| {
            let available = connections::driver(kind).is_some();
            let active = self.draft.driver == kind;
            button(connections::driver_name(kind), active, available).on_click(cx.listener(
                move |this, _, _, cx| {
                    if available {
                        this.draft.driver = kind;
                        this.driver_changed(cx);
                    }
                },
            ))
        }));

        let swatch = |tag: Option<ColorTag>, active: bool| {
            div()
                .id(SharedString::from(format!("color-{tag:?}")))
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
        };
        let colors = div().flex().items_center().gap_1p5().children(
            std::iter::once(None)
                .chain(ColorTag::ALL.map(Some))
                .map(|tag| {
                    swatch(tag, self.draft.color == tag).on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.draft.color = tag;
                            cx.notify();
                        },
                    ))
                }),
        );

        let read_only = div()
            .id("read-only")
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .child(
                div()
                    .size(px(14.))
                    .rounded_sm()
                    .border_1()
                    .border_color(Theme::muted())
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(10.))
                    .when(self.draft.read_only, |d| {
                        d.bg(Theme::accent())
                            .border_color(Theme::accent())
                            .child("✓")
                    }),
            )
            .child("Read-only")
            .child(
                div()
                    .text_color(Theme::muted())
                    .child("refuses anything that isn't a read"),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.draft.read_only = !this.draft.read_only;
                cx.notify();
            }));

        let label = |text: &'static str| {
            div()
                .w(px(72.))
                .flex_none()
                .text_color(Theme::muted())
                .child(text)
        };
        let row = || div().flex().items_center().gap_2();

        let status = self.status.clone().map(|s| {
            div()
                .text_color(if s.error {
                    Theme::error()
                } else {
                    Theme::muted()
                })
                .child(s.text)
        });

        let busy = self.busy;
        let form = div()
            .flex_1()
            .min_w_0()
            .p_4()
            .flex()
            .flex_col()
            .gap_2()
            .child(field("Name", &self.fields.name, None))
            .child(row().child(label("Driver")).child(drivers))
            .child(
                row()
                    .child(field("Host", &self.fields.host, None).flex_1())
                    .child(field("Port", &self.fields.port, Some(84.))),
            )
            .child(field("User", &self.fields.user, None))
            .child(field("Password", &self.fields.password, None))
            .child(field("Database", &self.fields.database, None))
            .child(row().child(label("Color")).child(colors))
            .child(row().child(label("")).child(read_only))
            .child(div().flex_1())
            .child(div().min_h(px(18.)).children(status))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        button("Test", false, !busy)
                            .on_click(cx.listener(|this, _, _, cx| this.open(false, cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        button("Close", false, true)
                            .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                    )
                    .child(
                        button("Save", false, !busy).on_click(cx.listener(|this, _, _, cx| {
                            this.save(cx);
                        })),
                    )
                    .child(
                        button("Connect", true, !busy)
                            .on_click(cx.listener(|this, _, _, cx| this.open(true, cx))),
                    ),
            );

        let dialog = div()
            .key_context("ConnectionDialog")
            .track_focus(&self.focus)
            .occlude()
            .w(px(720.))
            .h(px(440.))
            .flex()
            .bg(Theme::panel())
            .border_1()
            .border_color(Theme::border())
            .rounded_lg()
            .shadow_lg()
            .overflow_hidden()
            .text_color(Theme::text())
            .font_family(theme::font_family(cx))
            .text_size(px(12.))
            .on_action(
                cx.listener(|this, _: &NextField, window, cx| this.focus_field(1, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &PrevField, window, cx| this.focus_field(-1, window, cx)),
            )
            .on_action(cx.listener(|this, _: &Submit, _, cx| this.open(true, cx)))
            // Escape in a field with nothing to collapse bubbles up as Cancel
            .on_action(cx.listener(|this, _: &editor_view::Cancel, _, cx| this.close(cx)))
            .child(list)
            .child(form);

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

pub fn color_dot(tag: Option<ColorTag>) -> gpui::Div {
    div()
        .size(px(8.))
        .flex_none()
        .rounded_full()
        .map(|d| match tag {
            Some(t) => d.bg(connections::color(t)),
            None => d.border_1().border_color(Theme::muted()),
        })
}
