//! Shared models for the RV VNC viewer.

mod connection;
mod keyboard;
mod keysym;
mod prefs;
mod store;

pub use connection::{
    ConnectRequest, Connection, ConnectionId, EncryptionMode, QualityPreset, ScaleMode,
    parse_server,
};
pub use keyboard::{Keyboard, keysym_for_keystroke};
pub use keysym::{
    CAD_KEYSYMS, XK_ALT_L, XK_CAPS_LOCK, XK_CONTROL_L, XK_DELETE, XK_ESCAPE, XK_SUPER_L, XK_TAB,
    keysym_name, keysym_of,
};
pub use prefs::{Preferences, ThemePref};
pub use store::{
    AddressBook, StoreError, StorePaths, delete_password, load_password, password_key,
    save_password,
};
