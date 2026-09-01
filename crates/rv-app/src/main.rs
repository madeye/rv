mod actions;
mod app;
mod session_window;
mod theme;

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
            cx.open_window(options, |window, cx| {
                let book = cx.new(|cx| {
                    let mut app = AddressBookApp::new(window, cx);
                    if let Some(target) = connect_to.as_deref() {
                        app.connect_target(target, window, cx);
                    }
                    app
                });
                let shell = cx.new(|_| WindowRoot::new(book));
                cx.new(|cx| Root::new(shell, window, cx))
            })
            .expect("open address book");
        })
        .detach();
    });
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
