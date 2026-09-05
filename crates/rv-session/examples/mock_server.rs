#![allow(non_snake_case)]
//! Tiny RFB 3.8 server for trying RV locally: `cargo run -p rv-session --example mock_server`
//!
//! `RV_MOCK_TLS=1` makes it offer VeNCrypt (security type 19) with the
//! anonymous-DH `TLSVnc` subtype — the same thing stock TigerVNC does — so the
//! encrypted transport and its input path can be exercised without a real
//! server. Any 16-byte VncAuth response is accepted.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use openssl::dh::Dh;
use openssl::ssl::{Ssl, SslContext, SslMethod, SslVersion};

fn size() -> (u16, u16) {
    static SIZE: OnceLock<(u16, u16)> = OnceLock::new();
    *SIZE.get_or_init(|| {
        std::env::var("RV_MOCK_SIZE")
            .ok()
            .and_then(|s| {
                let (w, h) = s.split_once('x')?;
                Some((w.parse().ok()?, h.parse().ok()?))
            })
            .unwrap_or((640, 360))
    })
}

/// Number of horizontal strips each update is split into (mimics real servers).
fn rects() -> u16 {
    std::env::var("RV_MOCK_RECTS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
}

fn tls_enabled() -> bool {
    matches!(std::env::var("RV_MOCK_TLS").as_deref(), Ok("1"))
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

fn write_u16<S: Write>(s: &mut S, v: u16) -> std::io::Result<()> {
    s.write_all(&v.to_be_bytes())
}

fn write_u32<S: Write>(s: &mut S, v: u32) -> std::io::Result<()> {
    s.write_all(&v.to_be_bytes())
}

fn read_exact<S: Read>(s: &mut S, n: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf)?;
    Ok(buf)
}

/// Frame in the BGRA layout the viewer negotiates.
fn frame_bgra(t: f32) -> Vec<u8> {
    let (WIDTH, HEIGHT) = size();
    let mut px = vec![0u8; WIDTH as usize * HEIGHT as usize * 4];
    let cx = (WIDTH as f32 * (0.5 + 0.35 * t.sin())) as i32;
    let cy = (HEIGHT as f32 * (0.5 + 0.25 * (t * 0.7).cos())) as i32;
    for y in 0..HEIGHT as i32 {
        for x in 0..WIDTH as i32 {
            let i = ((y * WIDTH as i32 + x) * 4) as usize;
            let dx = x - cx;
            let dy = y - cy;
            let inside = dx * dx + dy * dy < 48 * 48;
            let (r, g, b) = if inside {
                (255, 210, 40)
            } else {
                (
                    (x * 255 / WIDTH as i32) as u8,
                    70,
                    (y * 255 / HEIGHT as i32) as u8,
                )
            };
            px[i] = b;
            px[i + 1] = g;
            px[i + 2] = r;
            px[i + 3] = 255;
        }
    }
    px
}

/// RFB version exchange plus security negotiation, ending with SecurityResult
/// OK. Returns the (optionally TLS-wrapped) stream ready for ClientInit.
fn negotiate(mut sock: TcpStream) -> std::io::Result<Box<dyn ReadWrite>> {
    sock.write_all(b"RFB 003.008\n")?;
    let _ = read_exact(&mut sock, 12)?;

    if !tls_enabled() {
        sock.write_all(&[1, 1])?; // one type: None
        let _ = read_exact(&mut sock, 1)?; // client's choice
        write_u32(&mut sock, 0)?; // SecurityResult OK
        return Ok(Box::new(sock));
    }

    // Offer VeNCrypt only, like a TLS-required TigerVNC.
    sock.write_all(&[1, 19])?;
    let choice = read_exact(&mut sock, 1)?[0];
    assert_eq!(choice, 19, "client must pick VeNCrypt");

    // VeNCrypt 0.2, then the client's chosen version back.
    sock.write_all(&[0, 2])?;
    let ver = read_exact(&mut sock, 2)?;
    assert_eq!(ver, [0, 2]);
    sock.write_all(&[0])?; // version OK

    // One subtype: TLSVnc (258).
    sock.write_all(&[1])?;
    write_u32(&mut sock, 258)?;
    let sub = u32::from_be_bytes(read_exact(&mut sock, 4)?.try_into().unwrap());
    assert_eq!(sub, 258, "client must pick TLSVnc");
    sock.write_all(&[1])?; // accept

    // Anonymous-DH TLS: no certificate, matching TigerVNC's TLSVnc.
    let mut builder = SslContext::builder(SslMethod::tls_server()).unwrap();
    builder.set_security_level(0);
    builder.set_cipher_list("aNULL:!eNULL").unwrap();
    builder
        .set_max_proto_version(Some(SslVersion::TLS1_2))
        .unwrap();
    builder.set_tmp_dh(&Dh::get_2048_256().unwrap()).unwrap();
    let ssl = Ssl::new(&builder.build()).unwrap();
    let mut tls = openssl::ssl::SslStream::new(ssl, sock).unwrap();
    tls.accept()
        .map_err(|e| std::io::Error::other(format!("tls accept: {e}")))?;

    // Post-TLS auth is VncAuth: 16-byte challenge, any response accepted.
    tls.write_all(&[0u8; 16])?;
    let _response = read_exact(&mut tls, 16)?;
    write_u32(&mut tls, 0)?; // SecurityResult OK
    Ok(Box::new(tls))
}

fn serve(sock: TcpStream) -> std::io::Result<()> {
    let mut sock = negotiate(sock)?;

    let _ = read_exact(&mut sock, 1)?; // ClientInit (shared flag)

    let (WIDTH, HEIGHT) = size();
    write_u16(&mut sock, WIDTH)?;
    write_u16(&mut sock, HEIGHT)?;
    sock.write_all(&[32, 24, 0, 1])?;
    sock.write_all(&255u16.to_be_bytes())?;
    sock.write_all(&255u16.to_be_bytes())?;
    sock.write_all(&255u16.to_be_bytes())?;
    sock.write_all(&[16, 8, 0, 0, 0, 0])?;
    let name = b"RV demo desktop";
    write_u32(&mut sock, name.len() as u32)?;
    sock.write_all(name)?;

    let start = Instant::now();
    while let Ok(message) = read_exact(&mut sock, 1) {
        match message[0] {
            0 => {
                let _ = read_exact(&mut sock, 3 + 16)?;
            }
            2 => {
                let _ = read_exact(&mut sock, 1)?;
                let n = u16::from_be_bytes(read_exact(&mut sock, 2)?.try_into().unwrap());
                let _ = read_exact(&mut sock, n as usize * 4)?;
            }
            3 => {
                let _ = read_exact(&mut sock, 9)?;
                let t = start.elapsed().as_secs_f32();
                let pixels = frame_bgra(t);
                let n = rects().clamp(1, HEIGHT);
                let strip = HEIGHT / n;
                sock.write_all(&[0, 0])?;
                write_u16(&mut sock, n)?;
                for i in 0..n {
                    let y = i * strip;
                    let h = if i == n - 1 { HEIGHT - y } else { strip };
                    write_u16(&mut sock, 0)?;
                    write_u16(&mut sock, y)?;
                    write_u16(&mut sock, WIDTH)?;
                    write_u16(&mut sock, h)?;
                    write_u32(&mut sock, 0)?;
                    let row = WIDTH as usize * 4;
                    sock.write_all(&pixels[y as usize * row..(y + h) as usize * row])?;
                }
                sock.flush()?;
            }
            4 => {
                let b = read_exact(&mut sock, 7)?;
                let keysym = u32::from_be_bytes(b[3..7].try_into().unwrap());
                eprintln!(
                    "[{}] key {} keysym=0x{keysym:04x} {:?}",
                    now_ms(),
                    if b[0] == 1 { "DOWN" } else { "UP  " },
                    char::from_u32(keysym).filter(|c| c.is_ascii_graphic()),
                );
            }
            5 => {
                let b = read_exact(&mut sock, 5)?;
                let x = u16::from_be_bytes([b[1], b[2]]);
                let y = u16::from_be_bytes([b[3], b[4]]);
                // Only log button transitions; motion would flood the log.
                if b[0] != 0 {
                    eprintln!(
                        "[{}] pointer buttons=0b{:08b} at ({x}, {y})",
                        now_ms(),
                        b[0]
                    );
                }
            }
            6 => {
                let _ = read_exact(&mut sock, 3)?;
                let len = u32::from_be_bytes(read_exact(&mut sock, 4)?.try_into().unwrap());
                let _ = read_exact(&mut sock, len as usize)?;
            }
            _ => break,
        }
    }
    Ok(())
}

/// Object-safe `Read + Write` so plain and TLS streams share the message loop.
trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

fn main() {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:5901".into());
    let listener = TcpListener::bind(&addr).expect("bind");
    let tls = if tls_enabled() {
        " (VeNCrypt TLSVnc)"
    } else {
        " (no password)"
    };
    eprintln!("RV mock VNC server on {addr}{tls}");
    for incoming in listener.incoming() {
        match incoming {
            Ok(sock) => {
                std::thread::spawn(move || {
                    if let Err(e) = serve(sock) {
                        eprintln!("client ended: {e}");
                    }
                });
            }
            Err(e) => eprintln!("accept: {e}"),
        }
    }
}
