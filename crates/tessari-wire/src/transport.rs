//! What a [`crate::Client`] talks over: a plain socket, or TLS on one.
//!
//! The client reads through one buffer and writes through another, which a
//! plain socket allows by cloning its handle. A TLS session cannot be cloned —
//! both directions share its state — so its two handles share one session
//! behind a lock. The client is synchronous and never reads and writes at the
//! same moment, so the lock is never contended; it exists because the two
//! buffers each need an owner.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
#[cfg(feature = "tls")]
use std::sync::{Arc, Mutex, PoisonError};

/// One handle on a connection.
pub(crate) enum Transport {
    Plain(TcpStream),
    #[cfg(feature = "tls")]
    Tls(Arc<Mutex<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>>),
}

impl Transport {
    /// A second handle on the same connection.
    pub(crate) fn duplicate(&self) -> std::io::Result<Self> {
        match self {
            Self::Plain(stream) => Ok(Self::Plain(stream.try_clone()?)),
            #[cfg(feature = "tls")]
            Self::Tls(session) => Ok(Self::Tls(Arc::clone(session))),
        }
    }

    /// The other end, for a `Debug` line.
    pub(crate) fn peer_addr(&self) -> std::io::Result<SocketAddr> {
        match self {
            Self::Plain(stream) => stream.peer_addr(),
            #[cfg(feature = "tls")]
            Self::Tls(session) => session
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .sock
                .peer_addr(),
        }
    }
}

/// The other end and whether it is encrypted, never the session's state.
impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            Self::Plain(_) => "plain",
            #[cfg(feature = "tls")]
            Self::Tls(_) => "tls",
        };
        f.debug_struct("Transport")
            .field("kind", &kind)
            .field("peer", &self.peer_addr().ok())
            .finish()
    }
}

impl Read for Transport {
    fn read(&mut self, into: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(into),
            #[cfg(feature = "tls")]
            Self::Tls(session) => session
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .read(into),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, from: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write(from),
            #[cfg(feature = "tls")]
            Self::Tls(session) => session
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .write(from),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(stream) => stream.flush(),
            #[cfg(feature = "tls")]
            Self::Tls(session) => session
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .flush(),
        }
    }
}

/// The name a node's certificate must carry: the host part of `address`.
///
/// An IP address is a name too — rustls checks it against the certificate's IP
/// entries — so `127.0.0.1:9080` is verified as strictly as `db.example:9080`.
#[cfg(feature = "tls")]
pub(crate) fn server_name(
    address: &str,
) -> crate::error::Result<rustls::pki_types::ServerName<'static>> {
    let host = address
        .rsplit_once(':')
        .map_or(address, |(host, _)| host)
        .trim_start_matches('[')
        .trim_end_matches(']');
    rustls::pki_types::ServerName::try_from(host.to_owned()).map_err(|why| {
        crate::error::Error::Tls(format!(
            "{host:?} is not a name a certificate carries: {why}"
        ))
    })
}

#[cfg(all(test, feature = "tls"))]
mod tests {
    use rustls::pki_types::ServerName;

    use super::server_name;

    #[test]
    fn the_name_checked_is_the_host_without_its_port() {
        assert_eq!(
            server_name("db.example:9080").expect("a name"),
            ServerName::try_from("db.example").expect("the host alone")
        );
        assert_eq!(
            server_name("[::1]:9080").expect("an address"),
            ServerName::try_from("::1").expect("the address without brackets")
        );
        assert!(matches!(
            server_name("127.0.0.1:9080"),
            Ok(ServerName::IpAddress(_))
        ));
        assert!(server_name("not a host:9080").is_err());
    }
}
