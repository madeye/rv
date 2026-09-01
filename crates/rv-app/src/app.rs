use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    Disableable as _, Icon, IconName, Root, Sizable, StyledExt, TitleBar,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{ContextMenuExt, PopupMenuItem},
    sidebar::{
        Sidebar, SidebarCollapsible, SidebarGroup, SidebarHeader, SidebarMenu, SidebarMenuItem,
        SidebarToggleButton,
    },
    switch::Switch,
    v_flex,
};

use rv_core::{
    AddressBook, ConnectRequest, Connection, ConnectionId, EncryptionMode, QualityPreset,
    delete_password, load_password, parse_server, save_password,
};

use crate::actions::*;
use crate::session_window;
use crate::theme;

/// Window body: address book plus GPUI Component overlay layers.
///
/// Dialogs must not be painted from inside `AddressBookApp::render` — the
/// dialog builder would `read` that entity while it is still being updated.
pub struct WindowRoot {
    address_book: Entity<AddressBookApp>,
}

impl WindowRoot {
    pub fn new(address_book: Entity<AddressBookApp>) -> Self {
        Self { address_book }
    }
}

impl Render for WindowRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .relative()
            .child(self.address_book.clone())
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Grid,
    List,
}

#[derive(Clone, PartialEq, Eq)]
enum SidebarFilter {
    All,
    Recents,
    Label(String),
}

impl SidebarFilter {
    fn title(&self) -> String {
        match self {
            Self::All => "All connections".into(),
            Self::Recents => "Recents".into(),
            Self::Label(l) => l.clone(),
        }
    }
}

#[derive(Clone)]
enum Modal {
    None,
    Connection,
    DeleteConfirm { id: ConnectionId, name: String },
    Preferences,
}

const CARD_WIDTH: Pixels = px(220.);
const THUMB_HEIGHT: Pixels = px(124.);

pub struct AddressBookApp {
    book: AddressBook,
    read_only: bool,
    search: Entity<InputState>,
    name_input: Entity<InputState>,
    server_input: Entity<InputState>,
    password_input: Entity<InputState>,
    labels_input: Entity<InputState>,
    collapsed: bool,
    view_mode: ViewMode,
    filter: SidebarFilter,
    selected: Option<ConnectionId>,
    remember_password: bool,
    encryption: EncryptionMode,
    quality: QualityPreset,
    view_only: bool,
    shared: bool,
    editing: Option<ConnectionId>,
    modal: Modal,
    status: SharedString,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl AddressBookApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut status: SharedString = "Ready".into();
        let mut read_only = false;
        let book = match rv_core::StorePaths::default_dir()
            .and_then(AddressBook::load_or_quarantine)
        {
            Ok((book, warnings)) => {
                for w in &warnings {
                    tracing::warn!("{w}");
                }
                if let Some(w) = warnings.first() {
                    status = w.clone().into();
                }
                book
            }
            Err(e) => {
                tracing::warn!("address book unavailable: {e}");
                read_only = true;
                status =
                    format!("Address book unavailable ({e}); changes will not be saved").into();
                let scratch = std::env::temp_dir().join(format!("rv-{}", std::process::id()));
                AddressBook::load(rv_core::StorePaths::in_dir(scratch)).expect("temp address book")
            }
        };

        theme::apply(book.prefs().theme, window, cx);

        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search connections…")
                .clean_on_escape()
        });
        let name_input = cx.new(|cx| InputState::new(window, cx).placeholder("Office Mac"));
        let server_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("host, host:1, host::5901, [::1]:5900")
        });
        let password_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("VNC password (optional)")
                .masked(true)
        });
        let labels_input = cx.new(|cx| InputState::new(window, cx).placeholder("work, lab, home"));

        let mut subscriptions = Vec::new();
        for input in [&name_input, &server_input, &password_input, &labels_input] {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                |this, _, ev: &InputEvent, window, cx| {
                    if matches!(ev, InputEvent::PressEnter { .. })
                        && matches!(this.modal, Modal::Connection)
                    {
                        this.submit_connect(true, window, cx);
                    }
                },
            ));
        }
        let this = cx.entity().downgrade();
        subscriptions.push(window.observe_window_appearance(move |window, cx| {
            if let Some(this) = this.upgrade() {
                let pref = this.read(cx).book.prefs().theme;
                theme::apply(pref, window, cx);
            }
        }));

        let focus = cx.focus_handle();
        focus.focus(window, cx);

        Self {
            book,
            read_only,
            search,
            name_input,
            server_input,
            password_input,
            labels_input,
            collapsed: false,
            view_mode: ViewMode::Grid,
            filter: SidebarFilter::All,
            selected: None,
            remember_password: true,
            encryption: EncryptionMode::LetServerChoose,
            quality: QualityPreset::Auto,
            view_only: false,
            shared: true,
            editing: None,
            modal: Modal::None,
            status,
            focus,
            _subscriptions: subscriptions,
        }
    }

    fn persist(&mut self) {
        if self.read_only {
            return;
        }
        if let Err(e) = self.book.save() {
            self.status = format!("Save failed: {e}").into();
        }
    }

    fn set_status(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.status = text.into();
        cx.notify();
    }

    fn query(&self, cx: &App) -> String {
        self.search.read(cx).value().to_string()
    }

    fn visible(&self, cx: &App) -> Vec<Connection> {
        let q = self.query(cx);
        match &self.filter {
            SidebarFilter::All => self.book.filtered(&q, None),
            SidebarFilter::Recents => {
                let matching: HashSet<ConnectionId> =
                    self.book.filtered(&q, None).iter().map(|c| c.id).collect();
                self.book
                    .recents(20)
                    .into_iter()
                    .filter(|c| matching.contains(&c.id))
                    .collect()
            }
            SidebarFilter::Label(label) => self.book.filtered(&q, Some(label)),
        }
    }

    fn open_editor(
        &mut self,
        conn: Option<&Connection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (name, server, labels, password) = match conn {
            Some(c) => (
                c.name.clone(),
                c.server_display(),
                c.labels.join(", "),
                if c.remember_password {
                    load_password(c.id).ok().flatten().unwrap_or_default()
                } else {
                    String::new()
                },
            ),
            None => Default::default(),
        };
        self.editing = conn.map(|c| c.id);
        self.encryption = conn.map_or(EncryptionMode::LetServerChoose, |c| c.encryption);
        self.quality = conn.map_or(QualityPreset::Auto, |c| c.quality);
        self.view_only = conn.is_some_and(|c| c.view_only);
        self.shared = conn.is_none_or(|c| c.shared);
        self.remember_password = conn.is_none_or(|c| c.remember_password);
        self.name_input
            .update(cx, |s, cx| s.set_value(name, window, cx));
        self.server_input
            .update(cx, |s, cx| s.set_value(server, window, cx));
        self.password_input
            .update(cx, |s, cx| s.set_value(password, window, cx));
        self.labels_input
            .update(cx, |s, cx| s.set_value(labels, window, cx));
        self.modal = Modal::Connection;
        let first = if conn.is_some() {
            &self.name_input
        } else {
            &self.server_input
        };
        first.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn on_new(&mut self, _: &NewConnection, window: &mut Window, cx: &mut Context<Self>) {
        self.open_editor(None, window, cx);
    }

    fn on_properties(&mut self, _: &OpenProperties, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.selected.and_then(|id| self.book.get(id).cloned()) else {
            self.set_status("Select a connection first", cx);
            return;
        };
        self.open_editor(Some(&conn), window, cx);
    }

    fn on_duplicate(&mut self, _: &DuplicateSelected, _: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.selected.and_then(|id| self.book.get(id).cloned()) else {
            return;
        };
        let mut copy = conn;
        copy.id = ConnectionId::new();
        copy.name = format!("{} copy", copy.name);
        copy.last_connected = None;
        copy.remember_password = false;
        self.selected = Some(copy.id);
        self.book.upsert(copy);
        self.persist();
        cx.notify();
    }

    fn submit_connect(&mut self, connect: bool, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name_input.read(cx).value().to_string();
        let server = self.server_input.read(cx).value().to_string();
        let password = self.password_input.read(cx).unmask_value().to_string();
        let labels = parse_labels(&self.labels_input.read(cx).value());
        let (host, port) = match parse_server(&server) {
            Ok(v) => v,
            Err(e) => {
                self.set_status(e, cx);
                return;
            }
        };
        let mut conn = self
            .editing
            .and_then(|id| self.book.get(id).cloned())
            .unwrap_or_else(|| Connection::new(&name, &host, port));
        conn.name = if name.trim().is_empty() {
            format!("{host}:{port}")
        } else {
            name.trim().to_string()
        };
        conn.host = host;
        conn.port = port;
        conn.labels = labels;
        conn.encryption = self.encryption;
        conn.quality = self.quality;
        conn.view_only = self.view_only;
        conn.shared = self.shared;
        conn.remember_password = self.remember_password;
        if !self.remember_password {
            let _ = delete_password(conn.id);
        } else if !password.is_empty()
            && let Err(e) = save_password(conn.id, &password)
        {
            self.status = format!("Password not saved: {e}").into();
        }
        let stored = if self.remember_password && password.is_empty() {
            load_password(conn.id).ok().flatten()
        } else {
            None
        };
        let req = ConnectRequest::from_connection(
            &conn,
            if password.is_empty() {
                stored
            } else {
                Some(password)
            },
        );
        if connect {
            conn.last_connected = Some(unix_now());
        }
        self.selected = Some(conn.id);
        self.modal = Modal::None;
        let saved_name = conn.name.clone();
        self.book.upsert(conn);
        self.persist();
        if let SidebarFilter::Label(label) = &self.filter
            && !self
                .selected
                .and_then(|id| self.book.get(id))
                .is_some_and(|c| c.labels.contains(label))
        {
            self.filter = SidebarFilter::All;
        }
        if connect {
            self.launch(req, cx);
        } else {
            self.set_status(format!("Saved {saved_name}"), cx);
        }
        self.focus.focus(window, cx);
    }

    fn on_connect(&mut self, _: &ConnectSelected, window: &mut Window, cx: &mut Context<Self>) {
        match self.modal.clone() {
            Modal::Connection => self.submit_connect(true, window, cx),
            Modal::DeleteConfirm { id, .. } => self.confirm_delete(id, cx),
            Modal::Preferences => self.close_modal(cx),
            Modal::None => {
                let Some(id) = self.selected else {
                    self.set_status("Select a connection to connect", cx);
                    return;
                };
                self.connect_id(id, cx);
            }
        }
    }

    fn connect_id(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        let Some(mut conn) = self.book.get(id).cloned() else {
            return;
        };
        conn.last_connected = Some(unix_now());
        let password = if conn.remember_password {
            load_password(id).ok().flatten()
        } else {
            None
        };
        let req = ConnectRequest::from_connection(&conn, password);
        self.book.upsert(conn);
        self.persist();
        self.launch(req, cx);
    }

    fn launch(&mut self, req: ConnectRequest, cx: &mut Context<Self>) {
        self.status = format!("Connecting to {}…", req.display_name()).into();
        let title = req.display_name().to_string();
        let prefs = self.book.prefs();
        let options = session_window::SessionOptions {
            scale: prefs.default_scale,
            pin_toolbar: prefs.pin_toolbar,
            menu_key: prefs.menu_key.clone(),
            hide_shots: prefs.hide_screenshots,
            thumb_path: req.connection_id.map(|id| self.book.paths().thumb_path(id)),
        };
        session_window::open(req, title, options, cx);
        cx.notify();
    }

    /// Connect to a command-line target. Reuses a saved entry with the same
    /// host and port; otherwise connects ad hoc without touching the book.
    pub fn connect_target(&mut self, target: &str, _: &mut Window, cx: &mut Context<Self>) {
        match parse_server(target) {
            Ok((host, port)) => {
                let existing = self
                    .book
                    .connections()
                    .iter()
                    .find(|c| c.host.eq_ignore_ascii_case(&host) && c.port == port)
                    .map(|c| c.id);
                if let Some(id) = existing {
                    self.selected = Some(id);
                    self.connect_id(id, cx);
                } else {
                    let conn = Connection::new("", &host, port);
                    let mut req = ConnectRequest::from_connection(&conn, None);
                    req.connection_id = None;
                    self.launch(req, cx);
                }
            }
            Err(e) => self.set_status(e, cx),
        }
    }

    fn ask_delete(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        let name = self
            .book
            .get(id)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "this connection".into());
        self.selected = Some(id);
        self.modal = Modal::DeleteConfirm { id, name };
        cx.notify();
    }

    fn confirm_delete(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        match self.book.remove(id) {
            Ok(()) => {
                if self.selected == Some(id) {
                    self.selected = None;
                }
                self.persist();
                self.status = "Connection removed".into();
                if let SidebarFilter::Label(label) = &self.filter
                    && !self.book.labels().contains(label)
                {
                    self.filter = SidebarFilter::All;
                }
            }
            Err(e) => {
                self.status = format!("Delete failed: {e}").into();
            }
        }
        self.modal = Modal::None;
        cx.notify();
    }

    fn close_modal(&mut self, cx: &mut Context<Self>) {
        self.modal = Modal::None;
        cx.notify();
    }

    fn on_close_modal(&mut self, _: &CloseModal, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.modal, Modal::None) {
            return;
        }
        self.close_modal(cx);
        self.focus.focus(window, cx);
    }

    fn on_delete(&mut self, _: &DeleteSelected, window: &mut Window, cx: &mut Context<Self>) {
        match self.modal.clone() {
            Modal::DeleteConfirm { id, .. } => self.confirm_delete(id, cx),
            Modal::None => {
                if self.search.focus_handle(cx).is_focused(window) {
                    return;
                }
                let Some(id) = self.selected else {
                    self.set_status("Select a connection to delete", cx);
                    return;
                };
                self.ask_delete(id, cx);
            }
            _ => {}
        }
    }

    fn on_prefs(&mut self, _: &OpenPreferences, _: &mut Window, cx: &mut Context<Self>) {
        self.modal = Modal::Preferences;
        cx.notify();
    }

    fn on_toggle_view(&mut self, _: &ToggleViewMode, _: &mut Window, cx: &mut Context<Self>) {
        self.view_mode = match self.view_mode {
            ViewMode::Grid => ViewMode::List,
            ViewMode::List => ViewMode::Grid,
        };
        cx.notify();
    }

    fn on_toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.collapsed = !self.collapsed;
        cx.notify();
    }

    fn on_focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |s, cx| s.focus(window, cx));
    }

    fn select(&mut self, id: ConnectionId, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(id);
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let labels = self.book.labels();
        let filter = self.filter.clone();
        let total = self.book.connections().len();
        let recent = self.book.recents(20).len();
        Sidebar::new("rv-sidebar")
            .collapsible(SidebarCollapsible::Icon)
            .collapsed(self.collapsed)
            .w(theme::sidebar_width())
            .bg(theme::sidebar(cx))
            .header(SidebarHeader::new().child(
                h_flex().gap_2().items_center().child(logo_mark(cx)).when(
                    !self.collapsed,
                    |this| {
                        this.child(
                            v_flex()
                                .child(div().text_color(theme::ink(cx)).font_semibold().child("RV"))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme::muted(cx))
                                        .child("Address Book"),
                                ),
                        )
                    },
                ),
            ))
            .child(
                SidebarGroup::new("Browse").child(
                    SidebarMenu::new().children([
                        SidebarMenuItem::new(count_label("All connections", total))
                            .icon(IconName::LayoutDashboard)
                            .active(filter == SidebarFilter::All)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.filter = SidebarFilter::All;
                                cx.notify();
                            })),
                        SidebarMenuItem::new(count_label("Recents", recent))
                            .icon(IconName::Calendar)
                            .active(filter == SidebarFilter::Recents)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.filter = SidebarFilter::Recents;
                                cx.notify();
                            })),
                    ]),
                ),
            )
            .when(!labels.is_empty(), |side| {
                side.child(
                    SidebarGroup::new("Labels").child(SidebarMenu::new().children(
                        labels.into_iter().map(|label| {
                            let selected =
                                matches!(&filter, SidebarFilter::Label(l) if l == &label);
                            let count = self.book.filtered("", Some(&label)).len();
                            let label_for_click = label.clone();
                            SidebarMenuItem::new(count_label(&label, count))
                                .icon(IconName::Star)
                                .active(selected)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.filter = SidebarFilter::Label(label_for_click.clone());
                                    cx.notify();
                                }))
                        }),
                    )),
                )
            })
    }

    fn render_card(&self, conn: &Connection, cx: &mut Context<Self>) -> impl IntoElement {
        let id = conn.id;
        let selected = self.selected == Some(id);
        let hide = self.book.prefs().hide_screenshots;
        let thumb = self.book.paths().thumb_path(id);
        let name = conn.name.clone();
        let host = conn.server_display();
        let accent = theme::accent(cx);
        let hover_bg = theme::hover(cx);
        let last = conn.last_connected.map(relative_time);

        v_flex()
            .id(ElementId::from(format!("card-{id}")))
            .w(CARD_WIDTH)
            .rounded_lg()
            .border_1()
            .border_color(if selected { accent } else { theme::line(cx) })
            .bg(if selected {
                theme::selected(cx)
            } else {
                theme::card(cx)
            })
            .when(!selected, |this| {
                this.hover(|s| s.bg(hover_bg).border_color(accent))
            })
            .shadow_sm()
            .overflow_hidden()
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, window, cx| this.select(id, window, cx)),
            )
            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                // Otherwise the click reaches the content area, which clears
                // the selection again.
                cx.stop_propagation();
                this.select(id, window, cx);
                if ev.click_count() >= 2 {
                    this.connect_id(id, cx);
                }
            }))
            .context_menu(connection_menu(cx.entity(), id))
            .child(
                div()
                    .relative()
                    .h(THUMB_HEIGHT)
                    .w_full()
                    .bg(theme::thumb_bg(cx))
                    .items_center()
                    .justify_center()
                    .flex()
                    .child(if !hide && thumb.exists() {
                        img(thumb)
                            .size_full()
                            .object_fit(ObjectFit::Cover)
                            .into_any_element()
                    } else {
                        Icon::new(IconName::Frame)
                            .size_8()
                            .text_color(theme::muted(cx))
                            .into_any_element()
                    })
                    .child(
                        div().absolute().top_1().right_1().child(
                            Button::new(SharedString::from(format!("card-del-{id}")))
                                .ghost()
                                .icon(IconName::Delete)
                                .xsmall()
                                .tooltip("Delete connection")
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.ask_delete(id, cx);
                                })),
                        ),
                    ),
            )
            .child(
                v_flex()
                    .px_3()
                    .py_2()
                    .gap_0p5()
                    .child(
                        div()
                            .font_semibold()
                            .text_color(theme::ink(cx))
                            .text_ellipsis()
                            .child(name),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::muted(cx))
                            .text_ellipsis()
                            .child(host),
                    )
                    .when(last.is_some() || !conn.labels.is_empty(), |this| {
                        this.child(
                            h_flex()
                                .mt_1()
                                .gap_1()
                                .items_center()
                                .flex_wrap()
                                .children(conn.labels.iter().map(|l| label_chip(l, cx)))
                                .when_some(last, |this, last| {
                                    this.child(
                                        div().text_xs().text_color(theme::muted(cx)).child(last),
                                    )
                                }),
                        )
                    }),
            )
    }

    fn render_row(&self, conn: &Connection, cx: &mut Context<Self>) -> impl IntoElement {
        let id = conn.id;
        let selected = self.selected == Some(id);
        let hover_bg = theme::hover(cx);
        h_flex()
            .id(ElementId::from(format!("row-{id}")))
            .w_full()
            .px_3()
            .py_2()
            .gap_3()
            .rounded_md()
            .items_center()
            .bg(if selected {
                theme::selected(cx)
            } else {
                theme::card(cx)
            })
            .when(!selected, |this| this.hover(|s| s.bg(hover_bg)))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, window, cx| this.select(id, window, cx)),
            )
            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                // Otherwise the click reaches the content area, which clears
                // the selection again.
                cx.stop_propagation();
                this.select(id, window, cx);
                if ev.click_count() >= 2 {
                    this.connect_id(id, cx);
                }
            }))
            .context_menu(connection_menu(cx.entity(), id))
            .child(Icon::new(IconName::Network).text_color(theme::accent(cx)))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .font_semibold()
                            .text_ellipsis()
                            .child(conn.name.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::muted(cx))
                            .text_ellipsis()
                            .child(conn.server_display()),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .children(conn.labels.iter().map(|l| label_chip(l, cx))),
            )
            .child(
                div()
                    .w(px(90.))
                    .text_xs()
                    .text_right()
                    .text_color(theme::muted(cx))
                    .child(conn.last_connected.map(relative_time).unwrap_or_default()),
            )
            .child(
                Button::new(SharedString::from(format!("row-del-{id}")))
                    .ghost()
                    .icon(IconName::Delete)
                    .xsmall()
                    .tooltip("Delete connection")
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.ask_delete(id, cx);
                    })),
            )
    }

    fn render_new_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let accent = theme::accent(cx);
        v_flex()
            .id("card-new")
            .w(CARD_WIDTH)
            .rounded_lg()
            .border_1()
            .border_dashed()
            .border_color(theme::line(cx))
            .hover(|s| s.border_color(accent))
            .cursor_pointer()
            .overflow_hidden()
            .on_click(cx.listener(|this, _, window, cx| {
                cx.stop_propagation();
                this.on_new(&NewConnection, window, cx);
            }))
            .child(
                div()
                    .h(THUMB_HEIGHT)
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(IconName::Plus).size_8().text_color(accent)),
            )
            .child(
                v_flex().px_3().py_2().gap_0p5().child(
                    div()
                        .font_semibold()
                        .text_color(theme::ink(cx))
                        .child("New connection"),
                ),
            )
    }

    fn render_modal(&self, cx: &mut Context<Self>) -> AnyElement {
        let (title, subtitle): (&str, Option<String>) = match &self.modal {
            Modal::Connection => {
                if self.editing.is_some() {
                    ("Connection properties", None)
                } else {
                    ("New connection", None)
                }
            }
            Modal::DeleteConfirm { name, .. } => (
                "Delete connection",
                Some(format!("Remove “{name}” from the address book?")),
            ),
            Modal::Preferences => ("Preferences", None),
            Modal::None => return div().into_any_element(),
        };

        let body: AnyElement = match &self.modal {
            Modal::Connection => v_flex()
                .gap_3()
                .child(field("VNC server", Input::new(&self.server_input), cx))
                .child(field("Name", Input::new(&self.name_input), cx))
                .child(field("Password", Input::new(&self.password_input), cx))
                .child(field("Labels", Input::new(&self.labels_input), cx))
                .child(connection_options(self, cx))
                .child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .mt_1()
                        .child(
                            Button::new("modal-cancel")
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| this.close_modal(cx))),
                        )
                        .child(Button::new("modal-save").outline().label("Save").on_click(
                            cx.listener(|this, _, window, cx| {
                                this.submit_connect(false, window, cx)
                            }),
                        ))
                        .child(
                            Button::new("modal-connect")
                                .primary()
                                .label("Connect")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.submit_connect(true, window, cx)
                                })),
                        ),
                )
                .into_any_element(),
            Modal::DeleteConfirm { id, .. } => {
                let id = *id;
                v_flex()
                    .gap_3()
                    .child(div().text_sm().text_color(theme::muted(cx)).child(
                        "The saved password and desktop preview for this connection are removed too.",
                    ))
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("del-cancel")
                                    .label("Cancel")
                                    .on_click(cx.listener(|this, _, _, cx| this.close_modal(cx))),
                            )
                            .child(Button::new("del-ok").danger().label("Delete").on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.confirm_delete(id, cx);
                                }),
                            )),
                    )
                    .into_any_element()
            }
            Modal::Preferences => v_flex()
                .gap_3()
                .child(prefs_body(self, cx))
                .child(
                    h_flex().justify_end().child(
                        Button::new("prefs-done")
                            .primary()
                            .label("Done")
                            .on_click(cx.listener(|this, _, _, cx| this.close_modal(cx))),
                    ),
                )
                .into_any_element(),
            Modal::None => div().into_any_element(),
        };

        div()
            .id("modal-overlay")
            .occlude()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::scrim())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _, _, cx| this.close_modal(cx)))
            .child(
                v_flex()
                    .id("modal-card")
                    .w(px(480.))
                    .rounded_lg()
                    .bg(theme::card(cx))
                    .border_1()
                    .border_color(theme::line(cx))
                    .shadow_lg()
                    .p_5()
                    .gap_4()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_lg()
                                    .font_semibold()
                                    .text_color(theme::ink(cx))
                                    .child(title),
                            )
                            .when_some(subtitle, |this, s| {
                                this.child(div().text_sm().text_color(theme::ink(cx)).child(s))
                            }),
                    )
                    .child(body),
            )
            .into_any_element()
    }

    fn render_content(&self, items: &[Connection], cx: &mut Context<Self>) -> AnyElement {
        let query = self.query(cx);
        if items.is_empty() {
            return empty_state(&query, &self.filter, self.book.connections().is_empty(), cx)
                .into_any_element();
        }
        match self.view_mode {
            ViewMode::Grid => {
                let mut cards: Vec<AnyElement> = items
                    .iter()
                    .map(|c| self.render_card(c, cx).into_any_element())
                    .collect();
                cards.push(self.render_new_card(cx).into_any_element());
                div()
                    .id("grid")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .content_start()
                    .gap_4()
                    .pb_4()
                    .overflow_y_scroll()
                    .children(cards)
                    .into_any_element()
            }
            ViewMode::List => {
                let rows: Vec<AnyElement> = items
                    .iter()
                    .map(|c| self.render_row(c, cx).into_any_element())
                    .collect();
                v_flex()
                    .id("list")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .gap_1()
                    .p_1()
                    .mb_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme::line(cx))
                    .bg(theme::card(cx))
                    .overflow_y_scroll()
                    .children(rows)
                    .into_any_element()
            }
        }
    }
}

impl Focusable for AddressBookApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for AddressBookApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let items = self.visible(cx);
        let has_selection = self.selected.is_some();
        let count_text = match items.len() {
            1 => "1 connection".to_string(),
            n => format!("{n} connections"),
        };

        v_flex()
            .id("address-book")
            .relative()
            .size_full()
            .bg(theme::surface(cx))
            .text_color(theme::ink(cx))
            .key_context("AddressBook")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_new))
            .on_action(cx.listener(Self::on_connect))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_prefs))
            .on_action(cx.listener(Self::on_toggle_view))
            .on_action(cx.listener(Self::on_toggle_sidebar))
            .on_action(cx.listener(Self::on_focus_search))
            .on_action(cx.listener(Self::on_properties))
            .on_action(cx.listener(Self::on_duplicate))
            .on_action(cx.listener(Self::on_close_modal))
            .on_action(|_: &QuitApp, _, cx| cx.quit())
            .child(
                TitleBar::new().child(
                    h_flex()
                        .w_full()
                        .pr_3()
                        .items_center()
                        .gap_2()
                        .child(
                            SidebarToggleButton::new()
                                .collapsed(self.collapsed)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.collapsed = !this.collapsed;
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .font_semibold()
                                .text_color(theme::ink(cx))
                                .child("RV Viewer"),
                        ),
                ),
            )
            .child(
                h_flex()
                    .h(px(48.))
                    .px_3()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(theme::line(cx))
                    .bg(theme::card(cx))
                    .child(
                        div().w(px(280.)).child(
                            Input::new(&self.search)
                                .prefix(Icon::new(IconName::Search).text_color(theme::muted(cx))),
                        ),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("view-mode")
                            .ghost()
                            .icon(if self.view_mode == ViewMode::Grid {
                                IconName::Menu
                            } else {
                                IconName::LayoutDashboard
                            })
                            .tooltip(if self.view_mode == ViewMode::Grid {
                                "Show as list (⌘L)"
                            } else {
                                "Show as grid (⌘L)"
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.on_toggle_view(&ToggleViewMode, window, cx);
                            })),
                    )
                    .child(
                        Button::new("prefs")
                            .ghost()
                            .icon(IconName::Settings)
                            .tooltip("Preferences (⌘,)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.on_prefs(&OpenPreferences, window, cx);
                            })),
                    )
                    .child(toolbar_sep(cx))
                    .child(
                        Button::new("props")
                            .ghost()
                            .icon(IconName::Info)
                            .label("Properties")
                            .tooltip("Edit the selected connection (⌘I)")
                            .disabled(!has_selection)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.on_properties(&OpenProperties, window, cx);
                            })),
                    )
                    .child(
                        Button::new("delete")
                            .ghost()
                            .icon(IconName::Delete)
                            .label("Delete")
                            .tooltip("Delete the selected connection (⌫)")
                            .disabled(!has_selection)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(id) = this.selected {
                                    this.ask_delete(id, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("connect")
                            .outline()
                            .icon(IconName::Play)
                            .label("Connect")
                            .tooltip("Connect to the selected entry (↩)")
                            .disabled(!has_selection)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.on_connect(&ConnectSelected, window, cx);
                            })),
                    )
                    .child(
                        Button::new("new")
                            .primary()
                            .icon(IconName::Plus)
                            .label("New connection")
                            .tooltip("⌘N")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.on_new(&NewConnection, window, cx);
                            })),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(
                        v_flex()
                            .id("content")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .px_4()
                            .pt_3()
                            .gap_3()
                            .on_click(cx.listener(|this, _, window, cx| {
                                // Click on the empty canvas: clear selection.
                                this.selected = None;
                                this.focus.focus(window, cx);
                                cx.notify();
                            }))
                            .child(
                                h_flex()
                                    .items_baseline()
                                    .gap_2()
                                    .child(
                                        div().text_lg().font_semibold().child(self.filter.title()),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(theme::muted(cx))
                                            .child(count_text),
                                    ),
                            )
                            .child(self.render_content(&items, cx)),
                    ),
            )
            .child(
                h_flex()
                    .h(px(28.))
                    .px_3()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(theme::line(cx))
                    .bg(theme::card(cx))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::muted(cx))
                            .text_ellipsis()
                            .child(self.status.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::muted(cx))
                            .child("Double-click to connect · ⌘N new · ⌘F search"),
                    ),
            )
            .when(!matches!(self.modal, Modal::None), |this| {
                this.child(self.render_modal(cx))
            })
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Human-friendly age of a unix timestamp.
fn relative_time(ts: i64) -> String {
    let delta = (unix_now() - ts).max(0);
    match delta {
        d if d < 60 => "just now".into(),
        d if d < 3600 => format!("{} min ago", d / 60),
        d if d < 86_400 => format!("{} h ago", d / 3600),
        d if d < 30 * 86_400 => format!("{} d ago", d / 86_400),
        d if d < 365 * 86_400 => format!("{} mo ago", d / (30 * 86_400)),
        d => format!("{} y ago", d / (365 * 86_400)),
    }
}

fn parse_labels(text: &str) -> Vec<String> {
    let mut out: Vec<String> = text
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

fn count_label(name: &str, count: usize) -> SharedString {
    if count == 0 {
        name.to_string().into()
    } else {
        format!("{name}  ·  {count}").into()
    }
}

fn logo_mark(cx: &App) -> impl IntoElement {
    div()
        .size_8()
        .rounded_md()
        .bg(theme::accent(cx))
        .text_color(theme::accent_fg(cx))
        .flex()
        .items_center()
        .justify_center()
        .font_bold()
        .child("RV")
}

fn toolbar_sep(cx: &App) -> impl IntoElement {
    div().w(px(1.)).h(px(20.)).mx_1().bg(theme::line(cx))
}

fn label_chip(label: &str, cx: &App) -> impl IntoElement {
    div()
        .px_1p5()
        .rounded_sm()
        .text_xs()
        .bg(theme::selected(cx))
        .text_color(theme::accent(cx))
        .child(label.to_string())
}

fn field(label: &'static str, input: impl IntoElement, cx: &App) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(div().text_xs().text_color(theme::muted(cx)).child(label))
        .child(input)
}

/// One preference row: label on the left, control on the right.
fn setting_row(label: &'static str, control: impl IntoElement, cx: &App) -> impl IntoElement {
    h_flex()
        .justify_between()
        .items_center()
        .gap_3()
        .child(div().text_sm().text_color(theme::ink(cx)).child(label))
        .child(control)
}

fn connection_menu(
    book: Entity<AddressBookApp>,
    id: ConnectionId,
) -> impl Fn(
    gpui_component::menu::PopupMenu,
    &mut Window,
    &mut Context<gpui_component::menu::PopupMenu>,
) -> gpui_component::menu::PopupMenu {
    move |menu, _, _| {
        let connect = book.clone();
        let props = book.clone();
        let dup = book.clone();
        let del = book.clone();
        menu.item(PopupMenuItem::new("Connect").on_click(move |_, _, cx| {
            connect.update(cx, |this, cx| {
                this.selected = Some(id);
                this.connect_id(id, cx);
            });
        }))
        .item(
            PopupMenuItem::new("Properties…").on_click(move |_, window, cx| {
                props.update(cx, |this, cx| {
                    this.selected = Some(id);
                    this.on_properties(&OpenProperties, window, cx);
                });
            }),
        )
        .item(
            PopupMenuItem::new("Duplicate").on_click(move |_, window, cx| {
                dup.update(cx, |this, cx| {
                    this.selected = Some(id);
                    this.on_duplicate(&DuplicateSelected, window, cx);
                });
            }),
        )
        .separator()
        .item(PopupMenuItem::new("Delete…").on_click(move |_, _, cx| {
            del.update(cx, |this, cx| {
                this.ask_delete(id, cx);
            });
        }))
    }
}

fn connection_options(app: &AddressBookApp, cx: &mut Context<AddressBookApp>) -> impl IntoElement {
    v_flex()
        .gap_2()
        .pt_2()
        .border_t_1()
        .border_color(theme::line(cx))
        .child(setting_row(
            "Encryption",
            Button::new("enc")
                .outline()
                .small()
                .label(app.encryption.label())
                .tooltip("Click to cycle")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.encryption = this.encryption.cycle();
                    cx.notify();
                })),
            cx,
        ))
        .child(setting_row(
            "Picture quality",
            Button::new("qual")
                .outline()
                .small()
                .label(app.quality.label())
                .tooltip("Click to cycle")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.quality = this.quality.cycle();
                    cx.notify();
                })),
            cx,
        ))
        .child(
            h_flex()
                .gap_4()
                .flex_wrap()
                .child(
                    Switch::new("remember")
                        .checked(app.remember_password)
                        .label("Remember password")
                        .on_click(cx.listener(|this, v: &bool, _, cx| {
                            this.remember_password = *v;
                            cx.notify();
                        })),
                )
                .child(
                    Switch::new("viewonly")
                        .checked(app.view_only)
                        .label("View only")
                        .on_click(cx.listener(|this, v: &bool, _, cx| {
                            this.view_only = *v;
                            cx.notify();
                        })),
                )
                .child(
                    Switch::new("shared")
                        .checked(app.shared)
                        .label("Shared session")
                        .on_click(cx.listener(|this, v: &bool, _, cx| {
                            this.shared = *v;
                            cx.notify();
                        })),
                ),
        )
}

fn prefs_body(app: &AddressBookApp, cx: &mut Context<AddressBookApp>) -> impl IntoElement {
    let prefs = app.book.prefs();
    let theme_pref = prefs.theme;
    let scale = prefs.default_scale;
    let pin = prefs.pin_toolbar;
    let hide = prefs.hide_screenshots;
    let menu_key = prefs.menu_key.to_ascii_uppercase();
    v_flex()
        .gap_3()
        .child(setting_row(
            "Appearance",
            Button::new("theme")
                .outline()
                .small()
                .icon(if theme::is_dark(cx) {
                    IconName::Moon
                } else {
                    IconName::Sun
                })
                .label(theme_pref.label())
                .on_click(cx.listener(|this, _, window, cx| {
                    let next = this.book.prefs().theme.cycle();
                    this.book.prefs_mut().theme = next;
                    this.persist();
                    theme::apply(next, window, cx);
                    cx.notify();
                })),
            cx,
        ))
        .child(setting_row(
            "Default scaling",
            Button::new("scale")
                .outline()
                .small()
                .label(scale.label())
                .on_click(cx.listener(|this, _, _, cx| {
                    let next = this.book.prefs().default_scale.cycle();
                    this.book.prefs_mut().default_scale = next;
                    this.persist();
                    cx.notify();
                })),
            cx,
        ))
        .child(setting_row(
            "Session menu key",
            Button::new("menukey")
                .outline()
                .small()
                .label(menu_key)
                .on_click(cx.listener(|this, _, _, cx| {
                    let next = match this.book.prefs().menu_key.as_str() {
                        "f8" => "f7",
                        "f7" => "f12",
                        _ => "f8",
                    };
                    this.book.prefs_mut().menu_key = next.into();
                    this.persist();
                    cx.notify();
                })),
            cx,
        ))
        .child(setting_row(
            "Pin the session toolbar",
            Switch::new("pin")
                .checked(pin)
                .on_click(cx.listener(|this, v: &bool, _, cx| {
                    this.book.prefs_mut().pin_toolbar = *v;
                    this.persist();
                    cx.notify();
                })),
            cx,
        ))
        .child(setting_row(
            "Hide desktop previews",
            Switch::new("hide")
                .checked(hide)
                .on_click(cx.listener(|this, v: &bool, _, cx| {
                    this.book.prefs_mut().hide_screenshots = *v;
                    this.persist();
                    cx.notify();
                })),
            cx,
        ))
        .child(
            div()
                .pt_2()
                .border_t_1()
                .border_color(theme::line(cx))
                .child(
                    Button::new("forget")
                        .danger()
                        .outline()
                        .small()
                        .label("Forget all passwords and previews")
                        .on_click(cx.listener(|this, _, _, cx| {
                            match this.book.forget_sensitive() {
                                Ok(()) => this.status = "Passwords and previews removed".into(),
                                Err(e) => this.status = format!("Could not forget: {e}").into(),
                            }
                            cx.notify();
                        })),
                ),
        )
}

fn empty_state(
    query: &str,
    filter: &SidebarFilter,
    book_empty: bool,
    cx: &mut Context<AddressBookApp>,
) -> impl IntoElement {
    let (title, hint): (String, String) = if !query.trim().is_empty() {
        (
            format!("No matches for “{}”", query.trim()),
            "Try another name, host, or label.".into(),
        )
    } else if book_empty {
        (
            "No connections yet".into(),
            "Create a connection to a VNC server on your network.".into(),
        )
    } else {
        match filter {
            SidebarFilter::Recents => (
                "Nothing recent".into(),
                "Connections you open show up here.".into(),
            ),
            SidebarFilter::Label(l) => (
                format!("No connections labelled “{l}”"),
                "Add the label in a connection's properties.".into(),
            ),
            SidebarFilter::All => ("No connections".into(), String::new()),
        }
    };
    v_flex()
        .flex_1()
        .items_center()
        .justify_center()
        .gap_3()
        .child(logo_mark(cx))
        .child(div().text_lg().font_semibold().child(title))
        .when(!hint.is_empty(), |this| {
            this.child(div().text_color(theme::muted(cx)).child(hint))
        })
        .when(query.trim().is_empty(), |this| {
            this.child(
                Button::new("empty-new")
                    .primary()
                    .label("New connection")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.on_new(&NewConnection, window, cx);
                    })),
            )
        })
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the `gpui::*` glob would shadow `#[test]`.
    use super::{parse_labels, relative_time, unix_now};

    #[test]
    fn labels_are_trimmed_sorted_and_deduped() {
        assert_eq!(
            parse_labels(" work, lab ,,work, home "),
            vec!["home", "lab", "work"]
        );
        assert!(parse_labels("  ").is_empty());
    }

    #[test]
    fn relative_time_buckets() {
        let now = unix_now();
        assert_eq!(relative_time(now), "just now");
        assert_eq!(relative_time(now - 120), "2 min ago");
        assert_eq!(relative_time(now - 7200), "2 h ago");
        assert_eq!(relative_time(now - 3 * 86_400), "3 d ago");
    }
}
