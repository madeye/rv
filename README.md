# RV

A native desktop VNC viewer written in Rust with [GPUI](https://www.gpui.rs/). Direct RFB (RFC 6143) connections, address-book chrome, and a session toolbar inspired by RealVNC Classic Viewer / Connect Viewer — original branding, not a RealVNC product.

## Features

- Address book with search, labels, list/grid views, and saved connections
- Session window with pinned or floating toolbar
- Mouse, keyboard, Ctrl+Alt+Del / extra keys
- Fit / 1:1 / stretch scaling and fullscreen
- Clipboard sync (Latin-1, per RFB)
- VNC Auth, Tight / ZRLE / TRLE / Raw encodings
- Optional VeNCrypt TLS (`Prefer on` / `Always`)
- Passwords stored in the OS keychain

## Build

```bash
cargo run -p rv-app --release
```

Connect to a local server such as TigerVNC, TightVNC, x11vnc, QEMU, or `rustvncserver` on `127.0.0.1:5900`.

## Tests

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
```

## Layout

| Crate | Role |
| --- | --- |
| `rv-core` | Connection models, address book, keysyms, prefs |
| `rv-session` | Tokio RFB session, framebuffer, VeNCrypt |
| `rv-app` | GPUI UI |

macOS is the primary target. Linux and Windows should compile via GPUI.

## License

[MIT](LICENSE) © 2026 Max Lv
