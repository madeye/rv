use super::{Phase, SessionOptions, SessionView, dest_rect};
use crate::actions::*;
use gpui::{
    AppContext, Bounds, Entity, Modifiers, MouseButton, MouseExitEvent, Pixels, TestAppContext,
    VisualTestContext, point, px, size,
};
use rv_core::{ConnectRequest, ScaleMode};
use rv_session::{SessionCommand, SessionEvent, SessionHandle, SessionTestPeer};

fn setup(
    cx: &mut TestAppContext,
    view_only: bool,
) -> (Entity<SessionView>, &mut VisualTestContext, SessionTestPeer) {
    setup_with(
        cx,
        view_only,
        800,
        400,
        ScaleMode::Fit,
        size(px(1000.), px(800.)),
    )
}

fn setup_with(
    cx: &mut TestAppContext,
    view_only: bool,
    fb_w: u16,
    fb_h: u16,
    scale: ScaleMode,
    window: gpui::Size<Pixels>,
) -> (Entity<SessionView>, &mut VisualTestContext, SessionTestPeer) {
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::bind_keys(cx);
    });
    let mut connection = rv_core::Connection::new("Offscreen desktop", "example.test", 5900);
    connection.view_only = view_only;
    let request = ConnectRequest::from_connection(&connection, None);
    let (handle, peer) = SessionHandle::test_pair();
    handle.framebuffer.lock().unwrap().resize(fb_w, fb_h);
    peer.events
        .send(SessionEvent::Connected {
            width: fb_w,
            height: fb_h,
        })
        .unwrap();
    peer.events
        .send(SessionEvent::FrameReady { generation: 1 })
        .unwrap();
    let mut entity = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| {
            let mut view = SessionView::with_handle(
                request,
                "Offscreen desktop".into(),
                SessionOptions {
                    scale,
                    pin_toolbar: true,
                    menu_key: "f8".into(),
                    hide_shots: true,
                    thumb_path: None,
                },
                handle,
                window,
                cx,
            );
            view.pump(cx);
            view
        });
        entity = Some(view.clone());
        gpui_component::Root::new(view, window, cx)
    });
    cx.simulate_resize(window);
    cx.update(|window, cx| {
        window.activate_window();
        window.draw(cx).clear(cx);
    });
    cx.run_until_parked();
    (entity.unwrap(), cx, peer)
}

fn picture(view: &Entity<SessionView>, cx: &VisualTestContext) -> Bounds<Pixels> {
    view.read_with(cx, |view, _| view.image_box.get())
}

fn hidden(view: &Entity<SessionView>, cx: &VisualTestContext) -> bool {
    view.read_with(cx, |view, _| view.local_cursor.is_hidden())
}

fn commands(peer: &mut SessionTestPeer) -> Vec<SessionCommand> {
    let mut commands = vec![];
    while let Ok(command) = peer.commands.try_recv() {
        commands.push(command);
    }
    commands
}

#[gpui::test]
fn offscreen_pointer_maps_to_framebuffer_and_releases_buttons(cx: &mut TestAppContext) {
    let (view, cx, mut peer) = setup(cx, false);
    let center = picture(&view, cx).center();
    cx.simulate_mouse_move(center, None, Modifiers::default());
    cx.simulate_mouse_down(center, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_up(center, MouseButton::Left, Modifiers::default());
    let pointers: Vec<_> = commands(&mut peer)
        .into_iter()
        .map(|command| {
            let SessionCommand::Input(vnc::X11Event::PointerEvent(event)) = command else {
                panic!("unexpected command: {command:?}");
            };
            (event.position_x, event.position_y, event.bottons)
        })
        .collect();
    assert_eq!(pointers, [(400, 200, 0), (400, 200, 1), (400, 200, 0)]);
    view.read_with(cx, |view, _| {
        assert_eq!(view.map_pointer(center), Some((400, 200)));
        assert_eq!(view.buttons, 0);
    });
    assert!(hidden(&view, cx));
}

#[gpui::test]
fn offscreen_letterbox_and_local_menu_restore_cursor(cx: &mut TestAppContext) {
    let (view, cx, mut peer) = setup(cx, false);
    let bounds = picture(&view, cx);
    cx.simulate_mouse_move(bounds.center(), None, Modifiers::default());
    assert!(hidden(&view, cx));
    commands(&mut peer);
    cx.simulate_mouse_move(
        bounds.origin + point(px(2.), px(2.)),
        None,
        Modifiers::default(),
    );
    assert!(!hidden(&view, cx));
    assert!(commands(&mut peer).is_empty());
    cx.simulate_mouse_move(bounds.center(), None, Modifiers::default());
    cx.simulate_keystrokes("f8");
    view.read_with(cx, |view, _| assert!(view.show_menu));
    assert!(!hidden(&view, cx));
}

#[gpui::test]
fn offscreen_mouse_exit_and_focus_loss_survive_frame_redraw(cx: &mut TestAppContext) {
    let (view, cx, peer) = setup(cx, false);
    let center = picture(&view, cx).center();
    cx.simulate_mouse_move(center, None, Modifiers::default());
    assert!(hidden(&view, cx));
    cx.simulate_event(MouseExitEvent {
        position: center,
        pressed_button: None,
        modifiers: Modifiers::default(),
    });
    assert!(!hidden(&view, cx));
    peer.events
        .send(SessionEvent::FrameReady { generation: 2 })
        .unwrap();
    view.update(cx, |view, cx| view.pump(cx));
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
    assert!(!hidden(&view, cx));
    cx.simulate_mouse_move(center, None, Modifiers::default());
    assert!(hidden(&view, cx));
    cx.deactivate_window();
    assert!(!hidden(&view, cx));
}

#[gpui::test]
fn offscreen_view_only_blocks_remote_input_and_keeps_cursor(cx: &mut TestAppContext) {
    let (view, cx, mut peer) = setup(cx, true);
    let center = picture(&view, cx).center();
    cx.simulate_mouse_move(center, None, Modifiers::default());
    cx.simulate_click(center, Modifiers::default());
    cx.simulate_keystrokes("a");
    assert!(commands(&mut peer).is_empty());
    assert!(!hidden(&view, cx));
}

#[gpui::test]
fn offscreen_disconnect_restores_cursor_and_stops_pointer_input(cx: &mut TestAppContext) {
    let (view, cx, mut peer) = setup(cx, false);
    let center = picture(&view, cx).center();
    cx.simulate_mouse_move(center, None, Modifiers::default());
    assert!(hidden(&view, cx));
    commands(&mut peer);
    peer.events.send(SessionEvent::Disconnected).unwrap();
    view.update(cx, |view, cx| view.pump(cx));
    cx.run_until_parked();
    cx.simulate_mouse_move(center, None, Modifiers::default());
    assert!(!hidden(&view, cx));
    assert!(commands(&mut peer).is_empty());
    view.read_with(cx, |view, _| assert_eq!(view.phase, Phase::Ended));
}

#[gpui::test]
fn offscreen_close_restores_cursor_and_closes_session(cx: &mut TestAppContext) {
    let (view, cx, mut peer) = setup(cx, false);
    cx.simulate_mouse_move(picture(&view, cx).center(), None, Modifiers::default());
    assert!(hidden(&view, cx));
    commands(&mut peer);
    cx.dispatch_action(SessionClose);
    assert!(!hidden(&view, cx));
    assert!(matches!(
        peer.commands.try_recv(),
        Ok(SessionCommand::Close)
    ));
}

#[gpui::test]
fn offscreen_keyboard_forwards_pairs_and_consumes_menu_shortcuts(cx: &mut TestAppContext) {
    let (view, cx, mut peer) = setup(cx, false);
    for key in ["a", "f8", "escape"] {
        cx.simulate_keystrokes(key);
        // GPUI's typing helper sends key-down only; release explicitly to
        // verify the remote cannot be left with a stuck key.
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse(key).unwrap(),
        });
    }
    let keys: Vec<_> = commands(&mut peer)
        .into_iter()
        .map(|command| {
            let SessionCommand::Input(vnc::X11Event::KeyEvent(event)) = command else {
                panic!("unexpected command: {command:?}");
            };
            (event.keycode, event.down)
        })
        .collect();
    assert_eq!(keys, [(u32::from(b'a'), true), (u32::from(b'a'), false)]);
    view.read_with(cx, |view, _| assert!(!view.show_menu));
}

/// A remote framebuffer narrower (in aspect) than the session window: the
/// window is `1000×800` (1.25:1) while the remote screen is `600×1200`
/// (0.5:1, a portrait phone). Scale to fit must letterbox the full desktop
/// inside the stage and map the pointer only within it.
#[gpui::test]
fn offscreen_fit_letterboxes_when_window_is_wider_than_remote(cx: &mut TestAppContext) {
    let (view, cx, mut peer) = setup_with(
        cx,
        false,
        600,
        1200,
        ScaleMode::Fit,
        size(px(1000.), px(800.)),
    );
    let box_ = picture(&view, cx);

    // The stage spans the window width and sits below the title/toolbar.
    assert_eq!(box_.origin.x, px(0.));
    assert_eq!(box_.size.width, px(1000.));
    assert!(box_.origin.y >= px(0.));
    assert!(box_.size.height > px(400.) && box_.size.height <= px(800.));

    // Fit geometry the app draws and maps to: the desktop becomes a centered
    // strip the height of the stage (height-constrained, since the remote is
    // taller relative to its width than the window is).
    let (dx, dy, dw, dh) = dest_rect(
        f32::from(box_.size.width),
        f32::from(box_.size.height),
        600,
        1200,
        ScaleMode::Fit,
    );
    assert_eq!(dy, 0.);
    assert_eq!(dh, f32::from(box_.size.height));
    assert!(dx > 0.);

    // The strip's corners map to the remote's corners...
    view.read_with(cx, |view, _| {
        let origin = box_.origin + point(px(dx), px(dy));
        let far = box_.origin + point(px(dx + dw - 1.), px(dy + dh - 1.));
        assert_eq!(view.map_pointer(origin), Some((0, 0)));
        // The far edge lands within a pixel of the remote's bottom-right
        // corner; fractional stage sizes round down by at most one pixel.
        assert_eq!(view.map_pointer(far), Some((598, 1198)));
        // ...while the letterbox outside it sends nothing to the remote.
        let letterbox = box_.origin + point(px(0.), px(dh * 0.5));
        assert_eq!(view.map_pointer(letterbox), None);
        let letterbox = box_.origin + point(px(f32::from(box_.size.width) - 1.), px(dh * 0.5));
        assert_eq!(view.map_pointer(letterbox), None);
    });

    // A click inside the strip reaches the remote.
    cx.simulate_mouse_move(
        box_.origin + point(px(dx + dw * 0.5), px(dy + dh * 0.5)),
        None,
        Modifiers::default(),
    );
    cx.simulate_mouse_down(
        box_.origin + point(px(dx + dw * 0.5), px(dy + dh * 0.5)),
        MouseButton::Left,
        Modifiers::default(),
    );
    cx.simulate_mouse_up(
        box_.origin + point(px(dx + dw * 0.5), px(dy + dh * 0.5)),
        MouseButton::Left,
        Modifiers::default(),
    );
    let pointers: Vec<_> = commands(&mut peer)
        .into_iter()
        .map(|command| {
            let SessionCommand::Input(vnc::X11Event::PointerEvent(event)) = command else {
                panic!("unexpected command: {command:?}");
            };
            (event.position_x, event.position_y)
        })
        .collect();
    assert_eq!(pointers, [(300, 600), (300, 600), (300, 600)]);
    // Cursor stays visible over the letterbox (nothing remote there to hide it).
    cx.simulate_mouse_move(
        box_.origin + point(px(1.), px(dh * 0.5)),
        None,
        Modifiers::default(),
    );
    assert!(!hidden(&view, cx));
}

/// Stretch mode with a remote narrower than the window must fill the whole
/// stage and map the pointer across it.
#[gpui::test]
fn offscreen_stretch_fills_stage_when_window_is_wider_than_remote(cx: &mut TestAppContext) {
    let (view, cx, mut peer) = setup_with(
        cx,
        false,
        600,
        1200,
        ScaleMode::Stretch,
        size(px(1000.), px(800.)),
    );
    let box_ = picture(&view, cx);
    assert_eq!(box_.origin.x, px(0.));
    assert_eq!(box_.size.width, px(1000.));
    assert!(box_.size.height > px(400.) && box_.size.height <= px(800.));

    view.read_with(cx, |view, _| {
        // Stretch fills the whole box, so every corner of the stage maps to a
        // remote corner and no point in the stage is dead space.
        let corners = [
            box_.origin,
            box_.origin + point(box_.size.width - px(1.), px(0.)),
            box_.origin + point(px(0.), box_.size.height - px(1.)),
            box_.origin + point(box_.size.width - px(1.), box_.size.height - px(1.)),
        ];
        assert_eq!(view.map_pointer(corners[0]), Some((0, 0)));
        assert_eq!(view.map_pointer(corners[1]), Some((599, 0)));
        assert_eq!(view.map_pointer(corners[2]), Some((0, 1198)));
        assert_eq!(view.map_pointer(corners[3]), Some((599, 1198)));
    });

    // A click at a bottom corner reaches the remote.
    let far = box_.origin + point(box_.size.width - px(1.), box_.size.height - px(1.));
    cx.simulate_mouse_move(far, None, Modifiers::default());
    cx.simulate_click(far, Modifiers::default());
    let pointers: Vec<_> = commands(&mut peer)
        .into_iter()
        .map(|command| {
            let SessionCommand::Input(vnc::X11Event::PointerEvent(event)) = command else {
                panic!("unexpected command: {command:?}");
            };
            (event.position_x, event.position_y)
        })
        .collect();
    assert_eq!(pointers, [(599, 1198), (599, 1198), (599, 1198)]);
    assert!(hidden(&view, cx));
}
