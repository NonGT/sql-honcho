//! Against the sshd and mysql services from docker/compose.yml, with
//! `HONCHO_IT=1`. The SSH server reaches MySQL as `mysql:3306`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;
use crate::mysql::MySqlDriver;
use crate::{ConnectConfig, DbRuntime, QueryEvent, Session, SessionOptions, TlsConfig, TlsMode};

fn enabled() -> bool {
    std::env::var_os("HONCHO_IT").is_some()
}

fn docker(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docker")
        .join(path)
}

fn ssh(auth: SshAuth, known_hosts: &Path) -> SshConfig {
    SshConfig {
        host: "127.0.0.1".into(),
        port: Some(32222),
        user: Some("tunnel".into()),
        auth,
        key_file: match auth {
            SshAuth::Key => Some(docker("ssh/id_ed25519")),
            _ => None,
        },
        secret: match auth {
            SshAuth::Password => Some("Honcho_test1".into()),
            _ => None,
        },
        known_hosts: Some(known_hosts.to_path_buf()),
        // keep the tests away from the developer's own ~/.ssh/config
        config_file: Some(PathBuf::from("/nonexistent")),
    }
}

fn via(ssh: SshConfig) -> ConnectConfig {
    ConnectConfig {
        host: "mysql".into(),
        port: Some(3306),
        user: "honcho".into(),
        password: Some("Honcho_test1".into()),
        database: Some("app".into()),
        ssh: Some(ssh),
        ..Default::default()
    }
}

async fn connect(rt: &DbRuntime, config: ConnectConfig) -> crate::Result<Session> {
    rt.connect(Arc::new(MySqlDriver), config, SessionOptions::default())
        .await
}

async fn query(s: &Session, sql: &str) -> Vec<QueryEvent> {
    let mut h = s.execute(sql);
    let mut out = Vec::new();
    while let Some(e) = h.next().await {
        out.push(e);
    }
    out
}

fn first_value(events: &[QueryEvent]) -> String {
    events
        .iter()
        .find_map(|e| match e {
            QueryEvent::Rows(r) => r
                .first()
                .map(|row| row.last().unwrap().display(200).into_owned()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no rows in {events:?}"))
}

/// A known_hosts file that already trusts the test server, going through
/// the same ask-then-trust steps the app does.
async fn trusted(rt: &DbRuntime, dir: &Path) -> PathBuf {
    let known = dir.join("known_hosts");
    match connect(rt, via(ssh(SshAuth::Password, &known))).await {
        Err(DbError::HostKey(p)) => trust_host_key(&p).unwrap(),
        other => panic!("expected a host key question, got {:?}", other.err()),
    }
    known
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn asks_about_an_unknown_host_key_then_remembers_it() {
    if !enabled() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let known = dir.path().join("known_hosts");
    let rt = DbRuntime::new().unwrap();
    let problem = match connect(&rt, via(ssh(SshAuth::Password, &known))).await {
        Err(DbError::HostKey(p)) => p,
        other => panic!("{:?}", other.err()),
    };
    assert!(!problem.changed);
    assert!(
        problem.fingerprint.starts_with("SHA256:"),
        "{}",
        problem.fingerprint
    );
    trust_host_key(&problem).unwrap();
    assert!(std::fs::read_to_string(&known)
        .unwrap()
        .contains("[127.0.0.1]:32222"));

    let s = connect(&rt, via(ssh(SshAuth::Password, &known)))
        .await
        .unwrap();
    assert_eq!(first_value(&query(&s, "SELECT DATABASE()").await), "app");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticates_with_password_key_and_passphrase() {
    if !enabled() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let rt = DbRuntime::new().unwrap();
    let known = trusted(&rt, dir.path()).await;

    connect(&rt, via(ssh(SshAuth::Key, &known))).await.unwrap();

    let rsa = |secret: Option<&str>| SshConfig {
        key_file: Some(docker("ssh/id_rsa_pass")),
        secret: secret.map(Into::into),
        ..ssh(SshAuth::Key, &known)
    };
    connect(&rt, via(rsa(Some("Honcho_pass")))).await.unwrap();
    match connect(&rt, via(rsa(None))).await.err().unwrap() {
        DbError::Tunnel(m) => assert!(m.contains("passphrase"), "{m}"),
        other => panic!("{other:?}"),
    }

    let wrong = SshConfig {
        secret: Some("nope".into()),
        ..ssh(SshAuth::Password, &known)
    };
    let err = connect(&rt, via(wrong)).await.err().unwrap();
    assert!(matches!(err, DbError::Auth(_)), "{err:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refuses_a_changed_host_key() {
    if !enabled() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let known = dir.path().join("known_hosts");
    // some other key recorded for the server
    let other = std::fs::read_to_string(docker("ssh/id_ed25519.pub")).unwrap();
    let other = other
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    std::fs::write(&known, format!("[127.0.0.1]:32222 {other}\n")).unwrap();

    let rt = DbRuntime::new().unwrap();
    match connect(&rt, via(ssh(SshAuth::Password, &known))).await {
        Err(DbError::HostKey(p)) => {
            assert!(p.changed);
            assert!(trust_host_key(&p).is_err());
        }
        other => panic!("{:?}", other.err()),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follows_an_ssh_config_alias() {
    if !enabled() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let rt = DbRuntime::new().unwrap();
    let known = trusted(&rt, dir.path()).await;
    let config = dir.path().join("config");
    std::fs::write(
        &config,
        format!(
            "Host honcho-jump\n  HostName 127.0.0.1\n  Port 32222\n  User tunnel\n  IdentityFile {}\n",
            docker("ssh/id_ed25519").display()
        ),
    )
    .unwrap();
    let alias = SshConfig {
        host: "honcho-jump".into(),
        port: None,
        user: None,
        auth: SshAuth::Key,
        key_file: None,
        secret: None,
        known_hosts: Some(known),
        config_file: Some(config),
    };
    let s = connect(&rt, via(alias)).await.unwrap();
    assert_eq!(first_value(&query(&s, "SELECT 1").await), "1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_and_cancel_work_through_the_tunnel() {
    if !enabled() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let rt = DbRuntime::new().unwrap();
    let known = trusted(&rt, dir.path()).await;
    let tls = |mode| ConnectConfig {
        tls: TlsConfig {
            mode,
            ca_file: Some(docker("tls/ca.pem")),
            ..Default::default()
        },
        ..via(ssh(SshAuth::Password, &known))
    };

    let s = connect(&rt, tls(TlsMode::VerifyCa)).await.unwrap();
    let cipher = first_value(&query(&s, "SHOW SESSION STATUS LIKE 'Ssl_cipher'").await);
    assert!(!cipher.is_empty());
    // the name checked is mysql, the host as the SSH server sees it, which
    // the test certificate doesn't cover
    let err = connect(&rt, tls(TlsMode::VerifyFull)).await.err().unwrap();
    match err {
        DbError::Tls(m) => assert!(m.contains("\"mysql\""), "{m}"),
        other => panic!("{other:?}"),
    }

    let h = s.execute("SELECT SLEEP(60)");
    let token = h.cancel_token();
    let started = Instant::now();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        token.cancel();
    });
    let mut h = h;
    let mut last = None;
    while let Some(e) = h.next().await {
        last = Some(e);
    }
    assert_eq!(last, Some(QueryEvent::Finished(Err(DbError::Cancelled))));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn says_when_the_ssh_server_cant_forward() {
    if !enabled() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let rt = DbRuntime::new().unwrap();
    let known = trusted(&rt, dir.path()).await;
    let nowhere = ConnectConfig {
        host: "no-such-host.invalid".into(),
        ..via(ssh(SshAuth::Password, &known))
    };
    match connect(&rt, nowhere).await.err().unwrap() {
        DbError::Tunnel(m) => assert!(m.contains("no-such-host.invalid:3306"), "{m}"),
        other => panic!("{other:?}"),
    }
}
