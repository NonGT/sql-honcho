use std::time::Duration;

use gpui::{
    actions, div, prelude::*, px, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    PromptLevel, SharedString, Window,
};
use honcho_config::{ColorTag, SavedConnection, SavedSsh};
use honcho_db::tunnel::{self, HostKeyProblem, SshAuth, SshConfig};
use honcho_db::{ConnectConfig, DbError, DriverKind, SessionOptions, TlsConfig, TlsMode};

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
    Connected(Box<ActiveConnection>),
    Closed,
}

struct Fields {
    name: Entity<EditorView>,
    host: Entity<EditorView>,
    port: Entity<EditorView>,
    user: Entity<EditorView>,
    password: Entity<EditorView>,
    database: Entity<EditorView>,
    ca: Entity<EditorView>,
    cert: Entity<EditorView>,
    key: Entity<EditorView>,
    ssh_host: Entity<EditorView>,
    ssh_port: Entity<EditorView>,
    ssh_user: Entity<EditorView>,
    ssh_key: Entity<EditorView>,
    ssh_secret: Entity<EditorView>,
}

impl Fields {
    fn in_order(&self) -> [&Entity<EditorView>; 9] {
        [
            &self.name,
            &self.host,
            &self.port,
            &self.user,
            &self.password,
            &self.database,
            &self.ca,
            &self.cert,
            &self.key,
        ]
    }

    /// The fields on a tab, in Tab order.
    fn on(&self, section: Section) -> Vec<&Entity<EditorView>> {
        match section {
            Section::General => self.in_order()[..6].to_vec(),
            Section::Tls => vec![&self.ca, &self.cert, &self.key],
            Section::Ssh => vec![
                &self.ssh_host,
                &self.ssh_port,
                &self.ssh_user,
                &self.ssh_key,
                &self.ssh_secret,
            ],
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    General,
    Tls,
    Ssh,
}

fn auth_label(auth: SshAuth) -> &'static str {
    match auth {
        SshAuth::Password => "Password",
        SshAuth::Key => "Key file",
        SshAuth::Agent => "Agent",
    }
}

fn mode_label(mode: TlsMode) -> &'static str {
    match mode {
        TlsMode::Disable => "Disable",
        TlsMode::Prefer => "Prefer",
        TlsMode::Require => "Require",
        TlsMode::VerifyCa => "Verify CA",
        TlsMode::VerifyFull => "Verify full",
    }
}

/// SQL Server sign-in methods, as `options["auth"]` holds them.
const SQL_SERVER_AUTHS: [(&str, &str, &str); 3] = [
    ("sql", "SQL login", ""),
    ("aad", "Entra token", "Token goes in Password."),
    ("windows", "Windows", "Empty User signs in as you."),
];

fn mode_hint(mode: TlsMode) -> &'static str {
    match mode {
        TlsMode::Disable => "Never encrypt.",
        TlsMode::Prefer => "Encrypt when the server can, without checking its certificate.",
        TlsMode::Require => "Always encrypt, without checking the certificate.",
        TlsMode::VerifyCa => "Encrypt, and the certificate must come from a trusted CA.",
        TlsMode::VerifyFull => {
            "Encrypt, the certificate must come from a trusted CA and name the host."
        }
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
    /// An unknown SSH host key waiting on the user, and the run to retry.
    host_key: Option<(HostKeyProblem, SavedConnection, bool)>,
    section: Section,
    busy: bool,
    /// The connection has saved secrets, which are kept unless new ones
    /// are typed. They're only read from the keyring to connect: on macOS
    /// every read can be a prompt, and just looking at a connection
    /// shouldn't be one.
    keeps_secrets: bool,
}

/// What a save does with the stored password.
enum PasswordChange {
    /// The user typed nothing over a saved one: leave it alone.
    Keep,
    Set(Option<String>),
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
            ca: field("system roots", cx),
            cert: field("none", cx),
            key: field("none", cx),
            ssh_host: field("host or ~/.ssh/config alias", cx),
            ssh_port: field("22", cx),
            ssh_user: field("from ~/.ssh/config, or your login", cx),
            ssh_key: field("from ~/.ssh/config", cx),
            ssh_secret: field("", cx),
        };
        fields.password.update(cx, |p, _| p.masked = true);
        fields.ssh_secret.update(cx, |p, _| p.masked = true);
        for f in fields.in_order().into_iter().chain(fields.on(Section::Ssh)) {
            cx.observe(f, |_, _, cx| cx.notify()).detach();
        }
        let mut this = Self {
            focus: cx.focus_handle(),
            draft: SavedConnection::new("", DriverKind::MySql),
            is_new: true,
            fields,
            status: None,
            host_key: None,
            section: Section::General,
            busy: false,
            keeps_secrets: false,
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

    fn set_field(field: &Entity<EditorView>, value: &str, cx: &mut Context<Self>) {
        field.update(cx, |f, cx| {
            f.set_text(value, cx);
            // an unfocused field shouldn't show a selection
            f.editor.set_cursor(f.editor.len_chars());
        });
    }

    fn load(
        &mut self,
        saved: SavedConnection,
        is_new: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.status = None;
        let port = saved.port.map(|p| p.to_string()).unwrap_or_default();
        let path = |p: &Option<std::path::PathBuf>| {
            p.as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        };
        let (ca, cert, key) = (
            path(&saved.tls.ca_file),
            path(&saved.tls.cert_file),
            path(&saved.tls.key_file),
        );
        let values = [
            saved.name.as_str(),
            saved.host.as_str(),
            port.as_str(),
            saved.user.as_str(),
            "",
            saved.database.as_deref().unwrap_or(""),
            &ca,
            &cert,
            &key,
        ];
        for (field, value) in self.fields.in_order().into_iter().zip(values) {
            Self::set_field(field, value, cx);
        }
        let ssh = saved.ssh.clone().unwrap_or_default();
        let ssh_values = [
            ssh.host.clone(),
            ssh.port.map(|p| p.to_string()).unwrap_or_default(),
            ssh.user.clone(),
            path(&ssh.key_file),
            String::new(),
        ];
        for (field, value) in self.fields.on(Section::Ssh).into_iter().zip(ssh_values) {
            Self::set_field(field, &value, cx);
        }
        self.host_key = None;
        self.keeps_secrets = !is_new;
        let hint = if is_new { "" } else { "saved, type to change" };
        for f in [&self.fields.password, &self.fields.ssh_secret] {
            f.update(cx, |f, cx| f.set_placeholder(hint, cx));
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
        if self.draft.driver == DriverKind::Sqlite {
            let file = text(&self.fields.host);
            if file.is_empty() {
                return Err("Pick the database file.".into());
            }
            return Ok(SavedConnection {
                name,
                host: file,
                port: None,
                user: String::new(),
                database: None,
                tls: TlsConfig::default(),
                ssh: None,
                ..self.draft.clone()
            });
        }
        let database = Some(text(&self.fields.database)).filter(|d| !d.is_empty());
        let path = |f: &Entity<EditorView>| {
            Some(text(f))
                .filter(|p| !p.is_empty())
                .map(std::path::PathBuf::from)
        };
        let tls = TlsConfig {
            mode: self.draft.tls.mode,
            ca_file: path(&self.fields.ca),
            cert_file: path(&self.fields.cert),
            key_file: path(&self.fields.key),
            server_name: None,
        };
        if tls.cert_file.is_some() != tls.key_file.is_some() {
            return Err("A client certificate needs both the certificate and its key.".into());
        }
        let ssh = match &self.draft.ssh {
            Some(draft) => {
                let host = text(&self.fields.ssh_host);
                if host.is_empty() {
                    return Err("Give the SSH host, or turn SSH off.".into());
                }
                let port = match text(&self.fields.ssh_port) {
                    p if p.is_empty() => None,
                    p => Some(
                        p.parse::<u16>()
                            .map_err(|_| format!("SSH port {p} isn't a number from 1 to 65535."))?,
                    ),
                };
                Some(SavedSsh {
                    host,
                    port,
                    user: text(&self.fields.ssh_user),
                    auth: draft.auth,
                    key_file: path(&self.fields.ssh_key),
                })
            }
            None => None,
        };
        Ok(SavedConnection {
            name,
            host: text(&self.fields.host),
            port,
            user: text(&self.fields.user),
            database,
            tls,
            ssh,
            ..self.draft.clone()
        })
    }

    fn password(&self, cx: &App) -> Option<String> {
        Some(self.fields.password.read(cx).text()).filter(|p| !p.is_empty())
    }

    fn password_change(&self, cx: &App) -> PasswordChange {
        match self.password(cx) {
            None if self.keeps_secrets => PasswordChange::Keep,
            p => PasswordChange::Set(p),
        }
    }

    fn ssh_secret(&self, cx: &App) -> Option<String> {
        Some(self.fields.ssh_secret.read(cx).text()).filter(|p| !p.is_empty())
    }

    fn ssh_secret_change(&self, cx: &App) -> PasswordChange {
        match self.ssh_secret(cx) {
            None if self.keeps_secrets => PasswordChange::Keep,
            p => PasswordChange::Set(p),
        }
    }

    fn writable(&self, cx: &App) -> Result<(), String> {
        let state = cx.global::<Connections>();
        match &state.load_error {
            Some(e) => Err(format!(
                "Not saving over {}, which couldn't be read: {e}",
                state.store.path().display()
            )),
            None => Ok(()),
        }
    }

    pub fn save(&mut self, cx: &mut Context<Self>) {
        self.save_then(cx, |_, _, _| {});
    }

    fn save_then(
        &mut self,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, SavedConnection, &mut Context<Self>) + 'static,
    ) {
        if self.busy {
            return;
        }
        let saved = match self.form(cx).and_then(|s| self.writable(cx).map(|()| s)) {
            Ok(s) => s,
            Err(e) => {
                self.set_status(e, true);
                cx.notify();
                return;
            }
        };
        let change = self.password_change(cx);
        // without SSH there's no secret to keep
        let ssh_change = match saved.ssh {
            Some(_) => self.ssh_secret_change(cx),
            None => PasswordChange::Set(None),
        };
        let secrets = cx.global::<Connections>().secrets.clone();
        let owner = saved.clone();
        let write = cx.background_spawn(async move {
            if let PasswordChange::Set(p) = change {
                owner.set_password(secrets.as_ref(), p.as_deref())?;
            }
            if let PasswordChange::Set(p) = ssh_change {
                owner.set_ssh_secret(secrets.as_ref(), p.as_deref())?;
            }
            Ok::<_, honcho_config::SecretError>(())
        });
        self.busy = true;
        self.set_status("Saving…", false);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let written = write.await;
            this.update(cx, |this, cx| {
                this.busy = false;
                let result = written.map_err(|e| e.to_string()).and_then(|()| {
                    let state = cx.global_mut::<Connections>();
                    state.store.upsert(saved.clone());
                    state.store.save().map_err(|e| e.to_string())
                });
                match result {
                    Ok(()) => {
                        if this.draft.id == saved.id {
                            this.draft = saved.clone();
                            this.is_new = false;
                        }
                        this.set_status("Saved.", false);
                        then(this, saved, cx);
                    }
                    Err(e) => this.set_status(e, true),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Runs a keyring job in the background and reports a failure.
    fn keyring_job(
        &mut self,
        what: &'static str,
        job: impl FnOnce(&dyn honcho_config::Secrets) -> Result<(), honcho_config::SecretError>
            + Send
            + 'static,
        cx: &mut Context<Self>,
    ) {
        let secrets = cx.global::<Connections>().secrets.clone();
        let run = cx.background_spawn(async move { job(secrets.as_ref()) });
        cx.spawn(async move |this, cx| {
            if let Err(e) = run.await {
                this.update(cx, |this, cx| {
                    this.set_status(format!("{what}: {e}"), true);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn duplicate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected_id().map(str::to_string) else {
            return;
        };
        if let Err(e) = self.writable(cx) {
            self.set_status(e, true);
            cx.notify();
            return;
        }
        let state = cx.global_mut::<Connections>();
        let original = state.store.get(&id).cloned();
        let copy = state.store.duplicate(&id).cloned();
        let saved = state.store.save();
        let (Some(original), Some(copy)) = (original, copy) else {
            return;
        };
        self.load(copy.clone(), false, window, cx);
        if let Err(e) = saved {
            self.set_status(e.to_string(), true);
        }
        self.keyring_job(
            "The password wasn't copied",
            move |secrets| original.copy_secrets_to(&copy, secrets),
            cx,
        );
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
        if let Err(e) = self.writable(cx) {
            self.set_status(e, true);
            cx.notify();
            return;
        }
        let state = cx.global_mut::<Connections>();
        let Some(removed) = state.store.remove(id) else {
            return;
        };
        let saved = state.store.save();
        let next = state.store.connections().first().cloned();
        match next {
            Some(next) => self.load(next, false, window, cx),
            None => self.new_connection(window, cx),
        }
        if let Err(e) = saved {
            self.set_status(e.to_string(), true);
        }
        self.keyring_job(
            "The saved password couldn't be deleted",
            move |secrets| removed.forget_secrets(secrets),
            cx,
        );
    }

    fn connect_config(&self, saved: &SavedConnection, cx: &App) -> ConnectConfig {
        ConnectConfig {
            host: saved.host.clone(),
            port: saved.port,
            user: saved.user.clone(),
            password: self.password(cx),
            database: saved.database.clone(),
            read_only: saved.read_only,
            tls: saved.tls.clone(),
            ssh: saved.ssh.as_ref().map(|ssh| SshConfig {
                host: ssh.host.clone(),
                port: ssh.port,
                user: Some(ssh.user.clone()).filter(|u| !u.is_empty()),
                auth: ssh.auth,
                key_file: ssh.key_file.clone(),
                secret: self.ssh_secret(cx),
                ..Default::default()
            }),
            options: saved.options.clone(),
            connect_timeout: Some(TEST_TIMEOUT),
        }
    }

    /// Test (`keep` false) or connect for real, after saving.
    fn open(&mut self, keep: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if keep {
            self.save_then(cx, |this, saved, cx| this.connect(saved, true, cx));
        } else {
            match self.form(cx) {
                Ok(saved) => self.connect(saved, false, cx),
                Err(e) => {
                    self.set_status(e, true);
                    cx.notify();
                }
            }
        }
    }

    fn connect(&mut self, saved: SavedConnection, keep: bool, cx: &mut Context<Self>) {
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
        let mut config = self.connect_config(&saved, cx);
        let state = cx.global::<Connections>();
        let (runtime, secrets) = (state.runtime.clone(), state.secrets.clone());
        let keeps = self.keeps_secrets;
        let owner = saved.clone();
        // off the UI thread, since the keyring can sit on a prompt
        let connecting = cx.background_spawn(async move {
            let read = |e: honcho_config::SecretError| DbError::Other(e.to_string());
            if keeps && config.password.is_none() {
                config.password = owner.password(secrets.as_ref()).map_err(read)?;
            }
            if let Some(ssh) = config.ssh.as_mut().filter(|s| keeps && s.secret.is_none()) {
                ssh.secret = owner.ssh_secret(secrets.as_ref()).map_err(read)?;
            }
            runtime
                .connect(driver, config, SessionOptions::default())
                .await
        });
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
                    Err(DbError::HostKey(problem)) if !problem.changed => {
                        this.status = None;
                        this.host_key = Some((*problem, saved.clone(), keep));
                    }
                    Err(e) => this.set_status(e.to_string(), true),
                    Ok(_) => this.set_status("Connected.", false),
                }
                if let Some(session) = session {
                    cx.emit(DialogEvent::Connected(Box::new(ActiveConnection {
                        saved,
                        session,
                    })));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The user trusts the SSH host key: record it and try again.
    fn trust_host_key(&mut self, cx: &mut Context<Self>) {
        let Some((problem, saved, keep)) = self.host_key.take() else {
            return;
        };
        match tunnel::trust_host_key(&problem) {
            Ok(()) => self.connect(saved, keep, cx),
            Err(e) => self.set_status(e.to_string(), true),
        }
        cx.notify();
    }

    fn set_ssh(&mut self, on: bool, cx: &mut Context<Self>) {
        self.draft.ssh = if on {
            Some(self.draft.ssh.clone().unwrap_or_default())
        } else {
            None
        };
        cx.notify();
    }

    /// The fields on screen, in Tab order.
    fn visible_fields(&self) -> Vec<Entity<EditorView>> {
        let f = &self.fields;
        let fields = match (self.section, &self.draft.ssh) {
            (Section::Ssh, None) => vec![],
            (Section::Ssh, Some(ssh)) => {
                let mut v = vec![&f.ssh_host, &f.ssh_port, &f.ssh_user];
                match ssh.auth {
                    SshAuth::Key => v.extend([&f.ssh_key, &f.ssh_secret]),
                    SshAuth::Password => v.push(&f.ssh_secret),
                    SshAuth::Agent => {}
                }
                v
            }
            // a file is all SQLite needs
            (Section::General, _) if self.draft.driver == DriverKind::Sqlite => {
                vec![&f.name, &f.host]
            }
            (section, _) => f.on(section),
        };
        fields.into_iter().cloned().collect()
    }

    fn show_section(&mut self, section: Section, window: &mut Window, cx: &mut Context<Self>) {
        self.section = section;
        if let Some(first) = self.visible_fields().first() {
            window.focus(&first.focus_handle(cx));
        }
        cx.notify();
    }

    fn browse(&mut self, field: Entity<EditorView>, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(path) = paths.first() {
                    this.update(cx, |_, cx| {
                        Self::set_field(&field, &path.display().to_string(), cx)
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    fn focus_field(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let fields = self.visible_fields();
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

    #[cfg(test)]
    pub fn set_color(&mut self, color: Option<ColorTag>) {
        self.draft.color = color;
    }

    #[cfg(test)]
    pub fn show_tls(&mut self, mode: TlsMode, window: &mut Window, cx: &mut Context<Self>) {
        self.draft.tls.mode = mode;
        self.show_section(Section::Tls, window, cx);
    }

    #[cfg(test)]
    pub fn show_ssh(&mut self, auth: SshAuth, window: &mut Window, cx: &mut Context<Self>) {
        self.draft.ssh = Some(SavedSsh {
            auth,
            ..Default::default()
        });
        self.show_section(Section::Ssh, window, cx);
    }

    #[cfg(test)]
    pub fn set_auto_reconnect(&mut self, on: bool) {
        self.draft.auto_reconnect = on;
    }
}

impl Focusable for ConnectionDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn button(label: &'static str, primary: bool, enabled: bool) -> gpui::Stateful<gpui::Div> {
    button_with_id(label, label, primary, enabled)
}

fn button_with_id(
    id: &'static str,
    label: &'static str,
    primary: bool,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
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
                        .when(width.is_none(), |d| d.w(px(84.)).flex_none())
                        .whitespace_nowrap()
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
                        if kind != DriverKind::SqlServer {
                            this.draft.options.remove("auth");
                        }
                        if kind == DriverKind::Sqlite {
                            this.section = Section::General;
                        }
                        this.driver_changed(cx);
                    }
                },
            ))
        }));

        let sql_auth = self
            .draft
            .options
            .get("auth")
            .map_or("sql", String::as_str)
            .to_string();
        let sql_auths = div()
            .flex()
            .gap_1()
            .children(SQL_SERVER_AUTHS.map(|(key, text, _)| {
                // Windows sign-in needs the Windows libraries
                let available = key != "windows" || cfg!(windows);
                button_with_id(key, text, sql_auth == key, available).on_click(cx.listener(
                    move |this, _, _, cx| {
                        if !available {
                            return;
                        }
                        if key == "sql" {
                            this.draft.options.remove("auth");
                        } else {
                            this.draft.options.insert("auth".into(), key.into());
                        }
                        cx.notify();
                    },
                ))
            }));
        let sql_auth_hint = SQL_SERVER_AUTHS
            .iter()
            .find(|(key, ..)| *key == sql_auth)
            .map(|(.., hint)| *hint)
            .filter(|h| !h.is_empty());
        let is_sql_server = self.draft.driver == DriverKind::SqlServer;
        let is_sqlite = self.draft.driver == DriverKind::Sqlite;

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

        let checkbox = |id: &'static str, label: &'static str, hint: &'static str, on: bool| {
            div()
                .id(id)
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
                        .when(on, |d| {
                            d.bg(Theme::accent())
                                .border_color(Theme::accent())
                                .child("✓")
                        }),
                )
                .child(label)
                .child(div().text_color(Theme::muted()).child(hint))
        };
        let read_only = checkbox(
            "read-only",
            "Read-only",
            "refuses anything that isn't a read",
            self.draft.read_only,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.draft.read_only = !this.draft.read_only;
            cx.notify();
        }));
        let auto_reconnect = checkbox(
            "auto-reconnect",
            "Reconnect automatically",
            "after the server closes it",
            self.draft.auto_reconnect,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.draft.auto_reconnect = !this.draft.auto_reconnect;
            cx.notify();
        }));

        let label = |text: &'static str| {
            div()
                .w(px(84.))
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
        let section_tab = |id: &'static str, text: SharedString, section: Section| {
            let active = self.section == section;
            div()
                .id(id)
                .px_3()
                .pb_1()
                .cursor_pointer()
                .border_b_2()
                .border_color(if active {
                    Theme::accent()
                } else {
                    Theme::panel()
                })
                .text_color(if active {
                    Theme::text()
                } else {
                    Theme::muted()
                })
                .child(text)
                .on_click(
                    cx.listener(move |this, _, window, cx| this.show_section(section, window, cx)),
                )
        };
        let tls_mode = self.draft.tls.mode;
        // the mode shows on the tab so a stricter setting isn't hidden
        let tls_label: SharedString = if tls_mode == TlsMode::Prefer {
            "TLS".into()
        } else {
            format!("TLS · {}", mode_label(tls_mode)).into()
        };
        let sections = div()
            .flex()
            .border_b_1()
            .border_color(Theme::border())
            .mb_1()
            .child(section_tab(
                "section-general",
                "General".into(),
                Section::General,
            ))
            // a local file has no TLS or SSH
            .when(!is_sqlite, |d| {
                d.child(section_tab("section-tls", tls_label, Section::Tls))
                    .child(section_tab(
                        "section-ssh",
                        match &self.draft.ssh {
                            Some(_) => "SSH · on".into(),
                            None => "SSH".into(),
                        },
                        Section::Ssh,
                    ))
            });

        let ssh_on = self.draft.ssh.is_some();
        let auth = self.draft.ssh.as_ref().map(|s| s.auth).unwrap_or_default();
        let ssh_toggle = checkbox(
            "ssh-on",
            "Connect through SSH",
            "Host on General is as the SSH server sees it",
            ssh_on,
        )
        .on_click(cx.listener(move |this, _, _, cx| this.set_ssh(!ssh_on, cx)));
        let auths =
            div()
                .flex()
                .gap_1()
                .children([SshAuth::Password, SshAuth::Key, SshAuth::Agent].map(|a| {
                    button(auth_label(a), a == auth, ssh_on).on_click(cx.listener(
                        move |this, _, _, cx| {
                            if let Some(ssh) = &mut this.draft.ssh {
                                ssh.auth = a;
                                cx.notify();
                            }
                        },
                    ))
                }));

        let browse = |id: &'static str, target: &Entity<EditorView>| {
            let target = target.clone();
            button_with_id(id, "Browse…", false, true)
                .on_click(cx.listener(move |this, _, _, cx| this.browse(target.clone(), cx)))
        };
        let path_row = |text: &'static str, input: &Entity<EditorView>, id: &'static str| {
            row()
                .child(field(text, input, None).flex_1())
                .child(browse(id, input))
        };
        let modes = div().flex().gap_1().children(TlsMode::ALL.map(|mode| {
            button(mode_label(mode), mode == tls_mode, true).on_click(cx.listener(
                move |this, _, _, cx| {
                    this.draft.tls.mode = mode;
                    cx.notify();
                },
            ))
        }));

        let form = div()
            .flex_1()
            .min_w_0()
            .p_4()
            .flex()
            .flex_col()
            .gap_2()
            .child(sections)
            .map(|d| match self.section {
                Section::General => d
                    .child(field("Name", &self.fields.name, None))
                    .child(row().child(label("Driver")).child(drivers))
                    .when(is_sql_server, |d| {
                        d.child(
                            row().child(label("Auth")).child(sql_auths).children(
                                sql_auth_hint.map(|hint| {
                                    div()
                                        .min_w_0()
                                        .text_color(Theme::muted())
                                        .child(hint)
                                }),
                            ),
                        )
                    })
                    .when(is_sqlite, |d| {
                        d.child(path_row("File", &self.fields.host, "browse-file"))
                    })
                    .when(!is_sqlite, |d| {
                        d.child(
                            row()
                                .child(field("Host", &self.fields.host, None).flex_1())
                                .child(field("Port", &self.fields.port, Some(84.))),
                        )
                        .child(field("User", &self.fields.user, None))
                        .child(field("Password", &self.fields.password, None))
                        .child(field("Database", &self.fields.database, None))
                    })
                    .child(row().child(label("Color")).child(colors))
                    .child(row().child(label("")).child(read_only))
                    // there's no server to drop a file
                    .when(!is_sqlite, |d| {
                        d.child(row().child(label("")).child(auto_reconnect))
                    }),
                Section::Tls => d
                    .child(row().child(label("Mode")).child(modes))
                    .child(
                        row().child(label("")).child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_color(Theme::muted())
                                .child(mode_hint(tls_mode)),
                        ),
                    )
                    .child(path_row("CA file", &self.fields.ca, "browse-ca"))
                    .child(path_row("Client cert", &self.fields.cert, "browse-cert"))
                    .child(path_row("Client key", &self.fields.key, "browse-key")),
                Section::Ssh => d
                    .child(row().child(label("")).child(ssh_toggle))
                    .when(ssh_on, |d| {
                        d.child(
                            row()
                                .child(field("SSH host", &self.fields.ssh_host, None).flex_1())
                                .child(field("Port", &self.fields.ssh_port, Some(84.))),
                        )
                        .child(field("SSH user", &self.fields.ssh_user, None))
                        .child(row().child(label("Auth")).child(auths))
                        .when(auth == SshAuth::Key, |d| {
                            d.child(path_row("Key file", &self.fields.ssh_key, "browse-ssh-key"))
                                .child(field("Passphrase", &self.fields.ssh_secret, None))
                        })
                        .when(auth == SshAuth::Password, |d| {
                            d.child(field("Password", &self.fields.ssh_secret, None))
                        })
                        .when(auth == SshAuth::Agent, |d| {
                            d.child(row().child(label("")).child(
                                div()
                                    .text_color(Theme::muted())
                                    .child("Uses the keys loaded in ssh-agent."),
                            ))
                        })
                    }),
            })
            .child(div().flex_1())
            .child(div().min_h(px(18.)).children(status))
            .children(self.host_key.as_ref().map(|(p, _, _)| {
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(Theme::accent())
                    .child(format!("First connection to the SSH server {}:{}.", p.host, p.port))
                    .child(
                        div()
                            .text_color(Theme::muted())
                            .child(format!("Its host key is {}", p.fingerprint)),
                    )
                    .child(
                        div().text_color(Theme::muted()).child(
                            "Check it with the server's admin if you can. Trusting adds it to known_hosts.",
                        ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .justify_end()
                            .child(button_with_id("host-key-cancel", "Cancel", false, true).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.host_key = None;
                                    cx.notify();
                                }),
                            ))
                            .child(
                                button_with_id("host-key-trust", "Trust and connect", true, true)
                                    .on_click(cx.listener(|this, _, _, cx| this.trust_host_key(cx))),
                            ),
                    )
            }))
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
            .w(px(780.))
            .h(px(480.))
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
