//! The PC's identity key (iroh secret key, 32 bytes) lives in Windows Credential Manager,
//! never in a file. It is what pairing binds the phone to, so losing or regenerating it
//! silently would unpair the phone with no warning: a store that fails for any reason other
//! than "no such entry" is an ERROR, never a reason to make a new key.

use keyring::Entry;

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum KeyError {
    #[error("KEY STORE FAILED - {0}")]
    Store(String),
    #[error("STORED KEY IS DAMAGED - {0}")]
    Corrupt(String),
    #[error("NO RANDOMNESS - {0}")]
    Random(String),
}

pub trait KeyStore: Send + Sync {
    /// Ok(None) = there is no key yet (first run). Any other failure is an Err.
    fn load(&self) -> Result<Option<[u8; 32]>, KeyError>;
    fn save(&self, key: &[u8; 32]) -> Result<(), KeyError>;
}

/// Credential Manager entry. `user` separates the real key from a QA profile's.
pub struct CredentialStore {
    entry: Entry,
}

pub const SERVICE: &str = "Local Tugboat";

impl CredentialStore {
    pub fn new(service: &str, user: &str) -> Result<CredentialStore, KeyError> {
        Ok(CredentialStore { entry: Entry::new(service, user).map_err(|e| KeyError::Store(e.to_string()))? })
    }

    /// Remove the entry (tests clean up after themselves).
    pub fn delete(&self) -> Result<(), KeyError> {
        match self.entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(KeyError::Store(e.to_string())),
        }
    }
}

impl KeyStore for CredentialStore {
    fn load(&self) -> Result<Option<[u8; 32]>, KeyError> {
        match self.entry.get_password() {
            Ok(text) => parse_key(&text).map(Some),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(KeyError::Store(e.to_string())),
        }
    }

    fn save(&self, key: &[u8; 32]) -> Result<(), KeyError> {
        // 64 hex characters: far below the 2560-byte Credential Manager limit.
        self.entry.set_password(&hex::encode(key)).map_err(|e| KeyError::Store(e.to_string()))
    }
}

fn parse_key(text: &str) -> Result<[u8; 32], KeyError> {
    let bytes = hex::decode(text.trim()).map_err(|e| KeyError::Corrupt(e.to_string()))?;
    bytes.try_into().map_err(|_| KeyError::Corrupt("expected 64 hex characters".into()))
}

/// The existing key, or a new random one saved and read back. A failing store stops the app.
pub fn load_or_create(store: &dyn KeyStore) -> Result<[u8; 32], KeyError> {
    if let Some(k) = store.load()? {
        return Ok(k);
    }
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|e| KeyError::Random(e.to_string()))?;
    store.save(&key)?;
    // Read it back: a save that silently did not stick would mean a new identity every start.
    match store.load()? {
        Some(k) if k == key => Ok(key),
        _ => Err(KeyError::Store("the key was saved but could not be read back".into())),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// In-memory store whose failures a test can switch on.
    #[derive(Default)]
    pub struct MemStore {
        pub key: Mutex<Option<[u8; 32]>>,
        pub fail_load: Mutex<Option<KeyError>>,
        pub drop_saves: Mutex<bool>,
    }

    impl KeyStore for MemStore {
        fn load(&self) -> Result<Option<[u8; 32]>, KeyError> {
            if let Some(e) = self.fail_load.lock().unwrap().clone() {
                return Err(e);
            }
            Ok(*self.key.lock().unwrap())
        }
        fn save(&self, key: &[u8; 32]) -> Result<(), KeyError> {
            if !*self.drop_saves.lock().unwrap() {
                *self.key.lock().unwrap() = Some(*key);
            }
            Ok(())
        }
    }

    #[test]
    fn first_run_makes_and_saves_a_key_and_later_runs_reuse_it() {
        let s = MemStore::default();
        let a = load_or_create(&s).unwrap();
        assert_ne!(a, [0u8; 32]);
        assert_eq!(load_or_create(&s).unwrap(), a, "second start = same identity");
        assert_eq!(*s.key.lock().unwrap(), Some(a));
    }

    #[test]
    fn a_failing_store_never_makes_a_new_identity() {
        let s = MemStore::default();
        let a = load_or_create(&s).unwrap();
        *s.fail_load.lock().unwrap() = Some(KeyError::Store("access denied".into()));
        assert_eq!(load_or_create(&s), Err(KeyError::Store("access denied".into())));
        assert_eq!(*s.key.lock().unwrap(), Some(a), "the saved key was not replaced");
    }

    #[test]
    fn a_save_that_does_not_stick_is_an_error() {
        let s = MemStore::default();
        *s.drop_saves.lock().unwrap() = true;
        assert!(matches!(load_or_create(&s), Err(KeyError::Store(_))));
    }

    #[test]
    fn damaged_text_is_reported_not_replaced() {
        assert!(matches!(parse_key("nothex"), Err(KeyError::Corrupt(_))));
        assert!(matches!(parse_key("abcd"), Err(KeyError::Corrupt(_))));
        assert_eq!(parse_key(&"ab".repeat(32)).unwrap(), [0xab; 32]);
    }

    /// The real Windows Credential Manager, under a throwaway service name that the test removes.
    #[test]
    fn credential_manager_roundtrip() {
        let user = format!("test-{}", std::process::id());
        let s = CredentialStore::new("Local Tugboat.test", &user).unwrap();
        s.delete().unwrap();
        assert_eq!(s.load().unwrap(), None, "a missing entry is Ok(None), not an error");
        let k = load_or_create(&s).unwrap();
        assert_eq!(s.load().unwrap(), Some(k));
        // A second handle (a new process start) finds the same key.
        let again = CredentialStore::new("Local Tugboat.test", &user).unwrap();
        assert_eq!(load_or_create(&again).unwrap(), k);
        s.delete().unwrap();
        assert_eq!(s.load().unwrap(), None);
    }
}
