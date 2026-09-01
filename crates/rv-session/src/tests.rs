use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use rv_core::{ConnectRequest, EncryptionMode, QualityPreset};

use crate::{SessionEvent, SessionHandle, coalesce_frames};

fn write_u16(s: &mut TcpStream, v: u16) {
    s.write_all(&v.to_be_bytes()).unwrap();
}

fn write_u32(s: &mut TcpStream, v: u32) {
    s.write_all(&v.to_be_bytes()).unwrap();
}

fn read_exact(s: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf).unwrap();
    buf
}

/// RFB 3.8 handshake: None auth, `width`×`height` framebuffer named `name`.
fn rfb_handshake(sock: &mut TcpStream, width: u16, height: u16, name: &[u8]) {
    sock.write_all(b"RFB 003.008\n").unwrap();
    let _ver = read_exact(sock, 12);
    sock.write_all(&[1, 1]).unwrap(); // one type: None
    let _choice = read_exact(sock, 1);
    write_u32(sock, 0); // SecurityResult OK
    let _shared = read_exact(sock, 1);

    write_u16(sock, width);
    write_u16(sock, height);
    // Pixel format (ignored; client sends its own).
    sock.write_all(&[32, 24, 0, 1]).unwrap();
    sock.write_all(&255u16.to_be_bytes()).unwrap();
    sock.write_all(&255u16.to_be_bytes()).unwrap();
    sock.write_all(&255u16.to_be_bytes()).unwrap();
    sock.write_all(&[16, 8, 0, 0, 0, 0]).unwrap();
    write_u32(sock, name.len() as u32);
    sock.write_all(name).unwrap();
}

/// Minimal RFB 3.8 server: None auth, 8×8 Raw framebuffer, then echo input.
fn mock_rfb_server(mut sock: TcpStream) {
    rfb_handshake(&mut sock, 8, 8, b"test");

    let mut saw_pointer = false;
    let mut saw_key = false;
    let mut sent_frame = false;
    sock.set_read_timeout(Some(Duration::from_secs(3))).ok();

    loop {
        let mut typ = [0u8; 1];
        if sock.read_exact(&mut typ).is_err() {
            break;
        }
        match typ[0] {
            0 => {
                // SetPixelFormat
                let _ = read_exact(&mut sock, 3 + 16);
            }
            2 => {
                // SetEncodings
                let _ = read_exact(&mut sock, 1);
                let n = u16::from_be_bytes(read_exact(&mut sock, 2).try_into().unwrap());
                let _ = read_exact(&mut sock, n as usize * 4);
            }
            3 => {
                // FramebufferUpdateRequest
                let _ = read_exact(&mut sock, 9);
                if !sent_frame {
                    sock.write_all(&[0, 0]).unwrap(); // FramebufferUpdate + pad
                    write_u16(&mut sock, 1); // nrects
                    write_u16(&mut sock, 0);
                    write_u16(&mut sock, 0);
                    write_u16(&mut sock, 8);
                    write_u16(&mut sock, 8);
                    write_u32(&mut sock, 0); // Raw
                    let mut pixels = Vec::with_capacity(8 * 8 * 4);
                    for _ in 0..64 {
                        pixels.extend_from_slice(&[0, 80, 200, 255]); // BGRA
                    }
                    sock.write_all(&pixels).unwrap();
                    sent_frame = true;
                }
            }
            4 => {
                // KeyEvent
                let _ = read_exact(&mut sock, 7);
                saw_key = true;
            }
            5 => {
                // PointerEvent
                let _ = read_exact(&mut sock, 5);
                saw_pointer = true;
            }
            6 => {
                let _ = read_exact(&mut sock, 3);
                let len = u32::from_be_bytes(read_exact(&mut sock, 4).try_into().unwrap());
                let _ = read_exact(&mut sock, len as usize);
            }
            _ => break,
        }
        if sent_frame && saw_pointer && saw_key {
            break;
        }
    }
}

fn wait_for(
    handle: &SessionHandle,
    timeout: Duration,
    pred: impl Fn(&SessionEvent) -> bool,
) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        for ev in handle.drain() {
            if pred(&ev) {
                return true;
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

fn request_for(port: u16) -> ConnectRequest {
    ConnectRequest {
        connection_id: None,
        name: "mock".into(),
        host: "127.0.0.1".into(),
        port,
        password: None,
        encryption: EncryptionMode::Off,
        quality: QualityPreset::Fast,
        view_only: false,
        shared: true,
    }
}

#[test]
fn handshake_frame_and_input() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        mock_rfb_server(sock);
    });

    let handle = SessionHandle::spawn(request_for(addr.port()));

    assert!(
        wait_for(&handle, Duration::from_secs(5), |e| matches!(
            e,
            SessionEvent::Connected {
                width: 8,
                height: 8,
                ..
            } | SessionEvent::FrameReady { .. }
        )),
        "expected connected/frame event"
    );
    assert!(
        wait_for(&handle, Duration::from_secs(3), |e| {
            matches!(e, SessionEvent::FrameReady { .. })
        }) || handle.framebuffer.lock().unwrap().width == 8,
        "expected a framebuffer"
    );

    {
        let fb = handle.framebuffer.lock().unwrap();
        assert_eq!(fb.width, 8);
        assert_eq!(fb.height, 8);
        assert_eq!(&fb.pixels[0..4], &[0, 80, 200, 255]);
    }

    handle.pointer(1, 1, 1);
    handle.key(0xff0d, true);
    handle.key(0xff0d, false);

    thread::sleep(Duration::from_millis(200));
    handle.close();
    drop(handle);
    let _ = server.join();
}

/// Regression: a busy desktop produces one `FrameReady` per rectangle. The UI
/// rebuilt its image for every one of them, stalling the thread that also
/// delivers key events; the late key-up made the remote auto-repeat the key.
#[test]
fn drain_collapses_frame_events_to_newest_generation() {
    let mut events = vec![
        SessionEvent::Status("a".into()),
        SessionEvent::FrameReady { generation: 1 },
        SessionEvent::FrameReady { generation: 2 },
        SessionEvent::Bell,
        SessionEvent::FrameReady { generation: 3 },
        SessionEvent::Clipboard("x".into()),
    ];
    coalesce_frames(&mut events);
    assert!(
        matches!(
            events.as_slice(),
            [
                SessionEvent::Status(_),
                SessionEvent::Bell,
                SessionEvent::FrameReady { generation: 3 },
                SessionEvent::Clipboard(_),
            ]
        ),
        "unexpected events: {events:?}"
    );

    let mut none = vec![SessionEvent::Bell, SessionEvent::Disconnected];
    coalesce_frames(&mut none);
    assert!(matches!(
        none.as_slice(),
        [SessionEvent::Bell, SessionEvent::Disconnected]
    ));

    let mut single = vec![SessionEvent::FrameReady { generation: 7 }];
    coalesce_frames(&mut single);
    assert!(matches!(
        single.as_slice(),
        [SessionEvent::FrameReady { generation: 7 }]
    ));
}

/// Server that floods the client with tiny Raw rectangles as fast as the
/// socket accepts them, while reporting every KeyEvent it reads on `keys`.
fn flooding_rfb_server(mut sock: TcpStream, keys: mpsc::Sender<(u32, bool)>) {
    const RECTS_PER_UPDATE: u16 = 256;
    rfb_handshake(&mut sock, 64, 64, b"flood");

    let mut writer = sock.try_clone().unwrap();
    let flood = thread::spawn(move || {
        // One update = RECTS_PER_UPDATE 1×1 Raw rects (16 bytes each).
        let mut update = vec![0u8, 0];
        update.extend_from_slice(&RECTS_PER_UPDATE.to_be_bytes());
        for i in 0..RECTS_PER_UPDATE {
            update.extend_from_slice(&(i % 64).to_be_bytes());
            update.extend_from_slice(&(i / 64).to_be_bytes());
            update.extend_from_slice(&1u16.to_be_bytes());
            update.extend_from_slice(&1u16.to_be_bytes());
            update.extend_from_slice(&0u32.to_be_bytes());
            update.extend_from_slice(&[200, 30, 30, 255]);
        }
        while writer.write_all(&update).is_ok() {}
    });

    sock.set_read_timeout(Some(Duration::from_secs(10))).ok();
    loop {
        let mut typ = [0u8; 1];
        if sock.read_exact(&mut typ).is_err() {
            break;
        }
        match typ[0] {
            0 => {
                let _ = read_exact(&mut sock, 3 + 16);
            }
            2 => {
                let _ = read_exact(&mut sock, 1);
                let n = u16::from_be_bytes(read_exact(&mut sock, 2).try_into().unwrap());
                let _ = read_exact(&mut sock, n as usize * 4);
            }
            3 => {
                let _ = read_exact(&mut sock, 9);
            }
            4 => {
                let b = read_exact(&mut sock, 7);
                let keysym = u32::from_be_bytes(b[3..7].try_into().unwrap());
                if keys.send((keysym, b[0] == 1)).is_err() {
                    break;
                }
            }
            5 => {
                let _ = read_exact(&mut sock, 5);
            }
            6 => {
                let _ = read_exact(&mut sock, 3);
                let len = u32::from_be_bytes(read_exact(&mut sock, 4).try_into().unwrap());
                let _ = read_exact(&mut sock, len as usize);
            }
            _ => break,
        }
    }
    let _ = sock.shutdown(std::net::Shutdown::Both);
    let _ = flood.join();
}

/// Regression for one keystroke typing a whole row of characters.
///
/// A streaming server delivers hundreds of rectangles per second. The UI used
/// to rebuild its image once per rectangle, stalling the thread that also
/// delivers key events; the delayed key-up made the remote auto-repeat the
/// key. Two guarantees keep that from coming back: `drain()` reports at most
/// one frame per call however many rectangles arrived, and input reaches the
/// server promptly while the flood is in progress.
#[test]
fn key_events_are_not_starved_by_frame_updates() {
    const KEY_TIMEOUT: Duration = Duration::from_millis(500);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (keys_tx, keys_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        flooding_rfb_server(sock, keys_tx);
    });

    let handle = SessionHandle::spawn(request_for(addr.port()));
    assert!(
        wait_for(&handle, Duration::from_secs(5), |e| matches!(
            e,
            SessionEvent::FrameReady { .. }
        )),
        "expected frames from the flooding server"
    );
    // Let the backlog of decoded rectangles build up before typing.
    thread::sleep(Duration::from_millis(300));

    let before = handle.framebuffer.lock().unwrap().generation;
    let events = handle.drain();
    let after = handle.framebuffer.lock().unwrap().generation;
    assert!(
        after - before > 1,
        "expected many rectangles during the pause, generation {before} -> {after}"
    );
    let frames = events
        .iter()
        .filter(|e| matches!(e, SessionEvent::FrameReady { .. }))
        .count();
    assert!(
        frames <= 1,
        "drain() must report at most one frame per call, got {frames}"
    );

    for round in 0..3 {
        let keysym = u32::from(b'a') + round;
        let pressed = Instant::now();
        handle.key(keysym, true);
        handle.key(keysym, false);
        for expect_down in [true, false] {
            let got = keys_rx
                .recv_timeout(KEY_TIMEOUT)
                .unwrap_or_else(|_| panic!("key event {round} lost behind frame updates"));
            assert_eq!(got, (keysym, expect_down));
        }
        let latency = pressed.elapsed();
        assert!(
            latency < KEY_TIMEOUT,
            "key round {round} took {latency:?} to reach the server"
        );
        thread::sleep(Duration::from_millis(100));
    }

    handle.close();
    drop(handle);
    let _ = server.join();
}
