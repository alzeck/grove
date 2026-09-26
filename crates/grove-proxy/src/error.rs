use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum CaError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path}: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error("certificate generation failed: {0}")]
    Generate(#[from] rcgen::Error),
    #[error("TLS key rejected: {0}")]
    Tls(#[from] rustls::Error),
    #[error("`security {command}` failed: {message}")]
    Security {
        command: &'static str,
        message: String,
    },
}

impl CaError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        CaError::Io {
            path: path.into(),
            source,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("can't listen on {addr}: {source}{}", held_by(.owner))]
    Bind {
        addr: SocketAddr,
        #[source]
        source: io::Error,
        /// The process holding the port, e.g. `caddy (pid 812)`, when `lsof`
        /// can tell.
        owner: Option<String>,
    },
    #[error("TLS setup failed: {0}")]
    Tls(#[from] rustls::Error),
}

fn held_by(owner: &Option<String>) -> String {
    owner
        .as_ref()
        .map(|o| format!(" (held by {o})"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_message_names_the_owner() {
        let err = ProxyError::Bind {
            addr: "127.0.0.1:443".parse().unwrap(),
            source: io::Error::from(io::ErrorKind::AddrInUse),
            owner: Some("caddy (pid 812)".into()),
        };
        let msg = err.to_string();
        assert!(msg.starts_with("can't listen on 127.0.0.1:443: "), "{msg}");
        assert!(msg.ends_with(" (held by caddy (pid 812))"), "{msg}");
    }
}
