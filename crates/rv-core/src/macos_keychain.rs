//! Touch ID-protected credentials, with migration from RV's login Keychain.
//! Call these blocking APIs on a worker thread: macOS may present authentication UI.

use core_foundation::base::TCFType;
use security_framework::{
    access_control::{ProtectionMode, SecAccessControl},
    os::macos::keychain::{SecKeychain, SecPreferencesDomain},
    passwords::{self, AccessControlOptions, PasswordOptions},
};

use crate::{ConnectionId, StoreError, password_key};

const SERVICE: &str = "rv";
const ITEM_NOT_FOUND: i32 = -25300;
const USER_CANCELLED: i32 = -128;
const MISSING_ENTITLEMENT: i32 = -34018;

trait PasswordStore {
    fn load(&self) -> Result<Option<String>, StoreError>;
    fn save(&self, password: &str) -> Result<(), StoreError>;
    fn delete(&self) -> Result<(), StoreError>;
}

struct KeychainEntry {
    account: String,
}

impl KeychainEntry {
    fn options(&self) -> PasswordOptions {
        let mut options = PasswordOptions::new_generic_password(SERVICE, &self.account);
        options.use_protected_keychain();
        options
    }
}

impl PasswordStore for KeychainEntry {
    fn load(&self) -> Result<Option<String>, StoreError> {
        match passwords::generic_password(self.options()) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| StoreError::Keyring("saved password is not UTF-8".into())),
            Err(error) if error.code() == ITEM_NOT_FOUND => Ok(None),
            Err(error) => Err(keychain_error(error)),
        }
    }

    fn save(&self, password: &str) -> Result<(), StoreError> {
        let mut options = self.options();
        {
            // Let macOS prefer Touch ID, with its standard password fallback
            // when biometrics are unavailable or temporarily locked out.
            let access = SecAccessControl::create_with_protection(
                Some(ProtectionMode::AccessibleWhenUnlockedThisDeviceOnly),
                AccessControlOptions::USER_PRESENCE.bits(),
            )
            .map_err(keychain_error)?;
            options.set_access_control(access);
        }
        passwords::set_generic_password_options(password.as_bytes(), options)
            .map_err(keychain_error)
    }

    fn delete(&self) -> Result<(), StoreError> {
        match passwords::delete_generic_password_options(self.options()) {
            Ok(()) => Ok(()),
            Err(error) if error.code() == ITEM_NOT_FOUND => Ok(()),
            Err(error) => Err(keychain_error(error)),
        }
    }
}

fn keychain_error(error: security_framework::base::Error) -> StoreError {
    match error.code() {
        USER_CANCELLED => StoreError::AuthenticationCancelled,
        MISSING_ENTITLEMENT => StoreError::Keyring(
            "Touch ID requires a signed RV app with its Keychain provisioning profile".into(),
        ),
        _ => StoreError::Keyring(error.to_string()),
    }
}

// SecItem's macOS compatibility shim can search/delete BOTH keychain types.
// Use the file-Keychain APIs explicitly so legacy cleanup cannot delete the
// newly migrated Data Protection entry with the same service and account.
struct LegacyEntry {
    account: String,
}

impl LegacyEntry {
    fn keychain(&self) -> Result<SecKeychain, StoreError> {
        SecKeychain::default_for_domain(SecPreferencesDomain::User).map_err(keychain_error)
    }
}

impl PasswordStore for LegacyEntry {
    fn load(&self) -> Result<Option<String>, StoreError> {
        match self
            .keychain()?
            .find_generic_password(SERVICE, &self.account)
        {
            Ok((bytes, _)) => String::from_utf8(bytes.to_vec())
                .map(Some)
                .map_err(|_| StoreError::Keyring("saved password is not UTF-8".into())),
            Err(error) if error.code() == ITEM_NOT_FOUND => Ok(None),
            Err(error) => Err(keychain_error(error)),
        }
    }

    fn save(&self, password: &str) -> Result<(), StoreError> {
        self.keychain()?
            .set_generic_password(SERVICE, &self.account, password.as_bytes())
            .map_err(keychain_error)
    }

    fn delete(&self) -> Result<(), StoreError> {
        let (_, item) = match self
            .keychain()?
            .find_generic_password(SERVICE, &self.account)
        {
            Ok(found) => found,
            Err(error) if error.code() == ITEM_NOT_FOUND => return Ok(()),
            Err(error) => return Err(keychain_error(error)),
        };
        // The safe wrapper discards deletion errors. Check the native status so
        // callers can retry a cancelled/failed cleanup instead of losing track.
        // SAFETY: item owns a valid file-Keychain item reference for this call.
        let status = unsafe {
            security_framework_sys::keychain_item::SecKeychainItemDelete(item.as_concrete_TypeRef())
        };
        if status == 0 || status == ITEM_NOT_FOUND {
            Ok(())
        } else {
            Err(keychain_error(security_framework::base::Error::from_code(
                status,
            )))
        }
    }
}

fn stores(id: ConnectionId) -> (KeychainEntry, LegacyEntry) {
    (
        KeychainEntry {
            account: password_key(id),
        },
        LegacyEntry {
            account: password_key(id),
        },
    )
}

fn load_from(
    protected: &impl PasswordStore,
    legacy: &impl PasswordStore,
) -> Result<Option<String>, StoreError> {
    // A denied/cancelled protected read must never fall back to the old entry.
    let password = match protected.load()? {
        Some(password) => password,
        None => {
            let Some(password) = legacy.load()? else {
                return Ok(None);
            };
            // Preserve the original until the protected write has succeeded.
            protected.save(&password)?;
            password
        }
    };
    // Retry cleanup if a previous migration wrote the protected copy but could
    // not remove the legacy item. Never silently leave an unprotected duplicate.
    legacy.delete()?;
    Ok(Some(password))
}

pub fn load_password(id: ConnectionId) -> Result<Option<String>, StoreError> {
    let (protected, legacy) = stores(id);
    load_from(&protected, &legacy)
}

pub fn save_password(id: ConnectionId, password: &str) -> Result<(), StoreError> {
    let (protected, legacy) = stores(id);
    protected.save(password)?;
    legacy.delete()
}

pub fn delete_password(id: ConnectionId) -> Result<(), StoreError> {
    let (protected, legacy) = stores(id);
    // Attempt both stores even if one is unavailable.
    let protected_result = protected.delete();
    let legacy_result = legacy.delete();
    protected_result.and(legacy_result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    struct FakeStore {
        password: RefCell<Option<String>>,
        deny_read: Cell<bool>,
        deny_write: Cell<bool>,
        deny_delete: Cell<bool>,
        reads: Cell<usize>,
    }

    impl PasswordStore for FakeStore {
        fn load(&self) -> Result<Option<String>, StoreError> {
            self.reads.set(self.reads.get() + 1);
            if self.deny_read.get() {
                return Err(StoreError::AuthenticationCancelled);
            }
            Ok(self.password.borrow().clone())
        }

        fn save(&self, password: &str) -> Result<(), StoreError> {
            if self.deny_write.get() {
                return Err(StoreError::Keyring("write failed".into()));
            }
            *self.password.borrow_mut() = Some(password.into());
            Ok(())
        }

        fn delete(&self) -> Result<(), StoreError> {
            if self.deny_delete.get() {
                return Err(StoreError::AuthenticationCancelled);
            }
            self.password.borrow_mut().take();
            Ok(())
        }
    }

    #[test]
    fn migration_preserves_legacy_until_protected_write_succeeds() {
        let protected = FakeStore::default();
        let legacy = FakeStore::default();
        legacy.save("test credential").unwrap();
        protected.deny_write.set(true);
        assert!(load_from(&protected, &legacy).is_err());
        assert!(legacy.password.borrow().is_some());
        protected.deny_write.set(false);
        assert_eq!(
            load_from(&protected, &legacy).unwrap().as_deref(),
            Some("test credential")
        );
        assert!(legacy.password.borrow().is_none());
        assert_eq!(
            protected.password.borrow().as_deref(),
            Some("test credential")
        );
    }

    #[test]
    fn cancelled_authentication_never_reads_legacy_or_returns_a_password() {
        let protected = FakeStore::default();
        let legacy = FakeStore::default();
        legacy.save("test credential").unwrap();
        protected.deny_read.set(true);
        assert!(matches!(
            load_from(&protected, &legacy),
            Err(StoreError::AuthenticationCancelled)
        ));
        assert_eq!(legacy.reads.get(), 0);
        assert!(legacy.password.borrow().is_some());
    }

    #[test]
    fn incomplete_migration_retries_cleanup_without_overwriting_new_password() {
        let protected = FakeStore::default();
        let legacy = FakeStore::default();
        legacy.save("old test credential").unwrap();
        legacy.deny_delete.set(true);
        assert!(load_from(&protected, &legacy).is_err());
        protected.save("new test credential").unwrap();
        legacy.deny_delete.set(false);
        assert_eq!(
            load_from(&protected, &legacy).unwrap().as_deref(),
            Some("new test credential")
        );
        assert_eq!(legacy.reads.get(), 1);
        assert!(legacy.password.borrow().is_none());
    }
}
