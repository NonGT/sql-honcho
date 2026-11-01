//! SSH tunnels: a local port forwarded through an SSH server to the
//! database, so any driver can use one without knowing.

pub mod ssh_config;

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use russh::client;
use russh::keys::{self, PrivateKeyWithHashAlg, PublicKey};
use tokio::net::TcpListener;

use crate::{DbError, Result};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SshAuth {
    Password,
    /// A private key file, with an optional passphrase.
    #[default]
    Key,
    /// Keys held by ssh-agent (or Pageant / the OpenSSH agent on Windows).
    Agent,
}

#[derive(Clone, Default, PartialEq)]
pub struct SshConfig {
    /// A host name, or an alias from the SSH config file.
    pub host: String,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub auth: SshAuth,
    /// Falls back to the alias's IdentityFile.
    pub key_file: Option<PathBuf>,
    /// The password, or the key's passphrase.
    pub secret: Option<String>,
    /// Defaults to `~/.ssh/known_hosts`.
    pub known_hosts: Option<PathBuf>,
    /// Defaults to `~/.ssh/config`.
    pub config_file: Option<PathBuf>,
}

impl fmt::Debug for SshConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("auth", &self.auth)
            .field("key_file", &self.key_file)
            .field("secret", &self.secret.as_ref().map(|_| "***"))
            .finish()
    }
}

/// The server's host key isn't the one on record, or there's no record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeyProblem {
    pub host: String,
    pub port: u16,
    /// OpenSSH public key line, to record it if the user trusts it.
    pub key: String,
    /// SHA256 fingerprint, as `ssh` shows it.
    pub fingerprint: String,
    /// A different key is on record: possibly someone in the middle.
    pub changed: bool,
    pub known_hosts: PathBuf,
}

impl fmt::Display for HostKeyProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.changed {
            write!(
                f,
                "the SSH host key for {} has changed (now {}). Someone could be \
                 intercepting the connection; if the server really was reinstalled, \
                 remove its old line from {}",
                self.host,
                self.fingerprint,
                self.known_hosts.display()
            )
        } else {
            write!(
                f,
                "unknown SSH host key for {} ({})",
                self.host, self.fingerprint
            )
        }
    }
}

/// Records the key as trusted for its host.
pub fn trust_host_key(problem: &HostKeyProblem) -> Result<()> {
    if problem.changed {
        return Err(DbError::Tunnel(
            "a changed host key has to be removed by hand".into(),
        ));
    }
    let key = PublicKey::from_openssh(&problem.key)
        .map_err(|e| DbError::Tunnel(format!("bad host key: {e}")))?;
    keys::known_hosts::learn_known_hosts_path(
        &problem.host,
        problem.port,
        &key,
        &problem.known_hosts,
    )
    .map_err(|e| {
        DbError::Tunnel(format!(
            "couldn't write {}: {e}",
            problem.known_hosts.display()
        ))
    })
}

fn default_known_hosts() -> Result<PathBuf> {
    ssh_config::default_path()
        .and_then(|p| p.parent().map(|d| d.join("known_hosts")))
        .ok_or_else(|| DbError::Tunnel("no home directory for known_hosts".into()))
}

struct Client {
    host: String,
    port: u16,
    known_hosts: PathBuf,
    problem: Arc<Mutex<Option<HostKeyProblem>>>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &keys::PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        let key = match key {
            keys::PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            keys::PublicKeyOrCertificate::Certificate(cert) => {
                PublicKey::from(cert.public_key().clone())
            }
        };
        let changed =
            match keys::check_known_hosts_path(&self.host, self.port, &key, &self.known_hosts) {
                Ok(true) => return Ok(true),
                Ok(false) => false,
                Err(keys::Error::KeyChanged { .. }) => true,
                Err(_) => false,
            };
        *self.problem.lock().unwrap() = Some(HostKeyProblem {
            host: self.host.clone(),
            port: self.port,
            key: key.to_openssh().unwrap_or_default(),
            fingerprint: key.fingerprint(Default::default()).to_string(),
            changed,
            known_hosts: self.known_hosts.clone(),
        });
        Ok(false)
    }
}

/// An open tunnel. Dropping it stops forwarding and closes the SSH session.
pub struct Tunnel {
    pub local_port: u16,
    /// Where the tunnel goes, as the SSH server sees it.
    pub ssh_host: String,
    /// Why forwarding a connection last failed, which the driver can only
    /// see as a closed socket.
    failure: Arc<Mutex<Option<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Tunnel {
    pub fn take_failure(&self) -> Option<String> {
        self.failure.lock().unwrap().take()
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Connects to the SSH server and forwards a new local port to
/// `target_host:target_port`, which the SSH server resolves.
pub async fn open(ssh: &SshConfig, target_host: &str, target_port: u16) -> Result<Tunnel> {
    let config_file = ssh.config_file.clone().or_else(ssh_config::default_path);
    let alias = config_file
        .map(|p| ssh_config::lookup(&p, &ssh.host))
        .unwrap_or_default();
    let host = alias.host_name.clone().unwrap_or_else(|| ssh.host.clone());
    let port = ssh.port.or(alias.port).unwrap_or(22);
    let user = ssh
        .user
        .clone()
        .filter(|u| !u.is_empty())
        .or(alias.user.clone())
        .or_else(|| {
            std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .ok()
        })
        .ok_or_else(|| DbError::Tunnel("no SSH user".into()))?;
    let known_hosts = match &ssh.known_hosts {
        Some(p) => p.clone(),
        None => default_known_hosts()?,
    };

    let problem = Arc::new(Mutex::new(None));
    let handler = Client {
        host: host.clone(),
        port,
        known_hosts,
        problem: problem.clone(),
    };
    let config = Arc::new(client::Config::default());
    let mut session = match client::connect(config, (host.as_str(), port), handler).await {
        Ok(s) => s,
        Err(e) => {
            if let Some(p) = problem.lock().unwrap().take() {
                return Err(DbError::HostKey(Box::new(p)));
            }
            return Err(DbError::Tunnel(format!(
                "couldn't reach {host}:{port}: {e}"
            )));
        }
    };

    let authed = match ssh.auth {
        SshAuth::Password => {
            let password = ssh.secret.clone().unwrap_or_default();
            session.authenticate_password(&user, password).await
        }
        SshAuth::Key => {
            let path = ssh
                .key_file
                .clone()
                .or(alias.identity_file.clone())
                .ok_or_else(|| DbError::Tunnel("no SSH key file".into()))?;
            let key = keys::load_secret_key(&path, ssh.secret.as_deref()).map_err(|e| {
                DbError::Tunnel(match e {
                    keys::Error::KeyIsEncrypted => {
                        format!("{} needs its passphrase", path.display())
                    }
                    e => format!("couldn't read {}: {e}", path.display()),
                })
            })?;
            let hash = session
                .best_supported_rsa_hash()
                .await
                .ok()
                .flatten()
                .flatten();
            session
                .authenticate_publickey(&user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await
        }
        SshAuth::Agent => agent_auth(&mut session, &user).await,
    }
    .map_err(|e| DbError::Tunnel(format!("SSH authentication: {e}")))?;
    if !authed.success() {
        return Err(DbError::Auth(format!(
            "the SSH server didn't accept {user}@{host}"
        )));
    }

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| DbError::Tunnel(format!("couldn't open a local port: {e}")))?;
    let local_port = listener
        .local_addr()
        .map_err(|e| DbError::Tunnel(e.to_string()))?
        .port();
    let session = Arc::new(session);
    let target_host = target_host.to_string();
    let failure = Arc::new(Mutex::new(None));
    let last_failure = failure.clone();
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let session = session.clone();
            let target_host = target_host.clone();
            let failure = failure.clone();
            tokio::spawn(async move {
                let opened = session
                    .channel_open_direct_tcpip(
                        target_host.clone(),
                        target_port.into(),
                        "127.0.0.1",
                        local_port.into(),
                    )
                    .await;
                let channel = match opened {
                    Ok(c) => c,
                    Err(e) => {
                        *failure.lock().unwrap() = Some(match e {
                            russh::Error::ChannelOpenFailure(why) => format!(
                                "the SSH server wouldn't forward to {target_host}:{target_port} ({why:?})"
                            ),
                            e => format!("the SSH session failed: {e}"),
                        });
                        // dropping the socket tells the driver the connection failed
                        return;
                    }
                };
                let mut stream = channel.into_stream();
                let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
            });
        }
    });
    Ok(Tunnel {
        local_port,
        ssh_host: host,
        failure: last_failure,
        task,
    })
}

async fn agent_auth(
    session: &mut client::Handle<Client>,
    user: &str,
) -> std::result::Result<russh::client::AuthResult, russh::Error> {
    #[cfg(unix)]
    let agent = keys::agent::client::AgentClient::connect_env().await;
    #[cfg(windows)]
    let agent =
        keys::agent::client::AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await;
    let mut agent = agent.map_err(|_| russh::Error::NotAuthenticated)?;
    let identities = agent
        .request_identities()
        .await
        .map_err(|_| russh::Error::NotAuthenticated)?;
    let hash = session
        .best_supported_rsa_hash()
        .await
        .ok()
        .flatten()
        .flatten();
    let mut last = Err(russh::Error::NotAuthenticated);
    for identity in identities {
        let key = identity.public_key().into_owned();
        match session
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
        {
            Ok(r) if r.success() => return Ok(r),
            Ok(r) => last = Ok(r),
            Err(_) => {}
        }
    }
    last
}

#[cfg(test)]
mod tests;
