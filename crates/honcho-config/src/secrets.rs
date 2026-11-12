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
