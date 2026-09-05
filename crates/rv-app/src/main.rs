mod actions;
mod app;
mod session_window;
mod theme;

use std::cell::RefCell;
use std::rc::Rc;

use gpui::*;
use gpui_component::{Root, TitleBar};

use crate::actions::*;
use crate::app::{AddressBookApp, WindowRoot};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let app = gpui_platform::application().with_assets(gpui_component_assets::Assets);
    app.run(move |cx| {
        #[cfg(target_os = "macos")]
        macos_dock_icon::set();

        gpui_component::init(cx);
        cx.bind_keys([
            KeyBinding::new("cmd-n", NewConnection, Some("AddressBook")),
            KeyBinding::new("ctrl-n", NewConnection, Some("AddressBook")),
            KeyBinding::new("cmd-f", FocusSearch, Some("AddressBook")),
            KeyBinding::new("ctrl-f", FocusSearch, Some("AddressBook")),
            KeyBinding::new("cmd-l", ToggleViewMode, Some("AddressBook")),
            KeyBinding::new("ctrl-l", ToggleViewMode, Some("AddressBook")),
            KeyBinding::new("cmd-i", OpenProperties, Some("AddressBook")),
            KeyBinding::new("ctrl-i", OpenProperties, Some("AddressBook")),
            KeyBinding::new("cmd-d", DuplicateSelected, Some("AddressBook")),
            KeyBinding::new("ctrl-d", DuplicateSelected, Some("AddressBook")),
            KeyBinding::new("cmd-b", ToggleSidebar, Some("AddressBook")),
            KeyBinding::new("ctrl-b", ToggleSidebar, Some("AddressBook")),
            KeyBinding::new("cmd-,", OpenPreferences, Some("AddressBook")),
            KeyBinding::new("ctrl-,", OpenPreferences, Some("AddressBook")),
            KeyBinding::new("enter", ConnectSelected, Some("AddressBook")),
            KeyBinding::new("escape", CloseModal, Some("AddressBook")),
            KeyBinding::new("delete", DeleteSelected, Some("AddressBook")),
            KeyBinding::new("cmd-backspace", DeleteSelected, Some("AddressBook")),
            KeyBinding::new("cmd-q", QuitApp, None),
            KeyBinding::new("cmd-shift-f", SessionFullscreen, Some("Session")),
            KeyBinding::new("cmd-w", SessionClose, Some("Session")),
            KeyBinding::new("f8", SessionMenu, Some("Session")),
        ]);

        let bounds = Bounds::centered(None, size(px(1100.), px(720.)), cx);
        let mut options = TitleBar::window_options();
        options.window_bounds = Some(WindowBounds::Windowed(bounds));
        options.window_min_size = Some(size(px(800.), px(520.)));
        options.app_id = Some("app.rv.viewer".into());

        let connect_to = connect_target(std::env::args().skip(1));

        cx.spawn(async move |cx| {
            let slot: Rc<RefCell<Option<Entity<AddressBookApp>>>> = Rc::default();
            let handle = cx
                .open_window(options, {
                    let slot = slot.clone();
                    move |window, cx| {
                        let book = cx.new(|cx| AddressBookApp::new(window, cx));
                        *slot.borrow_mut() = Some(book.clone());
                        let shell = cx.new(|_| WindowRoot::new(book));
                        cx.new(|cx| Root::new(shell, window, cx))
                    }
                })
                .expect("open address book");
            // Connect only after the window has painted: loading the saved
            // password may pop a modal Keychain prompt, and the user should
            // see the address book behind it rather than nothing.
            let book = slot.borrow().clone();
            if let (Some(target), Some(book)) = (connect_to, book) {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(150))
                    .await;
                let _ = handle.update(cx, |_, window, cx| {
                    book.update(cx, |app, cx| app.connect_target(&target, window, cx));
                });
            }
        })
        .detach();
    });
}

#[cfg(target_os = "macos")]
mod macos_dock_icon {
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    const PNG: &[u8] = include_bytes!("../../../assets/app-icon.png");

    pub fn set() {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let data = NSData::with_bytes(PNG);
        let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        unsafe {
            app.setApplicationIconImage(Some(&image));
        }
    }
}

/// `rv host[:display]` or `rv --connect host[:display]`.
fn connect_target(mut args: impl Iterator<Item = String>) -> Option<String> {
    while let Some(arg) = args.next() {
        if arg == "--connect" {
            return args.next();
        }
        if !arg.starts_with('-') {
            return Some(arg);
        }
    }
    None
}
