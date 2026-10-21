//! A blocking pop's wait: its non-blocking form tried again whenever the store
//! commits something, until it finds something, its timeout passes, the client
//! leaves or the node stops.
//!
//! The connection keeps reading while it waits. That is how it notices a client
//! that left — which must not be handed an element, since the element would be
//! taken from the list and written to a socket nobody reads — and the commands
//! it reads are held for after the reply, as a pipeline is.
//!
//! Every commit wakes every waiting connection, which then tries its form once:
//! the store's commit signal says that something changed, not what (Q-G079-10).

use std::time::Duration;

use tessari_constants::RESP_BLOCKED_READ_AHEAD_BYTES;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::sync::watch;
use tokio::time::Instant;

use super::{Connection, WATCH_STOPPING};
use crate::codec::{Command, Decoder, Reply};
use crate::surface::commands::{Shape, found_nothing};

/// A blocking pop that found nothing on its first try.
pub(super) struct Blocked {
    form: Command,
    shape: Shape,
    until: Option<Instant>,
    commits: watch::Receiver<u64>,
    /// What the form answers when there is nothing, which is the reply when
    /// the timeout passes.
    empty: Reply,
}

impl Connection {
    /// Try `form` once; when it finds nothing, keep it in `self.blocked` for
    /// the connection to wait on, and answer what it found for now.
    pub(super) async fn block(
        &mut self,
        form: Command,
        wait: Option<Duration>,
        shape: Shape,
    ) -> Reply {
        let until = wait.and_then(|wait| Instant::now().checked_add(wait));
        // Subscribed BEFORE the first try, so that a commit landing between
        // the try and the wait still wakes it.
        let commits = self.shared.db.commits().watching();
        let reply = self.attempt(&form).await;
        if !found_nothing(&reply) {
            return shape.answer(reply);
        }
        self.blocked = Some(Blocked {
            form,
            shape,
            until,
            commits,
            empty: reply.clone(),
        });
        reply
    }

    /// The form, answered as the command it is.
    async fn attempt(&mut self, form: &Command) -> Reply {
        Box::pin(self.answer(form)).await.0
    }

    /// Wait out `blocked`: its reply, or `None` when the client left or the
    /// node is stopping and the connection ends.
    pub(super) async fn wait<S: AsyncRead + Unpin>(
        &mut self,
        mut blocked: Blocked,
        stream: &mut S,
        decoder: &mut Decoder,
        chunk: &mut [u8],
    ) -> std::io::Result<Option<Reply>> {
        let limited = blocked.until.is_some();
        let until = blocked.until.unwrap_or_else(Instant::now);
        loop {
            let reading = decoder.buffered() < RESP_BLOCKED_READ_AHEAD_BYTES;
            tokio::select! {
                biased;
                read = stream.read(chunk), if reading => match read? {
                    0 => return Ok(None),
                    read => decoder.extend(&chunk[..read]),
                },
                changed = blocked.commits.changed() => {
                    if changed.is_err() {
                        return Ok(Some(blocked.empty));
                    }
                    let busy = self.shared.stopping.busy();
                    let reply = self.attempt(&blocked.form).await;
                    drop(busy);
                    if !found_nothing(&reply) {
                        return Ok(Some(blocked.shape.answer(reply)));
                    }
                }
                () = tokio::time::sleep_until(until), if limited => {
                    return Ok(Some(blocked.empty));
                }
                () = tokio::time::sleep(WATCH_STOPPING) => {
                    if self.shared.stopping.asked() {
                        return Ok(None);
                    }
                }
            }
        }
    }
}
