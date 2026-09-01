use thiserror::Error;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("VNC error: {0}")]
    Vnc(String),
    #[error("TLS error: {0}")]
    Tls(String),
    #[error("connection timed out")]
    Timeout,
    #[error("{0}")]
    Message(String),
}

impl From<vnc::VncError> for SessionError {
    fn from(e: vnc::VncError) -> Self {
        match e {
            // vnc-rs reports a dropped socket as "client isn't started".
            vnc::VncError::ClientNotRunning => {
                Self::Message("The server closed the connection".into())
            }
            e => Self::Vnc(e.to_string()),
        }
    }
}

impl From<openssl::error::ErrorStack> for SessionError {
    fn from(e: openssl::error::ErrorStack) -> Self {
        Self::Tls(e.to_string())
    }
}

impl SessionError {
    pub fn msg(text: impl Into<String>) -> Self {
        Self::Message(text.into())
    }
}
