use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{Connection, ConnectionId, Preferences};

#[cfg(not(target_os = "macos"))]
const SERVICE: &str = "rv";

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("keychain error: {0}")]
    Keyring(String),
    #[error("Keychain authentication was cancelled")]
    AuthenticationCancelled,
    #[error("unknown connection")]
    UnknownConnection,
}

#[derive(Debug, Clone)]
pub struct StorePaths {
    pub root: PathBuf,
    pub address_book: PathBuf,
    pub prefs: PathBuf,
    pub thumbs: PathBuf,
}

impl StorePaths {
    /// Platform data directory, or `$RV_DATA_DIR` when set (handy for
    /// testing against a scratch address book).
    pub fn default_dir() -> Result<Self, StoreError> {
        if let Some(dir) = std::env::var_os("RV_DATA_DIR").filter(|d| !d.is_empty()) {
            return Ok(Self::in_dir(PathBuf::from(dir)));
        }
        let dirs = directories::ProjectDirs::from("app", "RV", "rv").ok_or_else(|| {
            StoreError::Io(std::io::Error::other("cannot resolve application data dir"))
        })?;
        Ok(Self::in_dir(dirs.data_dir().to_path_buf()))
    }

    pub fn in_dir(root: PathBuf) -> Self {
        Self {
            address_book: root.join("addressbook.json"),
            prefs: root.join("prefs.json"),
            thumbs: root.join("thumbs"),
            root,
        }
    }

    pub fn ensure(&self) -> Result<(), StoreError> {
        fs::create_dir_all(&self.root)?;
        fs::create_dir_all(&self.thumbs)?;
        Ok(())
    }

    pub fn thumb_path(&self, id: ConnectionId) -> PathBuf {
        self.thumbs.join(format!("{id}.png"))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AddressBookFile {
    #[serde(default)]
    connections: Vec<Connection>,
}

#[derive(Debug, Clone)]
pub struct AddressBook {
    paths: StorePaths,
    connections: Vec<Connection>,
    prefs: Preferences,
}

impl AddressBook {
    /// Load strictly: any unreadable file is an error.
    pub fn load(paths: StorePaths) -> Result<Self, StoreError> {
        paths.ensure()?;
        let file: AddressBookFile = read_json(&paths.address_book)?.unwrap_or_default();
        let prefs = read_json(&paths.prefs)?.unwrap_or_default();
        Ok(Self {
            paths,
            connections: file.connections,
            prefs,
        })
    }

    /// Load, but survive corrupt JSON: the bad file is moved aside as
    /// `<name>.broken` so nothing is lost, and a warning describes what
    /// happened. I/O errors (unreadable directory) still fail.
    pub fn load_or_quarantine(paths: StorePaths) -> Result<(Self, Vec<String>), StoreError> {
        paths.ensure()?;
        let mut warnings = Vec::new();
        let file: AddressBookFile = read_json_or_quarantine(&paths.address_book, &mut warnings)?;
        let prefs = read_json_or_quarantine(&paths.prefs, &mut warnings)?;
        Ok((
            Self {
                paths,
                connections: file.connections,
                prefs,
            },
            warnings,
        ))
    }

    pub fn paths(&self) -> &StorePaths {
        &self.paths
    }

    pub fn connections(&self) -> &[Connection] {
        &self.connections
    }

    pub fn prefs(&self) -> &Preferences {
        &self.prefs
    }

    pub fn prefs_mut(&mut self) -> &mut Preferences {
        &mut self.prefs
    }

    pub fn get(&self, id: ConnectionId) -> Option<&Connection> {
        self.connections.iter().find(|c| c.id == id)
    }

    pub fn get_mut(&mut self, id: ConnectionId) -> Option<&mut Connection> {
        self.connections.iter_mut().find(|c| c.id == id)
    }

    pub fn upsert(&mut self, conn: Connection) {
        if let Some(existing) = self.connections.iter_mut().find(|c| c.id == conn.id) {
            *existing = conn;
        } else {
            self.connections.push(conn);
        }
    }

    pub fn remove(&mut self, id: ConnectionId) -> Result<(), StoreError> {
        let before = self.connections.len();
        self.connections.retain(|c| c.id != id);
        if self.connections.len() == before {
            return Err(StoreError::UnknownConnection);
        }
        let thumb = self.paths.thumb_path(id);
        let _ = fs::remove_file(thumb);
        let _ = delete_password(id);
        Ok(())
    }

    pub fn labels(&self) -> Vec<String> {
        let mut labels: Vec<String> = self
            .connections
            .iter()
            .flat_map(|c| c.labels.iter().cloned())
            .collect();
        labels.sort();
        labels.dedup();
        labels
    }

    pub fn recents(&self, n: usize) -> Vec<Connection> {
        let mut list: Vec<Connection> = self
            .connections
            .iter()
            .filter(|c| c.last_connected.is_some())
            .cloned()
            .collect();
        list.sort_by_key(|b| std::cmp::Reverse(b.last_connected));
        list.truncate(n);
        list
    }

    pub fn filtered(&self, query: &str, label: Option<&str>) -> Vec<Connection> {
        let q = query.trim().to_ascii_lowercase();
        self.connections
            .iter()
            .filter(|c| {
                if let Some(label) = label
                    && !c.labels.iter().any(|l| l == label)
                {
                    return false;
                }
                if q.is_empty() {
                    return true;
                }
                c.name.to_ascii_lowercase().contains(&q)
                    || c.host.to_ascii_lowercase().contains(&q)
                    || c.server_display().to_ascii_lowercase().contains(&q)
                    || c.labels.iter().any(|l| l.to_ascii_lowercase().contains(&q))
            })
            .cloned()
            .collect()
    }

    pub fn save(&self) -> Result<(), StoreError> {
        self.paths.ensure()?;
        let file = AddressBookFile {
            connections: self.connections.clone(),
        };
        atomic_write(&self.paths.address_book, &serde_json::to_vec_pretty(&file)?)?;
        atomic_write(&self.paths.prefs, &serde_json::to_vec_pretty(&self.prefs)?)?;
        Ok(())
    }

    pub fn forget_sensitive(&mut self) -> Result<(), StoreError> {
        for conn in &self.connections {
            let _ = delete_password(conn.id);
            let _ = fs::remove_file(self.paths.thumb_path(conn.id));
        }
        for conn in &mut self.connections {
            conn.remember_password = false;
        }
        self.prefs.hide_screenshots = true;
        self.save()
    }
}

pub fn password_key(id: ConnectionId) -> String {
    id.to_string()
}

#[cfg(target_os = "macos")]
pub use crate::macos_keychain::{delete_password, load_password, save_password};

#[cfg(not(target_os = "macos"))]
pub fn save_password(id: ConnectionId, password: &str) -> Result<(), StoreError> {
    let entry = keyring::Entry::new(SERVICE, &password_key(id))
        .map_err(|e| StoreError::Keyring(e.to_string()))?;
    entry
        .set_password(password)
        .map_err(|e| StoreError::Keyring(e.to_string()))
}

#[cfg(not(target_os = "macos"))]
pub fn load_password(id: ConnectionId) -> Result<Option<String>, StoreError> {
    let entry = keyring::Entry::new(SERVICE, &password_key(id))
        .map_err(|e| StoreError::Keyring(e.to_string()))?;
    match entry.get_password() {
        Ok(p) => Ok(Some(p)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(StoreError::Keyring(e.to_string())),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn delete_password(id: ConnectionId) -> Result<(), StoreError> {
    let entry = keyring::Entry::new(SERVICE, &password_key(id))
        .map_err(|e| StoreError::Keyring(e.to_string()))?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(StoreError::Keyring(e.to_string())),
    }
}

/// `Ok(None)` when the file does not exist.
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, StoreError> {
    if !path.exists() {
        return Ok(None);
    }
    let data = fs::read_to_string(path)?;
    Ok(Some(serde_json::from_str(&data)?))
}

fn read_json_or_quarantine<T: serde::de::DeserializeOwned + Default>(
    path: &Path,
    warnings: &mut Vec<String>,
) -> Result<T, StoreError> {
    match read_json(path) {
        Ok(v) => Ok(v.unwrap_or_default()),
        Err(StoreError::Json(e)) => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let aside = path.with_extension("json.broken");
            fs::rename(path, &aside)?;
            warnings.push(format!(
                "{name} was unreadable ({e}) and moved to {}",
                aside.display()
            ));
            Ok(T::default())
        }
        Err(e) => Err(e),
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_book() -> (AddressBook, tempfile_dir::Guard) {
        let guard = tempfile_dir::Guard::new();
        let paths = StorePaths::in_dir(guard.path().to_path_buf());
        (AddressBook::load(paths).unwrap(), guard)
    }

    // Tiny temp-dir helper so we don't take a tempfile crate dependency.
    mod tempfile_dir {
        use std::path::{Path, PathBuf};
        use std::time::{SystemTime, UNIX_EPOCH};

        pub struct Guard(PathBuf);
        impl Guard {
            pub fn new() -> Self {
                let nanos = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let path = std::env::temp_dir().join(format!("rv-test-{nanos}"));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn crud_and_filter() {
        let (mut book, _g) = tmp_book();
        let mut a = Connection::new("office", "10.0.0.8", 5900);
        a.labels.push("lab".into());
        let b = Connection::new("pi4", "192.0.2.4", 5900);
        book.upsert(a.clone());
        book.upsert(b);
        book.save().unwrap();

        let reloaded = AddressBook::load(book.paths.clone()).unwrap();
        assert_eq!(reloaded.connections().len(), 2);
        assert_eq!(reloaded.labels(), vec!["lab".to_string()]);
        assert_eq!(reloaded.filtered("off", None).len(), 1);
        assert_eq!(reloaded.filtered("", Some("lab")).len(), 1);
    }

    #[test]
    fn corrupt_book_is_quarantined() {
        let guard = tempfile_dir::Guard::new();
        let paths = StorePaths::in_dir(guard.path().to_path_buf());
        paths.ensure().unwrap();
        std::fs::write(&paths.address_book, b"{ not json").unwrap();
        assert!(AddressBook::load(paths.clone()).is_err());

        let (book, warnings) = AddressBook::load_or_quarantine(paths.clone()).unwrap();
        assert!(book.connections().is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(!paths.address_book.exists());
        assert!(paths.root.join("addressbook.json.broken").exists());
    }

    #[test]
    fn recents_order() {
        let (mut book, _g) = tmp_book();
        let mut a = Connection::new("a", "a.local", 5900);
        let mut b = Connection::new("b", "b.local", 5900);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        a.last_connected = Some(now - 10);
        b.last_connected = Some(now);
        book.upsert(a);
        book.upsert(b);
        let recents = book.recents(1);
        assert_eq!(recents[0].name, "b");
    }
}
