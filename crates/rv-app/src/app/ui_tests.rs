use super::{AddressBookApp, Modal, ViewMode, WindowRoot};
use crate::actions::*;
use gpui::{AppContext, Entity, Modifiers, TestAppContext, VisualTestContext, px};
use gpui_component::Root;
use rv_core::{AddressBook, Connection};
use tempfile::TempDir;

fn setup(
    cx: &mut TestAppContext,
    connections: Vec<Connection>,
) -> (Entity<AddressBookApp>, &mut VisualTestContext, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut book = AddressBook::load(rv_core::StorePaths::in_dir(dir.path().into())).unwrap();
    for connection in connections {
        book.upsert(connection);
    }
    book.save().unwrap();
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::bind_keys(cx);
    });
    let mut entity = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let app = cx.new(|cx| AddressBookApp::with_book(book, false, "Ready".into(), window, cx));
        entity = Some(app.clone());
        let shell = cx.new(|_| WindowRoot::new(app));
        Root::new(shell, window, cx)
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    (entity.unwrap(), cx, dir)
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let bounds = cx.debug_bounds(selector).expect("button must be rendered");
    assert!(bounds.size.width > px(0.) && bounds.size.height > px(0.));
    cx.simulate_mouse_move(bounds.center(), None, Modifiers::default());
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

#[gpui::test]
fn offscreen_save_connection_persists_form_and_clears_password(cx: &mut TestAppContext) {
    let (app, cx, dir) = setup(cx, vec![]);
    cx.simulate_keystrokes("ctrl-n");
    app.update(cx, |app, cx| {
        assert!(matches!(app.modal, Modal::Connection));
        app.remember_password = false;
        cx.notify();
    });
    cx.simulate_input("example.test:2");
    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.name_input
                .update(cx, |input, cx| input.set_value("Office", window, cx));
            app.labels_input.update(cx, |input, cx| {
                input.set_value("work, lab, work", window, cx)
            });
            app.password_input.update(cx, |input, cx| {
                input.set_value("temporary test password", window, cx)
            });
        });
    });
    click(cx, "modal-save");
    app.read_with(cx, |app, cx| {
        assert!(matches!(app.modal, Modal::None));
        assert!(!app.credential_busy);
        assert!(app.password_input.read(cx).unmask_value().is_empty());
    });
    let book = AddressBook::load(rv_core::StorePaths::in_dir(dir.path().into())).unwrap();
    let saved = &book.connections()[0];
    assert_eq!(
        (saved.name.as_str(), saved.host.as_str(), saved.port),
        ("Office", "example.test", 5902)
    );
    assert_eq!(saved.labels, ["lab", "work"]);
    assert!(!saved.remember_password);
    assert!(saved.last_connected.is_none());
}

#[gpui::test]
fn offscreen_invalid_server_keeps_editor_open(cx: &mut TestAppContext) {
    let (app, cx, _dir) = setup(cx, vec![]);
    cx.simulate_keystrokes("ctrl-n");
    click(cx, "modal-connect");
    app.read_with(cx, |app, _| {
        assert!(matches!(app.modal, Modal::Connection));
        assert!(!app.credential_busy);
        assert!(app.book.connections().is_empty());
        assert!(!app.status.is_empty());
        assert_ne!(app.status.as_ref(), "Ready");
    });
}

#[gpui::test]
fn offscreen_busy_editor_blocks_buttons_and_duplicate_actions(cx: &mut TestAppContext) {
    let (app, cx, _dir) = setup(cx, vec![]);
    cx.simulate_keystrokes("ctrl-n");
    cx.simulate_input("example.test");
    app.update(cx, |app, cx| {
        app.credential_busy = true;
        app.status = "Unlocking saved password…".into();
        cx.notify();
    });
    for selector in ["modal-save", "modal-connect", "modal-cancel"] {
        click(cx, selector);
    }
    cx.dispatch_action(NewConnection);
    cx.dispatch_action(CloseModal);
    app.read_with(cx, |app, cx| {
        assert!(app.credential_busy);
        assert!(matches!(app.modal, Modal::Connection));
        assert!(app.book.connections().is_empty());
        assert_eq!(app.server_input.read(cx).value().as_ref(), "example.test");
        assert_eq!(app.status.as_ref(), "Unlocking saved password…");
    });
    app.update(cx, |app, cx| {
        app.credential_busy = false;
        cx.notify();
    });
    click(cx, "modal-cancel");
    app.read_with(cx, |app, _| assert!(matches!(app.modal, Modal::None)));
}

#[gpui::test]
fn offscreen_properties_save_preserves_remembered_password_setting(cx: &mut TestAppContext) {
    let mut connection = Connection::new("Office", "example.test", 5900);
    connection.remember_password = true;
    let id = connection.id;
    let (app, cx, dir) = setup(cx, vec![connection]);
    app.update(cx, |app, _| app.selected = Some(id));
    cx.dispatch_action(OpenProperties);
    app.read_with(cx, |app, cx| {
        assert_eq!(app.editing, Some(id));
        assert!(app.remember_password);
        assert!(app.password_input.read(cx).unmask_value().is_empty());
    });
    click(cx, "modal-save");
    app.read_with(cx, |app, _| assert!(matches!(app.modal, Modal::None)));
    let book = AddressBook::load(rv_core::StorePaths::in_dir(dir.path().into())).unwrap();
    assert!(book.get(id).unwrap().remember_password);
}

#[gpui::test]
fn offscreen_search_and_view_shortcuts_keep_address_book_intact(cx: &mut TestAppContext) {
    let office = Connection::new("Office", "office.test", 5900);
    let lab = Connection::new("Lab", "lab.test", 5900);
    let lab_id = lab.id;
    let (app, cx, _dir) = setup(cx, vec![office, lab]);
    cx.simulate_keystrokes("ctrl-l ctrl-b ctrl-f");
    cx.simulate_input("lab");
    app.read_with(cx, |app, cx| {
        assert!(matches!(app.view_mode, ViewMode::List));
        assert!(app.collapsed);
        assert_eq!(
            app.visible(cx).iter().map(|c| c.id).collect::<Vec<_>>(),
            [lab_id]
        );
        assert_eq!(app.book.connections().len(), 2);
    });
    cx.simulate_keystrokes("escape");
    app.read_with(cx, |app, cx| assert_eq!(app.visible(cx).len(), 2));
}
