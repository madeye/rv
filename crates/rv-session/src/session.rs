use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use rv_core::{ConnectRequest, EncryptionMode};
use tokio::net::TcpStream;
use tokio::time::timeout;
use vnc::{PixelFormat, VncConnector, VncEvent, X11Event};

use crate::SessionError;
use crate::compositor::{Apply, Framebuffer};
use crate::encodings::encodings_for;
use crate::vencrypt;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Shortest interval between incremental update requests once the previous
/// one has been answered.
const REFRESH_EVERY: Duration = Duration::from_millis(16);
/// Longest interval without a request. Servers hold an incremental request
/// until the screen changes, so this only matters as a keepalive; it also
/// nudges vnc-rs's network task, which stops reading while its decoder is
/// backlogged until the next outgoing message.
const REFRESH_KEEPALIVE: Duration = Duration::from_millis(250);
/// How long the loop idles waiting for input before polling decoded frames again.
const POLL_EVERY: Duration = Duration::from_millis(4);
/// Decoded frame events applied per scheduling slot. A server streaming
/// updates must not keep the loop busy so long that a queued key-up waits;
/// the remote side would auto-repeat the key in the meantime.
const MAX_EVENTS_PER_SLOT: usize = 64;

#[derive(Debug, Clone)]
pub enum SessionEvent {
    Status(String),
    Connected { width: u16, height: u16 },
    FrameReady { generation: u64 },
    Clipboard(String),
    Bell,
    Error(String),
    Disconnected,
}

#[derive(Debug, Clone)]
pub enum SessionCommand {
    Input(X11Event),
    Close,
}

pub struct SessionHandle {
    pub framebuffer: Arc<Mutex<Framebuffer>>,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<SessionCommand>,
    event_rx: Mutex<mpsc::Receiver<SessionEvent>>,
    thread: Option<thread::JoinHandle<()>>,
}

/// In-memory peer for UI tests; no socket or worker thread is created.
#[cfg(feature = "test-support")]
pub struct SessionTestPeer {
    pub commands: tokio::sync::mpsc::UnboundedReceiver<SessionCommand>,
    pub events: mpsc::Sender<SessionEvent>,
}

impl SessionHandle {
    #[cfg(feature = "test-support")]
    pub fn test_pair() -> (Self, SessionTestPeer) {
        let (cmd_tx, commands) = tokio::sync::mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::channel();
        (
            Self {
                framebuffer: Arc::new(Mutex::new(Framebuffer::default())),
                cmd_tx,
                event_rx: Mutex::new(event_rx),
                thread: None,
            },
            SessionTestPeer { commands, events },
        )
    }

    pub fn spawn(request: ConnectRequest) -> Self {
        let framebuffer = Arc::new(Mutex::new(Framebuffer::default()));
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::channel();
        let fb = framebuffer.clone();
        let thread = thread::Builder::new()
            .name("rv-vnc".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                rt.block_on(run(request, fb, cmd_rx, event_tx));
            })
            .expect("spawn vnc thread");
        Self {
            framebuffer,
            cmd_tx,
            event_rx: Mutex::new(event_rx),
            thread: Some(thread),
        }
    }

    pub fn send(&self, cmd: SessionCommand) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn try_recv(&self) -> Option<SessionEvent> {
        self.event_rx.lock().ok()?.try_recv().ok()
    }

    /// Events since the last call.
    ///
    /// Frame notifications are collapsed to the newest generation (see
    /// [`coalesce_frames`]): pixels live in `framebuffer`, so one notification
    /// per drain is all a renderer needs, no matter how many rectangles the
    /// server sent.
    pub fn drain(&self) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        while let Some(e) = self.try_recv() {
            out.push(e);
        }
        coalesce_frames(&mut out);
        out
    }

    pub fn pointer(&self, x: u16, y: u16, buttons: u8) {
        self.send(SessionCommand::Input(X11Event::PointerEvent(
            (x, y, buttons).into(),
        )));
    }

    pub fn key(&self, keysym: u32, down: bool) {
        self.send(SessionCommand::Input(X11Event::KeyEvent(
            (keysym, down).into(),
        )));
    }

    pub fn copy_text(&self, text: String) {
        self.send(SessionCommand::Input(X11Event::CopyText(text)));
    }

    pub fn close(&self) {
        self.send(SessionCommand::Close);
    }
}

/// Collapse every `FrameReady` in `events` into a single one carrying the
/// newest generation, at the position of the last frame event. Every other
/// event keeps its relative order.
///
/// Servers send one rectangle per event, and a busy desktop yields dozens per
/// frame. Rebuilding the on-screen image once per rectangle stalls the UI
/// thread badly enough that queued key-ups are delivered late and the remote
/// auto-repeats the key.
pub fn coalesce_frames(events: &mut Vec<SessionEvent>) {
    let mut newest: Option<u64> = None;
    let mut last = None;
    for (i, e) in events.iter().enumerate() {
        if let SessionEvent::FrameReady { generation } = e {
            newest = Some(newest.map_or(*generation, |n| n.max(*generation)));
            last = Some(i);
        }
    }
    let (Some(generation), Some(last)) = (newest, last) else {
        return;
    };
    let mut index = 0;
    events.retain(|e| {
        let keep = !matches!(e, SessionEvent::FrameReady { .. }) || index == last;
        index += 1;
        keep
    });
    if let Some(SessionEvent::FrameReady { generation: g }) = events
        .iter_mut()
        .find(|e| matches!(e, SessionEvent::FrameReady { .. }))
    {
        *g = generation;
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        // Ask the session to stop but never block the caller: this runs on the
        // UI thread when a window closes, and the worker may be mid-handshake.
        // The worker notices the closed command channel and exits on its own.
        self.close();
        drop(self.thread.take());
    }
}

async fn run(
    request: ConnectRequest,
    fb: Arc<Mutex<Framebuffer>>,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<SessionCommand>,
    event_tx: mpsc::Sender<SessionEvent>,
) {
    let send = |e: SessionEvent| {
        let _ = event_tx.send(e);
    };

    send(SessionEvent::Status(format!(
        "Connecting to {}:{}…",
        request.host, request.port
    )));

    match connect_and_loop(request, fb, &mut cmd_rx, &send).await {
        Ok(()) => send(SessionEvent::Disconnected),
        Err(e) => {
            send(SessionEvent::Error(e.to_string()));
            send(SessionEvent::Disconnected);
        }
    }
}

async fn connect_and_loop(
    request: ConnectRequest,
    fb: Arc<Mutex<Framebuffer>>,
    cmd_rx: &mut tokio::sync::mpsc::UnboundedReceiver<SessionCommand>,
    send: &impl Fn(SessionEvent),
) -> Result<(), SessionError> {
    // A close request (window closed, handle dropped) must interrupt the
    // handshake too, not just the running session; otherwise a server that
    // never answers keeps the worker alive for the whole connect timeout.
    let client = tokio::select! {
        result = connect(&request, send) => result?,
        _ = wait_for_close(cmd_rx) => return Ok(()),
    };
    session_loop(client, fb, cmd_rx, send).await
}

/// Resolves once the UI asks to close (or drops the handle). Input queued
/// before the session exists has nothing to go to and is discarded.
async fn wait_for_close(cmd_rx: &mut tokio::sync::mpsc::UnboundedReceiver<SessionCommand>) {
    loop {
        match cmd_rx.recv().await {
            Some(SessionCommand::Close) | None => return,
            Some(SessionCommand::Input(_)) => {}
        }
    }
}

async fn dial(addr: &str) -> Result<TcpStream, SessionError> {
    let tcp = timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| SessionError::Timeout)?
        .map_err(|e| SessionError::msg(format!("cannot connect to {addr}: {e}")))?;
    let _ = tcp.set_nodelay(true);
    Ok(tcp)
}

/// Which transport to use given what the server offers and what the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    Plain,
    Ard,
    VeNCrypt,
}

fn choose_transport(
    mode: EncryptionMode,
    types: &[u8],
    has_username: bool,
) -> Result<Transport, SessionError> {
    let plain_ok = types
        .iter()
        .any(|t| matches!(*t, vencrypt::SEC_NONE | vencrypt::SEC_VNC_AUTH));
    let ard_ok = types.contains(&crate::ard::SECURITY_TYPE);
    let vencrypt_ok = types.contains(&vencrypt::VENCRYPT_SECURITY_TYPE);
    let offered = || {
        types
            .iter()
            .map(|t| vencrypt::security_type_name(*t))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match mode {
        EncryptionMode::Always if vencrypt_ok => Ok(Transport::VeNCrypt),
        EncryptionMode::Always => Err(SessionError::msg(format!(
            "server does not offer VeNCrypt (offers: {}); set Encryption to Let server choose to connect unencrypted",
            offered()
        ))),
        EncryptionMode::PreferOn if vencrypt_ok => Ok(Transport::VeNCrypt),
        EncryptionMode::LetServerChoose if vencrypt_ok && !plain_ok => Ok(Transport::VeNCrypt),
        _ if ard_ok && (has_username || !plain_ok) => Ok(Transport::Ard),
        _ if plain_ok => Ok(Transport::Plain),
        EncryptionMode::Off if vencrypt_ok => Err(SessionError::msg(format!(
            "server requires encryption (offers: {}); set Encryption to Let server choose",
            offered()
        ))),
        _ => Err(SessionError::msg(format!(
            "no supported security type (server offers: {}; RV supports None, VncAuth, ARD, VeNCrypt)",
            offered()
        ))),
    }
}

async fn connect(
    request: &ConnectRequest,
    send: &impl Fn(SessionEvent),
) -> Result<vnc::VncClient, SessionError> {
    let addr = format!("{}:{}", request.host, request.port);
    let mut tcp = dial(&addr).await?;
    let types = vencrypt::read_security_types(&mut tcp).await?;
    let stream = match choose_transport(
        request.encryption,
        &types,
        request.username.as_ref().is_some_and(|u| !u.is_empty()),
    )? {
        Transport::Plain => vencrypt::RfbStream::plain(tcp, &types),
        Transport::Ard => {
            send(SessionEvent::Status(
                "Authenticating with Mac login…".into(),
            ));
            timeout(
                CONNECT_TIMEOUT,
                crate::ard::handshake(
                    &mut tcp,
                    request.username.as_deref(),
                    request.password.as_deref(),
                ),
            )
            .await
            .map_err(|_| SessionError::Timeout)??;
            vencrypt::RfbStream::authenticated(tcp)
        }
        Transport::VeNCrypt => {
            send(SessionEvent::Status("Negotiating VeNCrypt…".into()));
            vencrypt::handshake(tcp, &request.host).await?
        }
    };

    let password = request.password.clone().unwrap_or_default();
    let mut connector = VncConnector::new(stream)
        .set_auth_method(async move { Ok(password) })
        .allow_shared(request.shared)
        .set_pixel_format(PixelFormat::bgra());
    for enc in encodings_for(request.quality) {
        connector = connector.add_encoding(enc);
    }
    Ok(connector.build()?.try_start().await?.finish()?)
}

async fn session_loop(
    mut client: vnc::VncClient,
    fb: Arc<Mutex<Framebuffer>>,
    cmd_rx: &mut tokio::sync::mpsc::UnboundedReceiver<SessionCommand>,
    send: &impl Fn(SessionEvent),
) -> Result<(), SessionError> {
    use tokio::sync::mpsc::error::TryRecvError;

    send(SessionEvent::Status(
        "Connected, waiting for desktop…".into(),
    ));
    let mut announced = false;
    let mut last_refresh = Instant::now();
    // One incremental request in flight at a time, like other VNC viewers.
    // Firing them unconditionally queues every key event behind a backlog of
    // requests the server has not answered yet.
    let mut refresh_answered = true;
    loop {
        // Input first: a key-up queued behind frame decoding turns into
        // auto-repeat on the server, so pixels never take priority over commands.
        loop {
            match cmd_rx.try_recv() {
                Ok(cmd) => {
                    if !apply_command(&mut client, cmd).await? {
                        return Ok(());
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    let _ = client.close().await;
                    return Ok(());
                }
            }
        }

        let mut applied = 0;
        while applied < MAX_EVENTS_PER_SLOT {
            match client.poll_event().await {
                Ok(Some(ev)) => {
                    handle_event(ev, &fb, send, &mut announced);
                    applied += 1;
                    refresh_answered = true;
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = client.close().await;
                    return Err(e.into());
                }
            }
        }
        let since_refresh = last_refresh.elapsed();
        if (refresh_answered && since_refresh >= REFRESH_EVERY)
            || since_refresh >= REFRESH_KEEPALIVE
        {
            last_refresh = Instant::now();
            refresh_answered = false;
            let _ = client.input(X11Event::Refresh).await;
        }
        if applied == MAX_EVENTS_PER_SLOT {
            // Backlog: go straight back to the input check instead of idling.
            tokio::task::yield_now().await;
            continue;
        }

        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    None => {
                        let _ = client.close().await;
                        return Ok(());
                    }
                    Some(cmd) => {
                        if !apply_command(&mut client, cmd).await? {
                            return Ok(());
                        }
                    }
                }
            }
            _ = tokio::time::sleep(POLL_EVERY) => {}
        }
    }
}

/// Returns `Ok(false)` once the session should end.
async fn apply_command(
    client: &mut vnc::VncClient,
    cmd: SessionCommand,
) -> Result<bool, SessionError> {
    match cmd {
        SessionCommand::Close => {
            let _ = client.close().await;
            Ok(false)
        }
        SessionCommand::Input(ev) => {
            client.input(ev).await?;
            Ok(true)
        }
    }
}

fn handle_event(
    ev: VncEvent,
    fb: &Arc<Mutex<Framebuffer>>,
    send: &impl Fn(SessionEvent),
    announced: &mut bool,
) {
    let mut fb = fb.lock().expect("framebuffer lock");
    match fb.apply(ev) {
        Apply::Resized => {
            if !*announced && fb.width > 0 {
                *announced = true;
                send(SessionEvent::Connected {
                    width: fb.width,
                    height: fb.height,
                });
            }
            send(SessionEvent::FrameReady {
                generation: fb.generation,
            });
        }
        Apply::Dirty => send(SessionEvent::FrameReady {
            generation: fb.generation,
        }),
        Apply::Clipboard(text) => send(SessionEvent::Clipboard(text)),
        Apply::Bell => send(SessionEvent::Bell),
        Apply::Error(e) => send(SessionEvent::Error(e)),
        Apply::Ignored => {}
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;

    #[test]
    fn let_server_choose_prefers_plain_but_accepts_vencrypt_only() {
        assert_eq!(
            choose_transport(EncryptionMode::LetServerChoose, &[1, 2, 19], false).unwrap(),
            Transport::Plain
        );
        assert_eq!(
            choose_transport(EncryptionMode::LetServerChoose, &[19], false).unwrap(),
            Transport::VeNCrypt
        );
        assert!(choose_transport(EncryptionMode::LetServerChoose, &[16, 33], false).is_err());
    }

    #[test]
    fn prefer_on_and_always() {
        assert_eq!(
            choose_transport(EncryptionMode::PreferOn, &[2, 19], false).unwrap(),
            Transport::VeNCrypt
        );
        assert_eq!(
            choose_transport(EncryptionMode::PreferOn, &[2], false).unwrap(),
            Transport::Plain
        );
        assert!(choose_transport(EncryptionMode::Always, &[2], false).is_err());
    }

    #[test]
    fn off_never_encrypts() {
        assert_eq!(
            choose_transport(EncryptionMode::Off, &[2, 19], false).unwrap(),
            Transport::Plain
        );
        assert!(choose_transport(EncryptionMode::Off, &[19], false).is_err());
    }
    #[test]
    fn apple_security_offer_selects_ard_without_weakening_required_tls() {
        let types = [30, 33, 36, 35];
        for mode in [
            EncryptionMode::LetServerChoose,
            EncryptionMode::PreferOn,
            EncryptionMode::Off,
        ] {
            assert_eq!(
                choose_transport(mode, &types, true).unwrap(),
                Transport::Ard
            );
            assert_eq!(
                choose_transport(mode, &types, false).unwrap(),
                Transport::Ard
            );
        }
        assert!(choose_transport(EncryptionMode::Always, &types, true).is_err());
        assert_eq!(
            choose_transport(EncryptionMode::LetServerChoose, &[2, 30], true).unwrap(),
            Transport::Ard
        );
        assert_eq!(
            choose_transport(EncryptionMode::LetServerChoose, &[2, 30], false).unwrap(),
            Transport::Plain
        );
        assert_eq!(
            choose_transport(EncryptionMode::PreferOn, &[2, 19, 30], true).unwrap(),
            Transport::VeNCrypt
        );
    }
}
