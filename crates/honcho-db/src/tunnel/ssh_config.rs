//! The part of `~/.ssh/config` a tunnel needs: what a host alias stands
//! for. Like OpenSSH, the first value found for a key wins.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostEntry {
    pub host_name: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub identity_file: Option<PathBuf>,
}

pub fn default_path() -> Option<PathBuf> {
    home().map(|h| h.join(".ssh").join("config"))
}

fn home() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf())
}

/// Settings for `host` from the config at `path`; empty when the file
/// is missing or nothing matches.
pub fn lookup(path: &Path, host: &str) -> HostEntry {
    std::fs::read_to_string(path)
        .map(|text| parse(&text, host))
        .unwrap_or_default()
}

pub fn parse(text: &str, host: &str) -> HostEntry {
    let mut entry = HostEntry::default();
    // lines before the first Host apply to every host
    let mut applies = true;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match line.split_once(|c: char| c.is_whitespace() || c == '=') {
            Some((k, v)) => (
                k,
                v.trim_start_matches(|c: char| c.is_whitespace() || c == '='),
            ),
            None => continue,
        };
        let value = value.trim().trim_matches('"');
        match key.to_ascii_lowercase().as_str() {
            "host" => applies = host_matches(value, host),
            // Match blocks have conditions we can't evaluate; skip them
            "match" => applies = false,
            _ if !applies => {}
            "hostname" => {
                entry
                    .host_name
                    .get_or_insert_with(|| value.replace("%h", host));
            }
            "port" => {
                if entry.port.is_none() {
                    entry.port = value.parse().ok();
                }
            }
            "user" => {
                entry.user.get_or_insert_with(|| value.to_string());
            }
            "identityfile" => {
                entry
                    .identity_file
                    .get_or_insert_with(|| expand_home(value));
            }
            _ => {}
        }
    }
    entry
}

/// `Host` takes several patterns; `!` negates, and a negated match rules
/// the line out.
fn host_matches(patterns: &str, host: &str) -> bool {
    let mut matched = false;
    for p in patterns.split_whitespace() {
        match p.strip_prefix('!') {
            Some(neg) if glob(neg, host) => return false,
            Some(_) => {}
            None => matched |= glob(p, host),
        }
    }
    matched
}

fn glob(pattern: &str, text: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(b'*'), _) => go(&p[1..], t) || (!t.is_empty() && go(p, &t[1..])),
            (Some(b'?'), Some(_)) => go(&p[1..], &t[1..]),
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b) && go(&p[1..], &t[1..]),
            _ => false,
        }
    }
    go(pattern.as_bytes(), text.as_bytes())
}

pub fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home().map_or_else(|| PathBuf::from(path), |h| h.join(rest)),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "
# global defaults come first
User fallback

Host prod-db *.internal !skip.internal
    HostName 10.0.0.5
    Port 2222
    IdentityFile ~/.ssh/prod_key

Host prod-db
    User ignored-later
    Port 22

Host bastion
    HostName=bastion.example.com
    User \"ops\"

Match host other
    User nope
";

    #[test]
    fn resolves_an_alias() {
        let e = parse(CONFIG, "prod-db");
        assert_eq!(e.host_name.as_deref(), Some("10.0.0.5"));
        assert_eq!(e.port, Some(2222), "first value wins");
        assert_eq!(e.user.as_deref(), Some("fallback"));
        assert!(e.identity_file.unwrap().ends_with(".ssh/prod_key"));
    }

    #[test]
    fn wildcards_negation_and_equals() {
        assert_eq!(
            parse(CONFIG, "api.internal").host_name.as_deref(),
            Some("10.0.0.5")
        );
        assert_eq!(parse(CONFIG, "skip.internal").host_name, None);
        let b = parse(CONFIG, "bastion");
        assert_eq!(b.host_name.as_deref(), Some("bastion.example.com"));
        assert_eq!(b.user.as_deref(), Some("fallback"));
        assert_eq!(parse(CONFIG, "other").user.as_deref(), Some("fallback"));
    }

    #[test]
    fn unknown_hosts_and_missing_files_are_empty() {
        assert_eq!(parse("Host x\n HostName y", "z"), HostEntry::default());
        assert_eq!(
            lookup(Path::new("/no/such/ssh/config"), "x"),
            HostEntry::default()
        );
        assert_eq!(
            parse("Host *\n HostName %h.example.com", "db")
                .host_name
                .as_deref(),
            Some("db.example.com")
        );
    }

    #[test]
    fn globs() {
        assert!(glob("*.example.com", "db.example.com"));
        assert!(glob("db?", "db1"));
        assert!(!glob("db?", "db12"));
        assert!(glob("*", ""));
        assert!(glob("DB", "db"));
    }
}
