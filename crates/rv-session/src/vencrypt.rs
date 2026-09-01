//! VeNCrypt (security type 19) handshake, then a synthetic RFB 3.8 prefix so
//! `vnc-rs` can finish authentication on the TLS stream.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::SessionError;

pub const VENCRYPT_SECURITY_TYPE: u8 = 19;
const TLS_NONE: u32 = 257;
const TLS_VNC: u32 = 258;
const X509_NONE: u32 = 260;
const X509_VNC: u32 = 261;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VencryptAuth {
    None,
    Vnc,
}

pub struct TlsRfbStream {
    inner: tokio_rustls::client::TlsStream<TcpStream>,
    prefix: Vec<u8>,
    prefix_off: usize,
    swallow_write: usize,
}

impl TlsRfbStream {
    fn new(inner: tokio_rustls::client::TlsStream<TcpStream>, auth: VencryptAuth) -> Self {
        // Replay a 3.8 security list matching the post-TLS auth so VncConnector
        // can run its usual handshake against the TLS stream.
        let mut prefix = b"RFB 003.008\n".to_vec();
        prefix.push(1); // one security type
        prefix.push(match auth {
            VencryptAuth::None => 1,
            VencryptAuth::Vnc => 2,
        });
        Self {
            inner,
            prefix,
            prefix_off: 0,
            swallow_write: 12 + 1, // version + chosen type
        }
    }
}

impl AsyncRead for TlsRfbStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.prefix_off < self.prefix.len() {
            let rest = &self.prefix[self.prefix_off..];
            let n = rest.len().min(buf.remaining());
            buf.put_slice(&rest[..n]);
            self.prefix_off += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for TlsRfbStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.swallow_write > 0 {
            let n = buf.len().min(self.swallow_write);
            self.swallow_write -= n;
            return Poll::Ready(Ok(n));
        }
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

// Handshake is sequential; vnc-rs requires Sync on the stream type even though
// I/O is never overlapping across threads.
unsafe impl Sync for TlsRfbStream {}

pub async fn handshake(
    mut stream: TcpStream,
    host: &str,
    require_cert: bool,
) -> Result<(TlsRfbStream, VencryptAuth), SessionError> {
    ensure_crypto();

    stream.write_u8(VENCRYPT_SECURITY_TYPE).await?;

    let major = stream.read_u8().await?;
    let minor = stream.read_u8().await?;
    if major != 0 || minor < 2 {
        return Err(SessionError::msg(format!(
            "unsupported VeNCrypt version {major}.{minor}"
        )));
    }
    stream.write_all(&[0, 2]).await?;

    let version_ok = stream.read_u8().await?;
    if version_ok != 0 {
        return Err(SessionError::msg("server rejected VeNCrypt 0.2"));
    }

    let n = stream.read_u8().await?;
    if n == 0 {
        return Err(SessionError::msg("server offered no VeNCrypt subtypes"));
    }
    let mut subtypes = Vec::with_capacity(n as usize);
    for _ in 0..n {
        subtypes.push(stream.read_u32().await?);
    }

    let (chosen, auth, x509) = pick_subtype(&subtypes).ok_or_else(|| {
        SessionError::msg(format!(
            "no supported VeNCrypt subtype in {subtypes:?} (want TLSVnc / TLSNone / X509*)"
        ))
    })?;

    stream.write_u32(chosen).await?;
    let accept = stream.read_u8().await?;
    if accept != 1 {
        return Err(SessionError::msg(
            "server rejected the chosen VeNCrypt subtype",
        ));
    }

    let connector = tls_connector(x509 || require_cert)?;
    let server_name = server_name(host);
    let tls = connector
        .connect(server_name, stream)
        .await
        .map_err(|e| SessionError::Tls(e.to_string()))?;

    Ok((TlsRfbStream::new(tls, auth), auth))
}

/// Choose the VeNCrypt subtype. X509 variants come first: the plain `TLS*`
/// subtypes mean anonymous Diffie-Hellman in every common server, which
/// rustls refuses to negotiate, so they only work as a last resort.
fn pick_subtype(available: &[u32]) -> Option<(u32, VencryptAuth, bool)> {
    const PREF: [(u32, VencryptAuth, bool); 4] = [
        (X509_VNC, VencryptAuth::Vnc, true),
        (TLS_VNC, VencryptAuth::Vnc, false),
        (X509_NONE, VencryptAuth::None, true),
        (TLS_NONE, VencryptAuth::None, false),
    ];
    for (id, auth, x509) in PREF {
        if available.contains(&id) {
            return Some((id, auth, x509));
        }
    }
    None
}

fn server_name(host: &str) -> ServerName<'static> {
    ServerName::try_from(host.to_string())
        .unwrap_or_else(|_| ServerName::try_from("vnc.local").expect("static name"))
}

fn tls_connector(verify: bool) -> Result<TlsConnector, SessionError> {
    let config = if verify {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    } else {
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
            .with_no_client_auth()
    };
    Ok(TlsConnector::from(Arc::new(config)))
}

fn ensure_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[derive(Debug)]
struct NoVerifier;

impl rustls::client::danger::ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_x509_over_anonymous_tls() {
        let (id, auth, x509) = pick_subtype(&[TLS_VNC, X509_VNC]).unwrap();
        assert_eq!((id, auth, x509), (X509_VNC, VencryptAuth::Vnc, true));
        let (id, auth, x509) = pick_subtype(&[TLS_NONE, TLS_VNC]).unwrap();
        assert_eq!((id, auth, x509), (TLS_VNC, VencryptAuth::Vnc, false));
        assert!(pick_subtype(&[999]).is_none());
    }

    #[test]
    fn prefix_replays_rfb_38_security_list() {
        let prefix_for = |auth: VencryptAuth| {
            let mut prefix = b"RFB 003.008\n".to_vec();
            prefix.push(1);
            prefix.push(match auth {
                VencryptAuth::None => 1,
                VencryptAuth::Vnc => 2,
            });
            prefix
        };
        assert_eq!(prefix_for(VencryptAuth::Vnc).last(), Some(&2));
        assert_eq!(prefix_for(VencryptAuth::None).last(), Some(&1));
    }
}
