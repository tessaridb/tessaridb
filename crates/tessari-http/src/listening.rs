//! The HTTP listener, which ends the surface on an accept failure that will not
//! pass (Q-834).
//!
//! axum's own listener logs such a failure and tries again every second for
//! ever, so a listener gone bad leaves a process its supervisor sees as healthy
//! and nobody can reach — the state Q-834 found the old wire loop in. This one
//! retries what passes on its own ([`tessari_serve::passes`]), and on anything
//! else records the failure and ends the server, so [`crate::Node::serve`]
//! returns it and the node ends as the wire surface's would.

use std::io;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// Where connections come from: the runtime's listener, or a test's script.
pub(crate) trait Accept: Send + 'static {
    fn accept(&mut self) -> impl Future<Output = io::Result<(TcpStream, SocketAddr)>> + Send;
    fn local_addr(&self) -> io::Result<SocketAddr>;
}

impl Accept for TcpListener {
    fn accept(&mut self) -> impl Future<Output = io::Result<(TcpStream, SocketAddr)>> + Send {
        Self::accept(self)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Self::local_addr(self)
    }
}

/// A listener that gives up on a failure that will not pass.
pub(crate) struct Listening<A> {
    accepting: A,
    /// Why the listener ended, handed once to [`crate::Node::serve`] to return.
    /// A one-shot channel rather than a shared slot: one value crosses from
    /// the accept loop to the caller, once, and nothing needs a lock for that.
    failed: Option<oneshot::Sender<io::Error>>,
    /// The other end, until the caller takes it.
    failure: Option<oneshot::Receiver<io::Error>>,
    /// Cancelled when it ends, which is what stops the server.
    ended: CancellationToken,
}

impl<A: Accept> Listening<A> {
    pub(crate) fn new(accepting: A) -> Self {
        let (failed, failure) = oneshot::channel();
        Self {
            accepting,
            failed: Some(failed),
            failure: Some(failure),
            ended: CancellationToken::new(),
        }
    }

    /// Cancelled when the listener gives up.
    pub(crate) fn ended(&self) -> CancellationToken {
        self.ended.clone()
    }

    /// Where the listener's failure will arrive, once it has one; `None` once
    /// it has been taken.
    pub(crate) fn failure(&mut self) -> Option<oneshot::Receiver<io::Error>> {
        self.failure.take()
    }
}

impl<A: Accept> axum::serve::Listener for Listening<A> {
    type Io = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (TcpStream, SocketAddr) {
        loop {
            match self.accepting.accept().await {
                Ok(accepted) => return accepted,
                Err(why) if tessari_serve::passes(&why) => {
                    log::warn!(
                        "accepting an HTTP connection failed ({why}); resting before the next"
                    );
                    tokio::time::sleep(tessari_serve::ACCEPT_PAUSE).await;
                }
                Err(why) => {
                    log::error!("the HTTP listener failed ({why})");
                    if let Some(failed) = self.failed.take() {
                        // A caller that stopped listening for the failure has
                        // nothing to be told.
                        drop(failed.send(why));
                    }
                    self.ended.cancel();
                    // Nothing is accepted after this; the server stops on `ended`.
                    std::future::pending::<()>().await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.accepting.local_addr()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use std::collections::VecDeque;
    use std::io;
    use std::net::SocketAddr;

    use axum::serve::Listener;
    use tokio::net::{TcpListener, TcpStream};

    use super::{Accept, Listening};

    /// Accepts that answer from a script, then wait for ever.
    struct Scripted(VecDeque<io::Result<(TcpStream, SocketAddr)>>);

    impl Accept for Scripted {
        async fn accept(&mut self) -> io::Result<(TcpStream, SocketAddr)> {
            match self.0.pop_front() {
                Some(next) => next,
                None => std::future::pending().await,
            }
        }

        fn local_addr(&self) -> io::Result<SocketAddr> {
            Err(io::ErrorKind::Unsupported.into())
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("a runtime")
    }

    #[test]
    fn a_listener_gone_bad_ends_the_server_and_says_why() {
        runtime().block_on(async {
            let mut listening = Listening::new(Scripted(VecDeque::from([
                Err(io::ErrorKind::ConnectionAborted.into()),
                // EBADF: the descriptor is gone, and axum's listener would retry
                // it every second for as long as the process lived.
                Err(io::Error::from_raw_os_error(9)),
            ])));
            let ended = listening.ended();
            let mut failure = listening.failure().expect("the failure's receiver");
            // Bounded, so a listener that retried instead of ending fails here
            // rather than hanging; paused time makes the bound instant.
            let waited = tokio::time::timeout(std::time::Duration::from_secs(60), async {
                tokio::select! {
                    biased;
                    _ = listening.accept() => panic!("a listener gone bad handed out a connection"),
                    () = ended.cancelled() => {}
                }
            })
            .await;
            assert!(waited.is_ok(), "a listener gone bad went on retrying");
            let recorded = failure.try_recv().ok();
            assert_eq!(
                recorded.and_then(|why| why.raw_os_error()),
                Some(9),
                "ended, but not for the failure that ended it"
            );
        });
    }

    #[test]
    fn a_failure_that_passes_is_waited_out_and_the_next_connection_served() {
        runtime().block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
            let address = listener.local_addr().expect("its address");
            let _client = TcpStream::connect(address).await.expect("a connection");
            let accepted = listener.accept().await.expect("the connection");
            let from = accepted.1;
            let mut listening = Listening::new(Scripted(VecDeque::from([
                Err(io::Error::from_raw_os_error(24)),
                Ok(accepted),
            ])));
            let (_, served_from) = listening.accept().await;
            assert_eq!(served_from, from);
            assert!(
                !listening.ended().is_cancelled(),
                "a passing failure ended the server"
            );
        });
    }
}
