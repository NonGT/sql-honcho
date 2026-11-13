//! Whether a saved connection's server answers. Only a TCP connection is
//! opened and closed again: nothing logs in, so checking production often
//! leaves no sessions and can't lock an account.

use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use honcho_config::SavedConnection;
use honcho_db::tunnel::ssh_config;
use honcho_db::DriverKind;

const TIMEOUT: Duration = Duration::from_secs(3);
const WHILE_UP: Duration = Duration::from_secs(120);
const FIRST_RETRY: Duration = Duration::from_secs(30);
const MAX_RETRY: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq)]
pub enum Reach {
    Checking,
    Reachable,
    Unreachable(String),
    /// The server answers but the last login was refused.
    LoginFailed(String),
}

#[derive(Clone, Debug)]
pub struct Probe {
    pub reach: Reach,
    pub checked: Option<Instant>,
    pub next: Instant,
    /// The wait after the last failure; doubles up to five minutes.
    pub retry: Duration,
    pub running: bool,
}

impl Probe {
    pub fn new(now: Instant) -> Self {
        Self {
            reach: Reach::Checking,
            checked: None,
            next: now,
            retry: Duration::ZERO,
            running: false,
        }
    }

    pub fn checked(&mut self, result: Result<(), String>, now: Instant) {
        self.running = false;
        self.checked = Some(now);
        match result {
            Ok(()) => {
                self.retry = Duration::ZERO;
                self.next = now + WHILE_UP;
                // the server being up says nothing about the password
                if !matches!(self.reach, Reach::LoginFailed(_)) {
                    self.reach = Reach::Reachable;
                }
            }
            Err(e) => {
                self.retry = if self.retry.is_zero() {
                    FIRST_RETRY
                } else {
                    (self.retry * 2).min(MAX_RETRY)
                };
                self.next = now + self.retry;
                self.reach = Reach::Unreachable(e);
            }
        }
    }
}

/// Where a check connects to: the SSH server for a tunnelled connection,
/// else the database server. None when there's nothing to dial.
pub fn address(saved: &SavedConnection) -> Option<(String, u16)> {
    if let Some(ssh) = &saved.ssh {
        let alias = ssh_config::default_path()
            .map(|p| ssh_config::lookup(&p, &ssh.host))
            .unwrap_or_default();
        let host = alias.host_name.unwrap_or_else(|| ssh.host.clone());
        return Some((host, ssh.port.or(alias.port).unwrap_or(22)));
    }
    // a named SQL Server instance's port is only known to its browser service
    if saved.host.is_empty() || saved.host.contains('\\') && saved.port.is_none() {
        return None;
    }
    let port = saved.port.or(match saved.driver {
        DriverKind::MySql => Some(3306),
        DriverKind::Postgres => Some(5432),
        DriverKind::SqlServer => Some(1433),
        _ => None,
    })?;
    Some((saved.host.clone(), port))
}

/// Blocks for up to a few seconds; run it off the main thread. None when
/// there's no way to tell.
pub fn check(saved: &SavedConnection) -> Option<Result<(), String>> {
    match saved.driver {
        DriverKind::Fake => return Some(Ok(())),
        DriverKind::Sqlite => {
            let path = ssh_config::expand_home(&saved.host);
            return Some(if path.exists() {
                Ok(())
            } else {
                Err(format!("{} doesn't exist", path.display()))
            });
        }
        _ => {}
    }
    let (host, port) = address(saved)?;
    let addrs: Vec<_> = match (host.as_str(), port).to_socket_addrs() {
        Ok(a) => a.collect(),
        Err(_) => return Some(Err(format!("Can't find {host}"))),
    };
    let mut last = None;
    for a in &addrs {
        match TcpStream::connect_timeout(a, TIMEOUT) {
            Ok(_) => return Some(Ok(())),
            Err(e) => last = Some(e),
        }
    }
    let why = match last.map(|e| e.kind()) {
        Some(std::io::ErrorKind::ConnectionRefused) => "refused",
        Some(std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => "timed out",
        _ => "no answer",
    };
    Some(Err(format!("Can't reach {host}:{port}: {why}")))
}

/// "just now", "3 min ago".
pub fn ago(at: Instant, now: Instant) -> String {
    let secs = now.saturating_duration_since(at).as_secs();
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{} min ago", secs / 60)
    } else {
        format!("{} h ago", secs / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mysql_at(port: u16) -> SavedConnection {
        SavedConnection {
            host: "127.0.0.1".into(),
            port: Some(port),
            ..SavedConnection::new("x", DriverKind::MySql)
        }
    }

    #[test]
    fn a_listening_port_is_reachable_and_a_closed_one_isnt() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_eq!(check(&mysql_at(port)), Some(Ok(())));
        drop(listener);
        let err = check(&mysql_at(port)).unwrap().unwrap_err();
        assert!(err.contains("refused"), "{err}");
    }

    #[test]
    fn failures_back_off_and_recovery_resets() {
        let now = Instant::now();
        let mut p = Probe::new(now);
        let waits: Vec<u64> = (0..6)
            .map(|_| {
                p.checked(Err("down".into()), now);
                (p.next - now).as_secs()
            })
            .collect();
        assert_eq!(waits, [30, 60, 120, 240, 300, 300]);
        p.checked(Ok(()), now);
        assert_eq!(p.reach, Reach::Reachable);
        assert_eq!((p.next - now).as_secs(), 120);

        // reachable doesn't clear a refused login
        p.reach = Reach::LoginFailed("denied".into());
        p.checked(Ok(()), now);
        assert_eq!(p.reach, Reach::LoginFailed("denied".into()));
    }

    #[test]
    fn named_instances_without_a_port_arent_checked() {
        let mut saved = mysql_at(3306);
        assert_eq!(address(&saved), Some(("127.0.0.1".into(), 3306)));
        saved.port = None;
        saved.host = r"db\SQLEXPRESS".into();
        assert_eq!(address(&saved), None);
    }
}
