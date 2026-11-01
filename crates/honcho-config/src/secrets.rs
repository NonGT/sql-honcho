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
    fn entry(key: &str) -> Result<keyring::Entry, SecretError> {
        keyring::Entry::new(SERVICE, key).map_err(store_error)
    }
}

impl Secrets for Keyring {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        match Self::entry(key)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(store_error(e)),
        }
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        Self::entry(key)?.set_password(value).map_err(store_error)
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(store_error(e)),
        }
    }
}

fn store_error(e: keyring::Error) -> SecretError {
    let unavailable = matches!(
        e,
        keyring::Error::NoDefaultStore
            | keyring::Error::NoStorageAccess(_)
            | keyring::Error::PlatformFailure(_)
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
