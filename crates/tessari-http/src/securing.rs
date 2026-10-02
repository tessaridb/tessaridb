//! TLS on the HTTP listener (ADR-0108 D4).
//!
//! A handshake is a conversation with the client, and the accept loop must not
//! wait on one: a client that opens a socket and sends nothing would otherwise
//! stall every connection behind it. So each handshake runs as its own task,
//! under the greeting's deadline, and the loop hands out whichever finishes
//! first. How many may be in flight is bounded by the node's connection
//! ceiling; beyond it a socket is closed unanswered, which costs a client that
//! is trying to exhaust the node more than it costs the node.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tessari_constants::{GREETING_SECONDS, MAX_CONNECTIONS};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use crate::listening::Accept;

/// A finished handshake, or nothing when it failed.
type Shaken = Option<(TlsStream<TcpStream>, SocketAddr)>;

/// TLS around another source of connections.
pub(crate) struct Securing<A> {
    accepting: A,
    acceptor: TlsAcceptor,
    shaking: JoinSet<Shaken>,
    room: Arc<Semaphore>,
}

impl<A> Securing<A> {
    pub(crate) fn new(accepting: A, acceptor: TlsAcceptor) -> Self {
        Self {
            accepting,
            acceptor,
            shaking: JoinSet::new(),
            room: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
        }
    }
}

impl<A: Accept<Io = TcpStream>> Accept for Securing<A> {
    type Io = TlsStream<TcpStream>;

    async fn accept(&mut self) -> io::Result<(Self::Io, SocketAddr)> {
        loop {
            tokio::select! {
                Some(done) = self.shaking.join_next(), if !self.shaking.is_empty() => {
                    // A handshake that failed, timed out or panicked is that
                    // client's problem and nobody else's.
                    if let Ok(Some(secured)) = done {
                        return Ok(secured);
                    }
                }
                accepted = self.accepting.accept() => {
                    let (stream, from) = accepted?;
                    let Ok(held) = Arc::clone(&self.room).try_acquire_owned() else {
                        log::warn!(
                            "an HTTP connection from {from} was closed: {MAX_CONNECTIONS} \
                             TLS handshakes already in flight"
                        );
                        continue;
                    };
                    let acceptor = self.acceptor.clone();
                    self.shaking.spawn(async move {
                        let shaken = tokio::time::timeout(
                            Duration::from_secs(GREETING_SECONDS),
                            acceptor.accept(stream),
                        )
                        .await;
                        drop(held);
                        match shaken {
                            Ok(Ok(secured)) => Some((secured, from)),
                            Ok(Err(why)) => {
                                log::info!("an HTTP connection from {from} failed its TLS handshake: {why}");
                                None
                            }
                            Err(_) => {
                                log::info!("an HTTP connection from {from} did not finish its TLS handshake in time");
                                None
                            }
                        }
                    });
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.accepting.local_addr()
    }
}
