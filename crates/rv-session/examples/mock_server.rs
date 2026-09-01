#![allow(non_snake_case)]
//! Tiny RFB 3.8 server for trying RV locally: `cargo run -p rv-session --example mock_server`

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Instant;

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

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

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

fn write_u16(s: &mut TcpStream, v: u16) {
    s.write_all(&v.to_be_bytes()).unwrap();
}

fn write_u32(s: &mut TcpStream, v: u32) {
    s.write_all(&v.to_be_bytes()).unwrap();
}

fn read_exact(s: &mut TcpStream, n: usize) -> std::io::Result<Vec<u8>> {
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

fn serve(mut sock: TcpStream) -> std::io::Result<()> {
    sock.write_all(b"RFB 003.008\n")?;
    let _ = read_exact(&mut sock, 12)?;
    sock.write_all(&[1, 1])?;
    let _ = read_exact(&mut sock, 1)?;
    write_u32(&mut sock, 0);
    let _ = read_exact(&mut sock, 1)?;

    let (WIDTH, HEIGHT) = size();
    write_u16(&mut sock, WIDTH);
    write_u16(&mut sock, HEIGHT);
    sock.write_all(&[32, 24, 0, 1])?;
    sock.write_all(&255u16.to_be_bytes())?;
    sock.write_all(&255u16.to_be_bytes())?;
    sock.write_all(&255u16.to_be_bytes())?;
    sock.write_all(&[16, 8, 0, 0, 0, 0])?;
    let name = b"RV demo desktop";
    write_u32(&mut sock, name.len() as u32);
    sock.write_all(name)?;

    let start = Instant::now();
    loop {
        let typ = match read_exact(&mut sock, 1) {
            Ok(b) => b[0],
            Err(_) => break,
        };
        match typ {
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
                write_u16(&mut sock, n);
                for i in 0..n {
                    let y = i * strip;
                    let h = if i == n - 1 { HEIGHT - y } else { strip };
                    write_u16(&mut sock, 0);
                    write_u16(&mut sock, y);
                    write_u16(&mut sock, WIDTH);
                    write_u16(&mut sock, h);
                    write_u32(&mut sock, 0);
                    let row = WIDTH as usize * 4;
                    sock.write_all(&pixels[y as usize * row..(y + h) as usize * row])?;
                }
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

fn main() {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:5901".into());
    let listener = TcpListener::bind(&addr).expect("bind");
    eprintln!("RV mock VNC server on {addr} (no password)");
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
