use std::collections::HashMap;
use std::sync::Mutex;

const SERVICE: &str = "sql-honcho";

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SecretError(pub String);

/// Where passwords live, keyed by strings like `<connection id>/password`.
pub trait Secrets: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError>;
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError>;
    /// Deleting a missing key is fine.
    fn delete(&self, key: &str) -> Result<(), SecretError>;
}

/// The OS credential store: Keychain, Windows Credential Manager, or the
/// Secret Service on Linux.
pub struct Keyring;

/// Keeps what another store returns for the life of the app. On macOS each
/// Keychain read can be a prompt, and opening a connection reads its
/// password from several places at once (the tab, the browser, completion).
pub struct Cached<S> {
    inner: S,
    /// Held across a read of the inner store, so reads that arrive while one
    /// is waiting on a prompt use its answer rather than asking again.
    cache: Mutex<HashMap<String, String>>,
}

impl<S: Secrets> Cached<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            cache: Mutex::default(),
        }
    }
}

impl<S: Secrets> Secrets for Cached<S> {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(v) = cache.get(key) {
            return Ok(Some(v.clone()));
        }
        let v = self.inner.get(key)?;
        if let Some(v) = &v {
            cache.insert(key.to_string(), v.clone());
        }
        Ok(v)
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        // saving a connection writes its password back unchanged, and on
        // macOS rewriting a Keychain item prompts just like reading it
        if cache.get(key).is_some_and(|v| v == value) {
            return Ok(());
        }
        self.inner.set(key, value)?;
        cache.insert(key.to_string(), value.to_string());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.remove(key);
        self.inner.delete(key)
    }
}

impl Keyring {
    fn entry(key: &str) -> Result<keyring_core::Entry, SecretError> {
        // Looked up again on each call while missing, so a keyring started
        // after the app (Secret Service on Linux) works without a restart.
        if keyring_core::get_default_store().is_none() {
            keyring_core::set_default_store(platform_store().map_err(store_error)?);
        }
        keyring_core::Entry::new(SERVICE, key).map_err(store_error)
    }
}

#[cfg(target_os = "macos")]
fn platform_store() -> keyring_core::Result<std::sync::Arc<keyring_core::CredentialStore>> {
    Ok(apple_native_keyring_store::keychain::Store::new()?)
}

#[cfg(target_os = "windows")]
fn platform_store() -> keyring_core::Result<std::sync::Arc<keyring_core::CredentialStore>> {
    Ok(windows_native_keyring_store::Store::new()?)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_store() -> keyring_core::Result<std::sync::Arc<keyring_core::CredentialStore>> {
    Ok(zbus_secret_service_keyring_store::Store::new()?)
}

impl Secrets for Keyring {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        match Self::entry(key)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(store_error(e)),
        }
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        Self::entry(key)?.set_password(value).map_err(store_error)
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(e) => Err(store_error(e)),
        }
    }
}

/// Keeps every secret in one entry of another store, so on macOS a single
/// "Always Allow" covers all connections rather than one per connection.
/// Secrets saved one entry each by older versions are read from there the
/// first time and moved in.
pub struct Bundled<S> {
    inner: S,
    /// Loaded on first use. A key mapped to None was deleted, so the old
    /// entry under that key isn't read again.
    map: Mutex<Option<Bundle>>,
}

type Bundle = HashMap<String, Option<String>>;

/// The key of the one entry everything goes in.
pub const BUNDLE_KEY: &str = "all-secrets";

impl<S: Secrets> Bundled<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            map: Mutex::default(),
        }
    }

    /// The loaded map, with the lock held; reads that arrive while the
    /// first one waits on a prompt use its answer.
    fn loaded(&self) -> Result<std::sync::MutexGuard<'_, Option<Bundle>>, SecretError> {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if map.is_none() {
            let saved = match self.inner.get(BUNDLE_KEY)? {
                Some(json) => serde_json::from_str(&json)
                    .map_err(|e| SecretError(format!("saved secrets couldn't be read: {e}")))?,
                None => HashMap::new(),
            };
            *map = Some(saved);
        }
        Ok(map)
    }

    fn write(&self, map: &Bundle) -> Result<(), SecretError> {
        let json = serde_json::to_string(map).map_err(|e| SecretError(e.to_string()))?;
        self.inner.set(BUNDLE_KEY, &json)
    }
}

impl<S: Secrets> Secrets for Bundled<S> {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        let mut guard = self.loaded()?;
        let map = guard.as_mut().expect("loaded");
        if let Some(v) = map.get(key) {
            return Ok(v.clone());
        }
        let Some(old) = self.inner.get(key)? else {
            return Ok(None);
        };
        map.insert(key.to_string(), Some(old.clone()));
        // if this fails it's read from the old entry again next time
        let _ = self.write(map);
        Ok(Some(old))
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        let mut guard = self.loaded()?;
        let map = guard.as_mut().expect("loaded");
        if map.get(key).is_some_and(|v| v.as_deref() == Some(value)) {
            return Ok(());
        }
        let before = map.insert(key.to_string(), Some(value.to_string()));
        self.write(map).inspect_err(|_| {
            match before {
                Some(v) => map.insert(key.to_string(), v),
                None => map.remove(key),
            };
        })
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        let mut guard = self.loaded()?;
        let map = guard.as_mut().expect("loaded");
        if matches!(map.get(key), Some(None)) {
            return Ok(());
        }
        let before = map.insert(key.to_string(), None);
        self.write(map).inspect_err(|_| {
            match before {
                Some(v) => map.insert(key.to_string(), v),
                None => map.remove(key),
            };
        })
    }
}

fn store_error(e: keyring_core::Error) -> SecretError {
    let unavailable = matches!(
        e,
        keyring_core::Error::NoDefaultStore
            | keyring_core::Error::NoStorageAccess(_)
            | keyring_core::Error::PlatformFailure(_)
    );
    if unavailable && cfg!(target_os = "linux") {
        SecretError(format!(
            "couldn't reach the system keyring ({e}). Passwords are kept in the \
             Secret Service, so a keyring such as GNOME Keyring or KeePassXC has to be running."
        ))
    } else {
        SecretError(format!("system keyring: {e}"))
    }
}

/// Keeps secrets in memory, for tests.
#[derive(Default)]
pub struct MemorySecrets(Mutex<HashMap<String, String>>);

impl Secrets for MemorySecrets {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        self.0
            .lock()
            .unwrap()
            .insert(key.to_string(), value.to_string());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counts reads and writes, each read taking a while like a prompt does.
    #[derive(Default)]
    struct Slow {
        inner: MemorySecrets,
        reads: std::sync::atomic::AtomicUsize,
        writes: std::sync::atomic::AtomicUsize,
    }

    impl Secrets for Slow {
        fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(50));
            self.inner.get(key)
        }
        fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
            self.writes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.set(key, value)
        }
        fn delete(&self, key: &str) -> Result<(), SecretError> {
            self.inner.delete(key)
        }
    }

    #[test]
    fn one_entry_holds_everything_and_old_ones_move_in() {
        use std::sync::atomic::Ordering::SeqCst;
        let slow = Slow::default();
        // saved one entry each by an older version
        slow.inner.set("a/password", "pa").unwrap();
        slow.inner.set("b/password", "pb").unwrap();
        let store = std::sync::Arc::new(Bundled::new(slow));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let s = store.clone();
                std::thread::spawn(move || s.get("a/password"))
            })
            .collect();
        for t in threads {
            assert_eq!(t.join().unwrap(), Ok(Some("pa".into())));
        }
        // the bundle once, then a's old entry once
        assert_eq!(store.inner.reads.load(SeqCst), 2);
        assert_eq!(store.inner.writes.load(SeqCst), 1);

        // a fresh launch reads only the bundle for a
        let fresh = Bundled::new(Slow {
            inner: MemorySecrets(Mutex::new(store.inner.inner.0.lock().unwrap().clone())),
            ..Default::default()
        });
        assert_eq!(fresh.get("a/password"), Ok(Some("pa".into())));
        assert_eq!(fresh.inner.reads.load(SeqCst), 1);
        assert_eq!(fresh.get("b/password"), Ok(Some("pb".into())));
        assert_eq!(fresh.inner.reads.load(SeqCst), 2, "b moves in too");
        assert_eq!(fresh.get("b/password"), Ok(Some("pb".into())));
        assert_eq!(fresh.get("c/password"), Ok(None));
        assert_eq!(fresh.inner.reads.load(SeqCst), 3, "c isn't anywhere");

        // unchanged isn't written; deleted doesn't come back from the old entry
        let writes = fresh.inner.writes.load(SeqCst);
        fresh.set("a/password", "pa").unwrap();
        assert_eq!(fresh.inner.writes.load(SeqCst), writes);
        fresh.delete("b/password").unwrap();
        assert_eq!(fresh.get("b/password"), Ok(None));
        fresh.set("c/password", "pc").unwrap();
        let again = Bundled::new(Slow {
            inner: MemorySecrets(Mutex::new(fresh.inner.inner.0.lock().unwrap().clone())),
            ..Default::default()
        });
        assert_eq!(again.get("b/password"), Ok(None));
        assert_eq!(again.get("c/password"), Ok(Some("pc".into())));
        assert_eq!(again.inner.reads.load(SeqCst), 1, "all from the bundle");
    }

    #[test]
    fn reads_at_the_same_time_ask_once() {
        let slow = Slow::default();
        slow.inner.set("c/password", "pw").unwrap();
        let cached = std::sync::Arc::new(Cached::new(slow));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let c = cached.clone();
                std::thread::spawn(move || c.get("c/password"))
            })
            .collect();
        for t in threads {
            assert_eq!(t.join().unwrap(), Ok(Some("pw".into())));
        }
        assert_eq!(cached.get("c/password"), Ok(Some("pw".into())));
        let reads = |c: &Cached<Slow>| c.inner.reads.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(reads(&cached), 1);

        // writing back what was read leaves the store alone
        cached.set("c/password", "pw").unwrap();
        let writes = |c: &Cached<Slow>| c.inner.writes.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(writes(&cached), 0);

        // a new value replaces the old without another read
        cached.set("c/password", "new").unwrap();
        assert_eq!(writes(&cached), 1);
        assert_eq!(cached.get("c/password"), Ok(Some("new".into())));
        cached.delete("c/password").unwrap();
        assert_eq!(cached.get("c/password"), Ok(None));
        assert_eq!(reads(&cached), 2);
    }

    // Touches the real credential store, so run it by hand:
    // cargo test -p honcho-config -- --ignored
    #[test]
    #[ignore]
    fn keyring_round_trip() {
        let key = format!("test-{}/password", uuid::Uuid::new_v4());
        let k = Cached::new(Keyring);
        assert_eq!(k.get(&key), Ok(None));
        k.set(&key, "ความลับ").unwrap();
        assert_eq!(k.get(&key), Ok(Some("ความลับ".into())));
        assert_eq!(Keyring.get(&key), Ok(Some("ความลับ".into())));
        k.delete(&key).unwrap();
        assert_eq!(k.get(&key), Ok(None));
        k.delete(&key).unwrap();
    }
}
