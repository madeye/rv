use std::cell::Cell;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    Disableable as _, Icon, IconName, Selectable as _, Sizable, StyledExt, TitleBar,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use image::{ImageBuffer, Rgba};
use smallvec::SmallVec;

use rv_core::{CAD_KEYSYMS, ConnectRequest, Keyboard, ScaleMode, XK_ALT_L, XK_CONTROL_L};
use rv_session::{SessionEvent, SessionHandle};

use crate::actions::*;
use crate::theme;

pub struct SessionOptions {
    pub scale: ScaleMode,
    pub pin_toolbar: bool,
    pub menu_key: String,
    pub hide_shots: bool,
    pub thumb_path: Option<PathBuf>,
}

/// Pixels of scroll per RFB wheel click. One notch of a mouse wheel is one
/// line, which GPUI reports as this many pixels; trackpads accumulate.
const WHEEL_STEP: f32 = 20.0;

const BTN_LEFT: u8 = 1;
const BTN_MIDDLE: u8 = 2;
const BTN_RIGHT: u8 = 4;
const BTN_WHEEL_UP: u8 = 8;
const BTN_WHEEL_DOWN: u8 = 16;
const BTN_WHEEL_LEFT: u8 = 32;
const BTN_WHEEL_RIGHT: u8 = 64;

pub fn open(req: ConnectRequest, title: String, session: SessionOptions, cx: &mut App) {
    // Open after the current window update finishes. Nesting `open_window`
    // inside another window's constructor tears the session window down.
    cx.spawn(async move |cx| {
        let mut window_options = TitleBar::window_options();
        window_options.window_bounds = Some(WindowBounds::Windowed(Bounds {
            origin: point(px(72.), px(64.)),
            size: size(px(1280.), px(800.)),
        }));
        window_options.window_min_size = Some(size(px(640.), px(400.)));
        window_options.titlebar = Some(TitlebarOptions {
            title: Some(title.clone().into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(9.), px(9.))),
        });
        let _ = cx.open_window(window_options, move |window, cx| {
            let view = cx.new(|cx| SessionView::new(req, title, session, window, cx));
            cx.new(|cx| gpui_component::Root::new(view, window, cx))
        });
    })
    .detach();
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Connecting,
    Connected,
    /// The session is over; `error` says whether it failed.
    Ended,
}

pub struct SessionView {
    req: ConnectRequest,
    handle: SessionHandle,
    title: SharedString,
    status: SharedString,
    phase: Phase,
    scale: ScaleMode,
    pin_toolbar: bool,
    toolbar_open: bool,
    menu_key: String,
    hide_shots: bool,
    thumb_path: Option<PathBuf>,
    show_menu: bool,
    info_open: bool,
    buttons: u8,
    wheel_accum: Point<f32>,
    last_generation: u64,
    render_image: Option<Arc<RenderImage>>,
    fb_w: u16,
    fb_h: u16,
    error: Option<SharedString>,
    /// Where the remote picture was last painted, in window pixels. Filled
    /// in by a probe element so pointer mapping never guesses chrome sizes.
    image_box: Rc<Cell<Bounds<Pixels>>>,
    keys: Keyboard,
    focus: FocusHandle,
    canvas_hovered: bool,
    pointer_in_window: bool,
    local_cursor: local_cursor::LocalCursor,
    _cursor_subscriptions: Vec<Subscription>,
}

impl SessionView {
    fn new(
        req: ConnectRequest,
        title: String,
        options: SessionOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let handle = SessionHandle::spawn(req.clone());
        Self::with_handle(req, title, options, handle, window, cx)
    }

    fn with_handle(
        req: ConnectRequest,
        title: String,
        options: SessionOptions,
        handle: SessionHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let cursor_subscriptions = vec![
            cx.observe_window_activation(window, Self::update_local_cursor),
            cx.on_focus(&focus, window, Self::update_local_cursor),
            cx.on_blur(&focus, window, Self::update_local_cursor),
        ];
        let SessionOptions {
            scale,
            pin_toolbar,
            menu_key,
            hide_shots,
            thumb_path,
        } = options;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                if this.update(cx, |this, cx| this.pump(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        Self {
            req,
            handle,
            title: title.into(),
            status: "Connecting…".into(),
            phase: Phase::Connecting,
            scale,
            pin_toolbar,
            toolbar_open: pin_toolbar,
            menu_key,
            hide_shots,
            thumb_path,
            show_menu: false,
            info_open: false,
            buttons: 0,
            wheel_accum: point(0.0, 0.0),
            last_generation: 0,
            render_image: None,
            fb_w: 0,
            fb_h: 0,
            error: None,
            image_box: Rc::new(Cell::new(Bounds::default())),
            keys: Keyboard::new(),
            focus,
            canvas_hovered: false,
            pointer_in_window: true,
            local_cursor: local_cursor::LocalCursor::default(),
            _cursor_subscriptions: cursor_subscriptions,
        }
    }

    fn update_local_cursor(&mut self, window: &mut Window, _: &mut Context<Self>) {
        // The server paints its pointer into the framebuffer. Only hide ours
        // over the live picture; letterboxing and local controls keep a cursor.
        let hidden = self.phase == Phase::Connected
            && self.render_image.is_some()
            && !self.view_only()
            && !self.show_menu
            && self.canvas_hovered
            && self.pointer_in_window
            && window.is_window_active()
            && self.focus.is_focused(window)
            && self.map_pointer(window.mouse_position()).is_some();
        self.local_cursor.set_hidden(hidden);
    }

    fn view_only(&self) -> bool {
        self.req.view_only
    }

    fn host(&self) -> String {
        format!("{}:{}", self.req.host, self.req.port)
    }

    fn pump(&mut self, cx: &mut Context<Self>) {
        // Rebuild the GPU image at most once per tick: each rebuild copies the
        // whole framebuffer on the UI thread, and this thread also delivers key
        // events. Stalling it delays key-ups, which the remote turns into
        // auto-repeat.
        let mut frame = None;
        let mut changed = false;
        for ev in self.handle.drain() {
            changed = true;
            match ev {
                SessionEvent::Status(s) => self.status = s.into(),
                SessionEvent::Connected { width, height } => {
                    self.fb_w = width;
                    self.fb_h = height;
                    self.phase = Phase::Connected;
                    self.status = format!("{width}×{height}").into();
                    self.error = None;
                }
                SessionEvent::FrameReady { generation } => frame = Some(generation),
                SessionEvent::Clipboard(text) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                    self.status = "Clipboard received".into();
                }
                SessionEvent::Bell => self.status = "Bell".into(),
                SessionEvent::Error(e) => {
                    self.error = Some(e.clone().into());
                    self.status = e.into();
                }
                SessionEvent::Disconnected => {
                    self.local_cursor.set_hidden(false);
                    if self.error.is_none() {
                        self.status = "Disconnected".into();
                    }
                    self.phase = Phase::Ended;
                    self.buttons = 0;
                    self.save_thumb();
                }
            }
        }
        if let Some(generation) = frame
            && generation != self.last_generation
        {
            self.last_generation = generation;
            self.rebuild_image(cx);
        }
        if changed {
            cx.notify();
        }
    }

    fn rebuild_image(&mut self, cx: &mut App) {
        let snapshot = {
            let Ok(fb) = self.handle.framebuffer.lock() else {
                return;
            };
            self.fb_w = fb.width;
            self.fb_h = fb.height;
            if fb.width == 0 || fb.height == 0 {
                return;
            }
            // The compositor already stores BGRA, which is what RenderImage
            // uploads, so this is a plain copy.
            (fb.width, fb.height, fb.pixels.clone())
        };
        let (width, height, pixels) = snapshot;
        let Some(buf) = ImageBuffer::<Rgba<u8>, _>::from_raw(width as u32, height as u32, pixels)
        else {
            return;
        };
        let image = Arc::new(RenderImage::new(SmallVec::from_elem(
            image::Frame::new(buf),
            1,
        )));
        if let Some(old) = self.render_image.replace(image) {
            cx.drop_image(old, None);
        }
    }

    fn save_thumb(&self) {
        if self.hide_shots {
            return;
        }
        let Some(path) = &self.thumb_path else {
            return;
        };
        if let Ok(fb) = self.handle.framebuffer.lock()
            && let Some(png) = fb.thumbnail_png(320)
        {
            let _ = std::fs::write(path, png);
        }
    }

    fn map_pointer(&self, position: Point<Pixels>) -> Option<(u16, u16)> {
        let bounds = self.image_box.get();
        let box_w = f32::from(bounds.size.width);
        let box_h = f32::from(bounds.size.height);
        let local_x = f32::from(position.x - bounds.origin.x);
        let local_y = f32::from(position.y - bounds.origin.y);
        map_to_fb(
            local_x, local_y, box_w, box_h, self.fb_w, self.fb_h, self.scale,
        )
    }

    fn send_pointer(&mut self, x: u16, y: u16) {
        if self.view_only() || self.phase != Phase::Connected {
            return;
        }
        self.handle.pointer(x, y, self.buttons);
    }

    fn pointer_at(&mut self, position: Point<Pixels>) {
        if let Some((x, y)) = self.map_pointer(position) {
            self.send_pointer(x, y);
        }
    }

    fn button(&mut self, bit: u8, down: bool, position: Point<Pixels>) {
        if down {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        self.pointer_at(position);
    }

    fn wheel(&mut self, ev: &ScrollWheelEvent) {
        let delta = ev.delta.pixel_delta(px(WHEEL_STEP));
        self.wheel_accum.x += f32::from(delta.x);
        self.wheel_accum.y += f32::from(delta.y);
        let Some((x, y)) = self.map_pointer(ev.position) else {
            self.wheel_accum = point(0.0, 0.0);
            return;
        };
        let mut clicks: Vec<u8> = Vec::new();
        while self.wheel_accum.y <= -WHEEL_STEP {
            self.wheel_accum.y += WHEEL_STEP;
            clicks.push(BTN_WHEEL_DOWN);
        }
        while self.wheel_accum.y >= WHEEL_STEP {
            self.wheel_accum.y -= WHEEL_STEP;
            clicks.push(BTN_WHEEL_UP);
        }
        while self.wheel_accum.x <= -WHEEL_STEP {
            self.wheel_accum.x += WHEEL_STEP;
            clicks.push(BTN_WHEEL_RIGHT);
        }
        while self.wheel_accum.x >= WHEEL_STEP {
            self.wheel_accum.x -= WHEEL_STEP;
            clicks.push(BTN_WHEEL_LEFT);
        }
        for bit in clicks {
            self.buttons |= bit;
            self.send_pointer(x, y);
            self.buttons &= !bit;
            self.send_pointer(x, y);
        }
    }

    fn send_keys(&mut self, events: impl IntoIterator<Item = (u32, bool)>) {
        if self.view_only() || self.phase != Phase::Connected {
            return;
        }
        for (keysym, down) in events {
            self.handle.key(keysym, down);
        }
    }

    fn tap_key(&mut self, keysym: u32) {
        self.send_keys([(keysym, true), (keysym, false)]);
    }

    fn send_cad(&mut self) {
        let down = CAD_KEYSYMS.iter().map(|k| (*k, true));
        let up = CAD_KEYSYMS.iter().rev().map(|k| (*k, false));
        self.send_keys(down.chain(up));
    }

    fn send_clipboard(&mut self, cx: &App) {
        if self.view_only() || self.phase != Phase::Connected {
            return;
        }
        if let Some(item) = cx.read_from_clipboard()
            && let Some(text) = item.text()
        {
            if text.chars().any(|c| c as u32 > 255) {
                self.status = "Clipboard not sent: RFB carries Latin-1 text only".into();
                return;
            }
            self.handle.copy_text(text);
            self.status = "Clipboard sent".into();
        }
    }

    fn toggle_fullscreen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.toggle_fullscreen();
        cx.notify();
    }

    fn set_scale(&mut self, scale: ScaleMode, cx: &mut Context<Self>) {
        self.scale = scale;
        cx.notify();
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        if self.phase == Phase::Ended {
            return;
        }
        self.save_thumb();
        let released = self.keys.release_all();
        self.send_keys(released);
        self.handle.close();
        self.status = "Disconnecting…".into();
        cx.notify();
    }

    fn reconnect(&mut self, cx: &mut Context<Self>) {
        self.local_cursor.set_hidden(false);
        self.handle = SessionHandle::spawn(self.req.clone());
        self.phase = Phase::Connecting;
        self.error = None;
        self.status = "Connecting…".into();
        self.buttons = 0;
        self.last_generation = 0;
        self.keys = Keyboard::new();
        if let Some(old) = self.render_image.take() {
            cx.drop_image(old, None);
        }
        cx.notify();
    }

    fn close_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.local_cursor.set_hidden(false);
        self.disconnect(cx);
        window.remove_window();
    }

    fn consume_key(&self, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        window.prevent_default();
    }

    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        tracing::debug!(
            key,
            key_char = ?event.keystroke.key_char,
            is_held = event.is_held,
            "key down"
        );
        if key.eq_ignore_ascii_case(&self.menu_key) {
            self.show_menu = !self.show_menu;
            cx.notify();
            self.consume_key(window, cx);
            return;
        }
        if self.show_menu && key == "escape" {
            self.show_menu = false;
            cx.notify();
            self.consume_key(window, cx);
            return;
        }
        let mods = event.keystroke.modifiers;
        let mut events = self
            .keys
            .set_modifiers(mods.control, mods.alt, mods.platform);
        events.extend(self.keys.key_down(
            key,
            event.keystroke.key_char.as_deref(),
            mods.shift,
            event.is_held,
        ));
        self.send_keys(events);
        if Keyboard::recognizes(key, event.keystroke.key_char.as_deref()) {
            self.consume_key(window, cx);
        }
    }

    fn handle_key_up(&mut self, event: &KeyUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let mods = event.keystroke.modifiers;
        let mut events = self.keys.key_up(key);
        events.extend(
            self.keys
                .set_modifiers(mods.control, mods.alt, mods.platform),
        );
        self.send_keys(events);
        if Keyboard::recognizes(key, event.keystroke.key_char.as_deref()) {
            self.consume_key(window, cx);
        }
    }

    fn handle_modifiers(&mut self, event: &ModifiersChangedEvent) {
        let events = self.keys.set_modifiers(
            event.modifiers.control,
            event.modifiers.alt,
            event.modifiers.platform,
        );
        self.send_keys(events);
    }

    fn render_toolbar(&self, fullscreen: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let live = self.phase == Phase::Connected && !self.view_only();
        h_flex()
            .h(theme::toolbar_height())
            .w_full()
            .px_2()
            .gap_0p5()
            .items_center()
            .bg(theme::toolbar())
            .text_color(theme::toolbar_fg())
            .child(tool_btn(
                "pin",
                if self.pin_toolbar {
                    IconName::Star
                } else {
                    IconName::StarOff
                },
                if self.pin_toolbar {
                    "Unpin toolbar (auto-hide)"
                } else {
                    "Pin toolbar"
                },
                cx.listener(|this, _, _, cx| {
                    this.pin_toolbar = !this.pin_toolbar;
                    this.toolbar_open = this.pin_toolbar;
                    cx.notify();
                }),
            ))
            .child(toolbar_sep())
            .child(tool_btn(
                "full",
                if fullscreen {
                    IconName::Minimize
                } else {
                    IconName::Maximize
                },
                if fullscreen {
                    "Exit full screen (⇧⌘F)"
                } else {
                    "Full screen (⇧⌘F)"
                },
                cx.listener(|this, _, window, cx| this.toggle_fullscreen(window, cx)),
            ))
            .child(
                Button::new("scale")
                    .ghost()
                    .text_color(theme::toolbar_fg())
                    .icon(IconName::ResizeCorner)
                    .label(self.scale.label())
                    .tooltip("Scaling: click to cycle")
                    .on_click(cx.listener(|this, _, _, cx| {
                        let next = this.scale.cycle();
                        this.set_scale(next, cx);
                    })),
            )
            .child(toolbar_sep())
            .child(
                tool_btn(
                    "cad",
                    IconName::SquareTerminal,
                    "Send Ctrl+Alt+Del",
                    cx.listener(|this, _, _, cx| {
                        this.send_cad();
                        cx.notify();
                    }),
                )
                .disabled(!live),
            )
            .child(key_chip(
                "Ctrl",
                cx.listener(|this, _, _, _| this.tap_key(XK_CONTROL_L)),
            ))
            .child(key_chip(
                "Alt",
                cx.listener(|this, _, _, _| this.tap_key(XK_ALT_L)),
            ))
            .child(key_chip(
                "Win",
                cx.listener(|this, _, _, _| this.tap_key(rv_core::XK_SUPER_L)),
            ))
            .child(key_chip(
                "Tab",
                cx.listener(|this, _, _, _| this.tap_key(rv_core::XK_TAB)),
            ))
            .child(key_chip(
                "Esc",
                cx.listener(|this, _, _, _| this.tap_key(rv_core::XK_ESCAPE)),
            ))
            .child(key_chip(
                "Caps",
                cx.listener(|this, _, _, _| this.tap_key(rv_core::XK_CAPS_LOCK)),
            ))
            .child(toolbar_sep())
            .child(
                tool_btn(
                    "clip",
                    IconName::Copy,
                    "Send clipboard to remote",
                    cx.listener(|this, _, _, cx| {
                        this.send_clipboard(cx);
                        cx.notify();
                    }),
                )
                .disabled(!live),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_xs()
                    .text_color(theme::toolbar_muted())
                    .text_ellipsis()
                    .max_w(px(260.))
                    .child(self.status.clone()),
            )
            .child(
                Button::new("info")
                    .ghost()
                    .text_color(theme::toolbar_fg())
                    .icon(IconName::Info)
                    .selected(self.info_open)
                    .tooltip("Connection info")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.info_open = !this.info_open;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("disc")
                    .ghost()
                    .icon(IconName::WindowClose)
                    .text_color(theme::danger(cx))
                    .tooltip("Disconnect")
                    .disabled(self.phase == Phase::Ended)
                    .on_click(cx.listener(|this, _, _, cx| this.disconnect(cx))),
            )
    }

    /// Auto-hide toolbar: a small handle at the top edge that grows into the
    /// full toolbar while hovered.
    fn render_floating_toolbar(
        &self,
        fullscreen: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                div()
                    .id("float-tb")
                    .occlude()
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        this.toolbar_open = *hovered;
                        cx.notify();
                    }))
                    .child(if self.toolbar_open {
                        div()
                            .rounded_b_lg()
                            .shadow_lg()
                            .overflow_hidden()
                            .child(self.render_toolbar(fullscreen, cx))
                            .into_any_element()
                    } else {
                        div()
                            .px_6()
                            .py_1()
                            .rounded_b_md()
                            .bg(theme::toolbar())
                            .text_color(theme::toolbar_muted())
                            .cursor_pointer()
                            .child(
                                Icon::new(IconName::ChevronDown)
                                    .small()
                                    .text_color(theme::toolbar_muted()),
                            )
                            .into_any_element()
                    }),
            )
    }

    fn render_menu(&self, fullscreen: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let live = self.phase == Phase::Connected && !self.view_only();
        let scale = self.scale;
        div()
            .absolute()
            .top_3()
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                v_flex()
                    .id("session-menu")
                    .occlude()
                    .w(px(280.))
                    .p_1()
                    .gap_0p5()
                    .rounded_lg()
                    .bg(theme::toolbar())
                    .border_1()
                    .border_color(theme::toolbar_line())
                    .shadow_lg()
                    .text_color(theme::toolbar_fg())
                    .child(
                        h_flex()
                            .px_2()
                            .py_1()
                            .justify_between()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::toolbar_muted())
                                    .child(format!("{} menu", self.menu_key.to_ascii_uppercase())),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::toolbar_muted())
                                    .child("Esc to close"),
                            ),
                    )
                    .child(menu_item(
                        "m-full",
                        if fullscreen {
                            IconName::Minimize
                        } else {
                            IconName::Maximize
                        },
                        if fullscreen {
                            "Exit full screen"
                        } else {
                            "Full screen"
                        },
                        cx.listener(|this, _, window, cx| {
                            this.show_menu = false;
                            this.toggle_fullscreen(window, cx);
                        }),
                    ))
                    .child(
                        h_flex()
                            .px_2()
                            .py_1()
                            .gap_1()
                            .items_center()
                            .child(
                                div()
                                    .text_xs()
                                    .w(px(64.))
                                    .text_color(theme::toolbar_muted())
                                    .child("Scaling"),
                            )
                            .children([ScaleMode::Fit, ScaleMode::Actual, ScaleMode::Stretch].map(
                                |mode| {
                                    Button::new(SharedString::from(format!("m-scale-{mode:?}")))
                                        .xsmall()
                                        .when(scale == mode, |b| b.primary())
                                        .when(scale != mode, |b| {
                                            b.ghost().text_color(theme::toolbar_fg())
                                        })
                                        .label(mode.label())
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.set_scale(mode, cx);
                                        }))
                                },
                            )),
                    )
                    .child(
                        menu_item(
                            "m-cad",
                            IconName::SquareTerminal,
                            "Send Ctrl+Alt+Del",
                            cx.listener(|this, _, _, cx| {
                                this.show_menu = false;
                                this.send_cad();
                                cx.notify();
                            }),
                        )
                        .disabled(!live),
                    )
                    .child(
                        menu_item(
                            "m-clip",
                            IconName::Copy,
                            "Send clipboard",
                            cx.listener(|this, _, _, cx| {
                                this.show_menu = false;
                                this.send_clipboard(cx);
                                cx.notify();
                            }),
                        )
                        .disabled(!live),
                    )
                    .child(menu_item(
                        "m-info",
                        IconName::Info,
                        if self.info_open {
                            "Hide connection info"
                        } else {
                            "Connection info"
                        },
                        cx.listener(|this, _, _, cx| {
                            this.show_menu = false;
                            this.info_open = !this.info_open;
                            cx.notify();
                        }),
                    ))
                    .child(menu_item(
                        "m-pin",
                        if self.pin_toolbar {
                            IconName::StarOff
                        } else {
                            IconName::Star
                        },
                        if self.pin_toolbar {
                            "Auto-hide toolbar"
                        } else {
                            "Pin toolbar"
                        },
                        cx.listener(|this, _, _, cx| {
                            this.show_menu = false;
                            this.pin_toolbar = !this.pin_toolbar;
                            this.toolbar_open = this.pin_toolbar;
                            cx.notify();
                        }),
                    ))
                    .child(div().h(px(1.)).my_1().bg(theme::toolbar_line()))
                    .child(if self.phase == Phase::Ended {
                        menu_item(
                            "m-close",
                            IconName::WindowClose,
                            "Close window",
                            cx.listener(|this, _, window, cx| this.close_window(window, cx)),
                        )
                    } else {
                        menu_item(
                            "m-disc",
                            IconName::WindowClose,
                            "Disconnect",
                            cx.listener(|this, _, _, cx| {
                                this.show_menu = false;
                                this.disconnect(cx);
                            }),
                        )
                        .text_color(theme::danger(cx))
                    }),
            )
    }

    fn render_info(&self) -> impl IntoElement {
        let rows: Vec<(&str, String)> = vec![
            ("Server", self.host()),
            (
                "Desktop",
                if self.fb_w > 0 {
                    format!("{}×{}", self.fb_w, self.fb_h)
                } else {
                    "—".into()
                },
            ),
            ("Scaling", self.scale.label().into()),
            ("Encryption", self.req.encryption.label().into()),
            ("Quality", self.req.quality.label().into()),
            (
                "Access",
                if self.view_only() {
                    "View only".into()
                } else {
                    "Full control".into()
                },
            ),
        ];
        v_flex()
            .id("session-info")
            .occlude()
            .absolute()
            .top_3()
            .right_3()
            .p_3()
            .gap_1()
            .rounded_md()
            .bg(theme::toolbar())
            .border_1()
            .border_color(theme::toolbar_line())
            .shadow_lg()
            .text_color(theme::toolbar_fg())
            .children(rows.into_iter().map(|(k, v)| {
                h_flex()
                    .gap_3()
                    .text_xs()
                    .child(div().w(px(70.)).text_color(theme::toolbar_muted()).child(k))
                    .child(div().child(v))
            }))
            .child(
                div()
                    .text_xs()
                    .text_color(theme::toolbar_muted())
                    .mt_1()
                    .child(format!(
                        "{} for the session menu",
                        self.menu_key.to_ascii_uppercase()
                    )),
            )
    }

    fn render_ended(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let failed = self.error.is_some();
        div()
            .id("session-ended")
            .occlude()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::scrim())
            .child(
                v_flex()
                    .w(px(380.))
                    .gap_3()
                    .p_5()
                    .rounded_lg()
                    .bg(theme::card(cx))
                    .border_1()
                    .border_color(theme::line(cx))
                    .shadow_lg()
                    .text_color(theme::ink(cx))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                Icon::new(if failed {
                                    IconName::TriangleAlert
                                } else {
                                    IconName::CircleCheck
                                })
                                .text_color(if failed {
                                    theme::danger(cx)
                                } else {
                                    theme::muted(cx)
                                }),
                            )
                            .child(div().text_lg().font_semibold().child(if failed {
                                "Connection failed"
                            } else {
                                "Disconnected"
                            })),
                    )
                    .child(div().text_sm().text_color(theme::muted(cx)).child(
                        self.error.clone().unwrap_or_else(|| {
                            format!("The session with {} has ended.", self.host()).into()
                        }),
                    ))
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .mt_1()
                            .child(Button::new("ended-close").label("Close").on_click(
                                cx.listener(|this, _, window, cx| this.close_window(window, cx)),
                            ))
                            .child(
                                Button::new("ended-retry")
                                    .primary()
                                    .icon(IconName::Replace)
                                    .label("Reconnect")
                                    .on_click(cx.listener(|this, _, _, cx| this.reconnect(cx))),
                            ),
                    ),
            )
    }

    fn render_picture(&self, cx: &Context<Self>) -> AnyElement {
        let Some(image) = self.render_image.clone() else {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    Icon::new(IconName::Loader)
                        .size_10()
                        .text_color(theme::toolbar_muted()),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(theme::toolbar_muted())
                        .child(self.status.clone()),
                )
                .into_any_element();
        };
        let probe = {
            let cell = self.image_box.clone();
            let update_cursor = cx.listener(|this, hovered: &bool, window, cx| {
                this.canvas_hovered = *hovered;
                this.update_local_cursor(window, cx);
            });
            let on_move = cx.listener(|this, hovered: &bool, window, cx| {
                this.pointer_in_window = true;
                this.canvas_hovered = *hovered;
                this.update_local_cursor(window, cx);
            });
            let on_exit = cx.listener(|this, _: &MouseExitEvent, window, cx| {
                // MouseExited can retain the last in-window coordinates. Keep
                // subsequent video frames from hiding the cursor again.
                this.pointer_in_window = false;
                this.update_local_cursor(window, cx);
            });
            canvas(
                move |bounds, window, _| {
                    cell.set(bounds);
                    window.insert_hitbox(bounds, HitboxBehavior::Normal)
                },
                move |_, hitbox, window, cx| {
                    // Track the actual picture hitbox, including occluding UI.
                    // Div::on_hover suppresses hover during dragging/typing,
                    // which would bring the duplicate cursor back mid-session.
                    let move_hitbox = hitbox.clone();
                    window.on_mouse_event(move |_: &MouseMoveEvent, phase, window, cx| {
                        if phase == DispatchPhase::Capture {
                            on_move(&move_hitbox.should_handle_scroll(window), window, cx);
                        }
                    });
                    window.on_mouse_event(move |event: &MouseExitEvent, phase, window, cx| {
                        if phase == DispatchPhase::Capture {
                            on_exit(event, window, cx);
                        }
                    });
                    let hitbox = hitbox.clone();
                    window.defer(cx, move |window, cx| {
                        update_cursor(&hitbox.should_handle_scroll(window), window, cx);
                    });
                },
            )
            .absolute()
            .inset_0()
        };
        match self.scale {
            ScaleMode::Fit | ScaleMode::Stretch => div()
                .relative()
                .size_full()
                .child(img(image).id("fb").size_full().object_fit(
                    if self.scale == ScaleMode::Fit {
                        ObjectFit::Contain
                    } else {
                        ObjectFit::Fill
                    },
                ))
                .child(probe)
                .into_any_element(),
            ScaleMode::Actual => div()
                .id("fb-scroll")
                .size_full()
                .overflow_scroll()
                .child(
                    // Centered when smaller than the viewport, scrollable when larger.
                    div()
                        .min_w_full()
                        .min_h_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .relative()
                                .flex_shrink_0()
                                .w(px(f32::from(self.fb_w)))
                                .h(px(f32::from(self.fb_h)))
                                .child(img(image).id("fb").size_full().object_fit(ObjectFit::Fill))
                                .child(probe),
                        ),
                )
                .into_any_element(),
        }
    }
}

impl Focusable for SessionView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Drop for SessionView {
    fn drop(&mut self) {
        // Window closed mid-session: keep the last picture as the preview.
        if self.phase == Phase::Connected {
            self.save_thumb();
        }
    }
}

impl Render for SessionView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.update_local_cursor(window, cx);
        let fullscreen = window.is_fullscreen();
        let show_pinned = self.pin_toolbar;

        v_flex()
            .size_full()
            .bg(theme::desktop())
            .text_color(theme::toolbar_fg())
            .key_context("Session")
            .track_focus(&self.focus)
            .child({
                let focus = self.focus.clone();
                canvas(
                    |_, _, _| (),
                    move |_, _, window, cx| {
                        disable_platform_ime(window);
                        window.handle_input(&focus, DisabledIme, cx);
                    },
                )
                .w(px(0.))
                .h(px(0.))
            })
            .on_key_down(cx.listener(|this, ev, window, cx| this.handle_key(ev, window, cx)))
            .on_key_up(cx.listener(|this, ev, window, cx| this.handle_key_up(ev, window, cx)))
            .on_modifiers_changed(cx.listener(|this, ev, _, _| this.handle_modifiers(ev)))
            .on_action(cx.listener(|this, _: &SessionFullscreen, window, cx| {
                this.toggle_fullscreen(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SessionDisconnect, _, cx| this.disconnect(cx)))
            .on_action(cx.listener(|this, _: &SessionClose, window, cx| {
                this.close_window(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SessionCad, _, cx| {
                this.send_cad();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SessionScaleCycle, _, cx| {
                let next = this.scale.cycle();
                this.set_scale(next, cx);
            }))
            .on_action(cx.listener(|this, _: &SessionToggleToolbar, _, cx| {
                this.pin_toolbar = !this.pin_toolbar;
                this.toolbar_open = this.pin_toolbar;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SessionMenu, _, cx| {
                this.show_menu = !this.show_menu;
                cx.notify();
            }))
            .when(!fullscreen, |this| {
                this.child(
                    TitleBar::new().child(
                        h_flex()
                            .w_full()
                            .px_2()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .font_semibold()
                                    .text_color(theme::ink(cx))
                                    .text_ellipsis()
                                    .child(self.title.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::muted(cx))
                                    .child(self.host()),
                            ),
                    ),
                )
            })
            .when(show_pinned, |this| {
                this.child(self.render_toolbar(fullscreen, cx))
            })
            .child(
                div()
                    .id("stage")
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .overflow_hidden()
                    .child(
                        div()
                            .id("vnc-canvas")
                            .size_full()
                            .bg(theme::desktop())
                            .cursor(CursorStyle::Arrow)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                                    this.focus.focus(window, cx);
                                    if this.show_menu {
                                        this.show_menu = false;
                                        cx.notify();
                                    }
                                    this.button(BTN_LEFT, true, ev.position);
                                }),
                            )
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, ev: &MouseUpEvent, _, _| {
                                    this.button(BTN_LEFT, false, ev.position);
                                }),
                            )
                            .on_mouse_down(
                                MouseButton::Middle,
                                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                                    this.focus.focus(window, cx);
                                    this.button(BTN_MIDDLE, true, ev.position);
                                }),
                            )
                            .on_mouse_up(
                                MouseButton::Middle,
                                cx.listener(|this, ev: &MouseUpEvent, _, _| {
                                    this.button(BTN_MIDDLE, false, ev.position);
                                }),
                            )
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                                    this.focus.focus(window, cx);
                                    this.button(BTN_RIGHT, true, ev.position);
                                }),
                            )
                            .on_mouse_up(
                                MouseButton::Right,
                                cx.listener(|this, ev: &MouseUpEvent, _, _| {
                                    this.button(BTN_RIGHT, false, ev.position);
                                }),
                            )
                            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, _| {
                                this.pointer_at(ev.position);
                            }))
                            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _, _| {
                                this.wheel(ev);
                            }))
                            .child(self.render_picture(cx)),
                    )
                    .when(!show_pinned && self.phase != Phase::Ended, |this| {
                        this.child(self.render_floating_toolbar(fullscreen, cx))
                    })
                    .when(self.info_open, |this| this.child(self.render_info()))
                    .when(self.show_menu, |this| {
                        this.child(self.render_menu(fullscreen, cx))
                    })
                    .when(self.phase == Phase::Ended, |this| {
                        this.child(self.render_ended(cx))
                    }),
            )
    }
}

/// Map a point inside the picture box to framebuffer coordinates.
///
/// `box_w`/`box_h` is the element the picture is drawn in; for Fit the image
/// is letterboxed inside it, for Stretch and 1:1 it fills it.
fn map_to_fb(
    local_x: f32,
    local_y: f32,
    box_w: f32,
    box_h: f32,
    fb_w: u16,
    fb_h: u16,
    mode: ScaleMode,
) -> Option<(u16, u16)> {
    if fb_w == 0 || fb_h == 0 || box_w < 1.0 || box_h < 1.0 {
        return None;
    }
    let (dx, dy, dw, dh) = dest_rect(box_w, box_h, fb_w, fb_h, mode);
    if local_x < dx || local_y < dy || local_x >= dx + dw || local_y >= dy + dh {
        return None;
    }
    let fx = (local_x - dx) / dw * fb_w as f32;
    let fy = (local_y - dy) / dh * fb_h as f32;
    Some((
        fx.clamp(0.0, (fb_w as f32 - 1.0).max(0.0)) as u16,
        fy.clamp(0.0, (fb_h as f32 - 1.0).max(0.0)) as u16,
    ))
}

fn dest_rect(
    box_w: f32,
    box_h: f32,
    fb_w: u16,
    fb_h: u16,
    mode: ScaleMode,
) -> (f32, f32, f32, f32) {
    match mode {
        // Stretch fills the box; in 1:1 mode the box *is* the picture.
        ScaleMode::Stretch | ScaleMode::Actual => (0.0, 0.0, box_w, box_h),
        ScaleMode::Fit => {
            let scale = (box_w / fb_w as f32).min(box_h / fb_h as f32);
            let dw = fb_w as f32 * scale;
            let dh = fb_h as f32 * scale;
            ((box_w - dw) * 0.5, (box_h - dh) * 0.5, dw, dh)
        }
    }
}

fn toolbar_sep() -> impl IntoElement {
    div().w(px(1.)).h(px(18.)).mx_1().bg(theme::toolbar_line())
}

fn tool_btn(
    id: &'static str,
    icon: IconName,
    tip: impl Into<SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    Button::new(id)
        .ghost()
        .icon(icon)
        // The toolbar is always dark; the ghost variant would otherwise use
        // the theme foreground, which is near-black in light mode.
        .text_color(theme::toolbar_fg())
        .tooltip(tip)
        .on_click(on_click)
}

fn key_chip(
    label: &'static str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    Button::new(label)
        .xsmall()
        .ghost()
        .text_color(theme::toolbar_fg())
        .label(label)
        .tooltip(format!("Send {label}"))
        .on_click(on_click)
}

fn menu_item(
    id: &'static str,
    icon: IconName,
    label: impl Into<SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    Button::new(id)
        .ghost()
        .small()
        .w_full()
        .justify_start()
        .text_color(theme::toolbar_fg())
        .icon(icon)
        .label(label)
        .on_click(on_click)
}

/// Platform IME client that refuses composition and committed text.
///
/// VNC needs raw key down/up, not composed characters from an input method.
struct DisabledIme;

impl InputHandler for DisabledIme {
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut App,
    ) -> Option<UTF16Selection> {
        None
    }

    fn marked_text_range(&mut self, _: &mut Window, _: &mut App) -> Option<Range<usize>> {
        None
    }

    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<String> {
        None
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        _: &str,
        _: &mut Window,
        _: &mut App,
    ) {
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        _: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        _: &mut App,
    ) {
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut App) {}

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<Bounds<Pixels>> {
        None
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<usize> {
        None
    }

    fn accepts_text_input(&mut self, _: &mut Window, _: &mut App) -> bool {
        false
    }

    fn prefers_ime_for_printable_keys(&mut self, _: &mut Window, _: &mut App) -> bool {
        false
    }

    fn apple_press_and_hold_enabled(&mut self) -> bool {
        false
    }
}

fn disable_platform_ime(window: &Window) {
    macos_ime::disable(window);
}

mod local_cursor {
    /// Owns exactly one balanced hide/unhide pair, even if the window is closed
    /// while the pointer is hidden. Repaints and mouse moves must not stack hides.
    #[derive(Default)]
    pub struct LocalCursor {
        hidden: bool,
    }

    impl LocalCursor {
        #[cfg(test)]
        pub(super) fn is_hidden(&self) -> bool {
            self.hidden
        }

        pub fn set_hidden(&mut self, hidden: bool) {
            if self.hidden != hidden {
                set_platform_hidden(hidden);
                self.hidden = hidden;
            }
        }
    }

    impl Drop for LocalCursor {
        fn drop(&mut self) {
            self.set_hidden(false);
        }
    }

    #[cfg(all(target_os = "macos", not(test)))]
    fn set_platform_hidden(hidden: bool) {
        use objc2_app_kit::NSCursor;

        if hidden {
            NSCursor::hide();
        } else {
            NSCursor::unhide();
        }
    }

    #[cfg(all(not(target_os = "macos"), not(test)))]
    fn set_platform_hidden(_: bool) {}

    #[cfg(test)]
    std::thread_local! {
        static TRANSITIONS: std::cell::RefCell<Vec<bool>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    #[cfg(test)]
    fn set_platform_hidden(hidden: bool) {
        TRANSITIONS.with_borrow_mut(|transitions| transitions.push(hidden));
    }

    #[test]
    fn repeated_updates_and_window_close_balance_cursor_hiding() {
        TRANSITIONS.with_borrow_mut(Vec::clear);
        {
            let mut cursor = LocalCursor::default();
            cursor.set_hidden(false);
            cursor.set_hidden(true);
            cursor.set_hidden(true); // Mouse moves and frame updates.
            cursor.set_hidden(false); // Leave the picture or lose focus.
            cursor.set_hidden(false);
            cursor.set_hidden(true); // Return to the remote desktop.
        } // Closing the window must restore the cursor.
        TRANSITIONS.with_borrow(|transitions| {
            assert_eq!(transitions, &[true, false, true, false]);
        });
        drop(LocalCursor::default());
        TRANSITIONS.with_borrow(|transitions| assert_eq!(transitions.len(), 4));
    }
}

#[cfg(test)]
mod ui_tests;

#[cfg(target_os = "macos")]
mod macos_ime {
    use gpui::Window;
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSArray, NSString};
    use raw_window_handle::RawWindowHandle;

    pub fn disable(window: &Window) {
        let Ok(handle) = raw_window_handle::HasWindowHandle::window_handle(window) else {
            return;
        };
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return;
        };
        let view = appkit.ns_view.as_ptr().cast::<AnyObject>();
        if view.is_null() {
            return;
        }
        unsafe {
            let view = &*view;
            let ctx: *mut AnyObject = msg_send![view, inputContext];
            if ctx.is_null() {
                return;
            }
            let ctx = &*ctx;
            // Empty locale list: no input sources, so CJK/dead-key IMEs cannot attach.
            let empty = NSArray::<NSString>::new();
            let _: () = msg_send![ctx, setAllowedInputSourceLocales: &*empty];
            let _: () = msg_send![ctx, discardMarkedText];
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod macos_ime {
    pub fn disable(_window: &gpui::Window) {}
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the `gpui::*` glob would shadow `#[test]`.
    use super::{ScaleMode, map_to_fb};

    #[test]
    fn fit_letterboxes_and_maps_corners() {
        // 200×100 picture in a 400×400 box scales ×2: drawn at y 100..300, x 0..400.
        assert_eq!(
            map_to_fb(0.0, 100.0, 400.0, 400.0, 200, 100, ScaleMode::Fit),
            Some((0, 0))
        );
        assert_eq!(
            map_to_fb(399.0, 299.0, 400.0, 400.0, 200, 100, ScaleMode::Fit),
            Some((199, 99))
        );
        assert_eq!(
            map_to_fb(200.0, 200.0, 400.0, 400.0, 200, 100, ScaleMode::Fit),
            Some((100, 50))
        );
        assert_eq!(
            map_to_fb(10.0, 10.0, 400.0, 400.0, 200, 100, ScaleMode::Fit),
            None
        );
    }

    #[test]
    fn actual_maps_one_to_one() {
        assert_eq!(
            map_to_fb(17.0, 23.0, 200.0, 100.0, 200, 100, ScaleMode::Actual),
            Some((17, 23))
        );
        assert_eq!(
            map_to_fb(200.0, 0.0, 200.0, 100.0, 200, 100, ScaleMode::Actual),
            None
        );
    }

    #[test]
    fn stretch_scales_each_axis() {
        assert_eq!(
            map_to_fb(200.0, 200.0, 400.0, 400.0, 200, 100, ScaleMode::Stretch),
            Some((100, 50))
        );
    }

    #[test]
    fn empty_framebuffer_maps_nothing() {
        assert_eq!(
            map_to_fb(1.0, 1.0, 100.0, 100.0, 0, 0, ScaleMode::Fit),
            None
        );
    }
}
