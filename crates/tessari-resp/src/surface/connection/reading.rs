//! Waiting for the client's next bytes — and, while the connection listens,
//! writing the messages published to it in the meantime.

use std::time::Duration;

use tessari_constants::{FRAME_STALL_SECONDS, RESP_IDLE_SECONDS, RESP_PUBSUB_DRAIN};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use super::{Connection, WATCH_STOPPING};
use crate::codec::Decoder;
use crate::surface::channels::Delivery;

/// What woke a listening connection.
enum Woke {
    Read(std::io::Result<usize>),
    Delivered(Option<Delivery>),
    Tick,
}

impl Connection {
    /// The next bytes, or `None` when the connection ended, idled out, stalled
    /// half-way through a command, or the node is stopping.
    pub(super) async fn next_bytes<S: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        decoder: &Decoder,
        stream: &mut S,
        chunk: &mut [u8],
    ) -> std::io::Result<Option<usize>> {
        if self.is_listening() {
            return self.next_while_listening(decoder, stream, chunk).await;
        }
        let mut idle = Duration::ZERO;
        loop {
            match tokio::time::timeout(WATCH_STOPPING, stream.read(chunk)).await {
                Ok(Ok(0)) => return Ok(None),
                Ok(Ok(read)) => return Ok(Some(read)),
                Ok(Err(why)) => return Err(why),
                Err(_) => {
                    idle = idle.saturating_add(WATCH_STOPPING);
                    let patience = if decoder.holds_a_part() {
                        FRAME_STALL_SECONDS
                    } else {
                        RESP_IDLE_SECONDS
                    };
                    if self.shared.stopping.asked() || idle >= Duration::from_secs(patience) {
                        return Ok(None);
                    }
                }
            }
        }
    }

    /// As [`Self::next_bytes`], writing each message published to the
    /// connection as it arrives. A listening connection does not idle out —
    /// waiting for messages is what it is for — but one cut off for not
    /// reading, or whose user may no longer read its space, is closed.
    async fn next_while_listening<S: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        decoder: &Decoder,
        stream: &mut S,
        chunk: &mut [u8],
    ) -> std::io::Result<Option<usize>> {
        let mut stalled = Duration::ZERO;
        let mut out = Vec::new();
        loop {
            let woke = {
                // Only a command changes what the connection listens on, and
                // none runs while it waits here.
                let Some(subscribed) = self.subscribed.as_mut() else {
                    return Ok(None);
                };
                tokio::select! {
                    biased;
                    read = stream.read(chunk) => Woke::Read(read),
                    delivery = subscribed.receiver.recv() => Woke::Delivered(delivery),
                    () = tokio::time::sleep(WATCH_STOPPING) => Woke::Tick,
                }
            };
            match woke {
                Woke::Read(Ok(0)) => return Ok(None),
                Woke::Read(Ok(read)) => return Ok(Some(read)),
                Woke::Read(Err(why)) => return Err(why),
                Woke::Delivered(None) => return Ok(None),
                Woke::Delivered(Some(first)) => {
                    if !self.write_deliveries(first, stream, &mut out).await? {
                        return Ok(None);
                    }
                }
                Woke::Tick => {
                    if self.shared.stopping.asked()
                        || self.subscribed.as_ref().is_some_and(|held| held.is_cut())
                        || !self.still_listening().await
                    {
                        return Ok(None);
                    }
                    stalled = if decoder.holds_a_part() {
                        stalled.saturating_add(WATCH_STOPPING)
                    } else {
                        Duration::ZERO
                    };
                    if stalled >= Duration::from_secs(FRAME_STALL_SECONDS) {
                        return Ok(None);
                    }
                }
            }
        }
    }

    /// Write `first` and whatever else is already waiting, up to a round's
    /// worth, in one write; `false` when the subscriber was cut off instead.
    async fn write_deliveries<S: AsyncWrite + Unpin>(
        &mut self,
        first: Delivery,
        stream: &mut S,
        out: &mut Vec<u8>,
    ) -> std::io::Result<bool> {
        let Some(subscribed) = self.subscribed.as_mut() else {
            return Ok(true);
        };
        out.clear();
        subscribed.taken(&first);
        first.encode(self.protocol, out);
        let mut examined = 1_usize;
        while examined < RESP_PUBSUB_DRAIN {
            let Ok(next) = subscribed.receiver.try_recv() else {
                break;
            };
            examined = examined.saturating_add(1);
            subscribed.taken(&next);
            next.encode(self.protocol, out);
        }
        if subscribed.is_cut() {
            tracing::info!("resp subscriber cut off: it did not read what was published to it");
            return Ok(false);
        }
        stream.write_all(out).await?;
        stream.flush().await?;
        Ok(true)
    }
}
