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

    // Touches the real credential store, so run it by hand:
    // cargo test -p honcho-config -- --ignored
    #[test]
    #[ignore]
    fn keyring_round_trip() {
        let key = format!("test-{}/password", uuid::Uuid::new_v4());
        assert_eq!(Keyring.get(&key), Ok(None));
        Keyring.set(&key, "ความลับ").unwrap();
        assert_eq!(Keyring.get(&key), Ok(Some("ความลับ".into())));
        Keyring.delete(&key).unwrap();
        assert_eq!(Keyring.get(&key), Ok(None));
        Keyring.delete(&key).unwrap();
    }
}
