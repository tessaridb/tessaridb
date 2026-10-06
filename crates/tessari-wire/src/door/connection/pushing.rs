//! A held stream the leader pushes (ADR-0120).
//!
//! The plain stream sends a round only in answer to an ask, and the ask is the
//! acknowledgement, so a commit that lands while a round is out waits for the
//! follower to apply that round and ask again — a whole round trip the leader
//! spends waiting to be asked. Here the follower names its positions once and
//! the leader keeps its own: it sends what lands as it lands, up to two rounds
//! unacknowledged, and counts only the `Held` reports the follower sends once a
//! round is durable. So it reads while it waits: a `Held` must be counted when
//! it arrives, not when the next commit happens to wake the loop.

use std::collections::VecDeque;

use tessari_types::Sequence;

use super::*;
use crate::collection::{Streamed, stream_answer_pushed, stream_held};

/// How many rounds the leader sends before one of them is acknowledged
/// (ADR-0120 D3): one being applied, and one on its way.
const UNACKNOWLEDGED: usize = 2;

impl<H: Holding> Connection<H> {
    /// Serve a pushed stream until the follower closes or the door stops.
    ///
    /// `body` is the opening `StreamFrom`: the positions the follower holds,
    /// counted as held and taken as where to send from.
    pub(super) async fn stream_pushed(
        &self,
        link: &mut tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
        follower: [u8; NODE_ID_LEN],
        body: Vec<u8>,
        presented: Option<&rustls::pki_types::CertificateDer<'_>>,
    ) -> Result<()> {
        let heartbeat = std::time::Duration::from_millis(STREAM_HEARTBEAT_MILLIS);
        // Asked again before every frame, for the plain stream's reason.
        let admitted = || presented.is_some_and(|presented| self.keys.still_admits(presented));
        let quiet = Streamed {
            answers: Vec::new(),
        }
        .encode();
        let mut commits = self.holding.commits();
        let (reading, mut writing) = tokio::io::split(link);
        // The follower's next frame, read while the leader waits. One future
        // kept across every turn of the loop and replaced only once it has
        // finished, because a frame half read must never be dropped.
        let next_frame = |mut reading: tokio::io::ReadHalf<_>| async move {
            let read = frame_async::read_tagged(&mut reading).await;
            (reading, read)
        };
        let mut frames = std::pin::pin!(next_frame(reading));
        // Where the next round begins, per log.
        let mut next = StreamAsk::decode(&body)?;
        self.held_by(follower, &next).await?;
        // Each round sent and not yet acknowledged: where it left every log.
        let mut unacknowledged: VecDeque<Vec<Sequence>> = VecDeque::new();
        loop {
            if unacknowledged.len() < UNACKNOWLEDGED {
                if !admitted() {
                    tracing::warn!(
                        "a held stream ended: the peer's certificate is no longer admitted here"
                    );
                    return Ok(());
                }
                // Seen before the read, so a commit landing during it wakes
                // the wait below rather than being slept past.
                commits.borrow_and_update();
                let holding = Arc::clone(&self.holding);
                let cut = next.clone();
                match self
                    .store(move || stream_answer_pushed(&*holding, follower, &cut))
                    .await
                {
                    Ok(round) if !round.is_quiet() => {
                        bounded(frame_async::write_tagged(
                            &mut writing,
                            PeerFrame::Streamed.tag(),
                            &round.encode(),
                        ))
                        .await??;
                        for (ask, answer) in next.asks.iter_mut().zip(&round.answers) {
                            if let Some((at, _)) = answer.records.last() {
                                ask.from = Sequence::new(at.get().saturating_add(1));
                            }
                        }
                        unacknowledged.push_back(next.asks.iter().map(|ask| ask.from).collect());
                        continue;
                    }
                    Ok(_) => {}
                    Err(Error::Uncollectable { from }) => {
                        let mut refused = Vec::with_capacity(8);
                        crate::frame::put_u64(&mut refused, from);
                        bounded(frame_async::write_tagged(
                            &mut writing,
                            PeerFrame::Uncollectable.tag(),
                            &refused,
                        ))
                        .await??;
                        return Ok(());
                    }
                    Err(Error::Unsubscribed) => {
                        bounded(frame_async::write_tagged(
                            &mut writing,
                            PeerFrame::Unsubscribed.tag(),
                            &[],
                        ))
                        .await??;
                        return Ok(());
                    }
                    Err(why) => return Err(why),
                }
            }
            tokio::select! {
                biased;
                () = self.stop.cancelled() => return Ok(()),
                (reading, read) = frames.as_mut() => {
                    frames.set(next_frame(reading));
                    match read {
                        Ok(Some((tag, body))) if tag == PeerFrame::Held.tag() => {
                            let holds = StreamAsk::decode(&body)?;
                            self.held_by(follower, &holds).await?;
                            // A round is acknowledged once every log is held
                            // past where it left it.
                            while unacknowledged.front().is_some_and(|left| {
                                left.len() == holds.asks.len()
                                    && left.iter().zip(&holds.asks).all(|(left, held)| *left <= held.from)
                            }) {
                                unacknowledged.pop_front();
                            }
                        }
                        // A restart (ADR-0120 D4): held, and the new place to
                        // send from; what was in flight is the follower's to
                        // throw away, up to the mark that says so.
                        Ok(Some((tag, body))) if tag == PeerFrame::StreamFrom.tag() => {
                            next = StreamAsk::decode(&body)?;
                            self.held_by(follower, &next).await?;
                            unacknowledged.clear();
                            bounded(frame_async::write_tagged(
                                &mut writing,
                                PeerFrame::Restarted.tag(),
                                &[],
                            ))
                            .await??;
                        }
                        Ok(Some((tag, _))) => return Err(Error::OutOfTurn { tag }),
                        Ok(None) => return Ok(()),
                        Err(Error::Io(why)) if why.kind() == std::io::ErrorKind::UnexpectedEof => {
                            return Ok(());
                        }
                        Err(why) => return Err(why),
                    }
                }
                changed = commits.changed(), if unacknowledged.len() < UNACKNOWLEDGED => {
                    if changed.is_err() {
                        // The commit signal went with the node.
                        return Ok(());
                    }
                }
                // Only while everything sent is acknowledged: the beat says
                // nothing after what the follower holds has landed here.
                () = tokio::time::sleep(heartbeat), if unacknowledged.is_empty() => {
                    if !admitted() {
                        tracing::warn!(
                            "a held stream ended: the peer's certificate is no longer admitted here"
                        );
                        return Ok(());
                    }
                    bounded(frame_async::write_tagged(
                        &mut writing,
                        PeerFrame::Streamed.tag(),
                        &quiet,
                    ))
                    .await??;
                }
            }
        }
    }

    /// Count what `follower` says it holds durably (ADR-0120 D2).
    async fn held_by(&self, follower: [u8; NODE_ID_LEN], holds: &StreamAsk) -> Result<()> {
        let holding = Arc::clone(&self.holding);
        let holds = holds.clone();
        self.store(move || stream_held(&*holding, follower, &holds))
            .await
    }
}
