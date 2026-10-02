//! A follower held open to be SENT what its leader commits (ADR-0106 D5).
//!
//! # Why the leader sends rather than the follower asking on a clock
//!
//! A collection round ran every ten seconds, so a follower was up to ten seconds
//! behind on a healthy link and a leader killed in that window took every write
//! it had acknowledged with it — measured: 262 of 262 (G053 SG1). No database
//! the design was compared with waits a period before moving a change: a
//! write-ahead-log sender and a binary-log dump thread push as they write, a
//! Raft leader sends `AppendEntries` per commit.
//!
//! # The shape: a round, repeated, where the leader waits instead of answering empty
//!
//! The follower opens ONE connection and sends [`StreamAsk`] — every log it
//! follows and the first position it does not hold in each, exactly the asks a
//! collection round makes. The leader answers the way a round is answered, with
//! one [`Collected`] per log, except that when it has nothing to send it waits
//! for its next commit instead of answering empty, and meanwhile sends a
//! heartbeat every [`tessari_constants::STREAM_HEARTBEAT_MILLIS`] — an empty
//! round meaning *nothing after your positions has landed here*. The follower applies the round with the round's own code, in
//! its writer's commit order, and sends its next ask — whose positions are
//! therefore the ones it has made durable.
//!
//! That gives three properties without a mechanism of their own:
//!
//! - **Flow control.** One round is in flight at a time; a slow follower is
//!   simply asked for less often, and what it has not taken stays in the log.
//!   Nothing is queued in the leader's memory, for the reason the client feed
//!   gives (`push.rs`): a buffer in front of the log is a buffer that loses.
//! - **Acknowledgement.** The positions in each ask are what the follower holds,
//!   which is what a write waiting for copies needs to know (ADR-0106 D1).
//! - **One apply path.** The stream does not apply anything the round would not:
//!   the leader reads through [`Origin::collected`], grant and all, and the
//!   follower applies through `Collector::apply`. A second path that decided
//!   what a follower may receive would be a second authority over the grant.
//!
//! A refusal ends the stream — the follower's round then meets it again on its
//! own terms, which is where re-seeding and every other repair already live.

use std::net::{SocketAddr, TcpStream};

use rustls::ClientConnection;
use tessari_encoding::NODE_ID_LEN;

use super::{Collect, Collected, Origin};
use crate::error::{Error, Result};
use crate::frame;
use crate::keys::PeerKeys;
use crate::link::{hear, open, say};
use crate::peer::{Hello, PeerFrame};

/// What a follower asks for on a held stream: every log, from where it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamAsk {
    /// One collection ask per log, in the order the follower applies them.
    pub asks: Vec<Collect>,
}

impl StreamAsk {
    /// The body of a [`PeerFrame::Stream`] frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        frame::put_u64(
            &mut body,
            u64::try_from(self.asks.len()).unwrap_or(u64::MAX),
        );
        for ask in &self.asks {
            frame::put_bytes(&mut body, &ask.encode());
        }
        body
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] when the body is not the shape an ask takes,
    /// or names more than [`tessari_constants::STREAM_LOGS_MAX`] logs — a bound on
    /// the work one peer can have a leader repeat on every commit.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let (count, mut at) = frame::take_u64(body, 0)?;
        if count > tessari_constants::STREAM_LOGS_MAX {
            return Err(Error::Malformed);
        }
        // Grown as asks arrive rather than sized by the count, which came from
        // the other end (see `Collected::decode`).
        let mut asks = Vec::new();
        for _ in 0..count {
            let (bytes, next) = frame::take_bytes(body, at)?;
            at = next;
            asks.push(Collect::decode(&bytes)?);
        }
        Ok(Self { asks })
    }
}

/// The leader's round on a held stream: one answer per log asked, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Streamed {
    /// What each asked log held after the follower's position.
    pub answers: Vec<Collected>,
}

impl Streamed {
    /// The body of a [`PeerFrame::Streamed`] frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        frame::put_u64(
            &mut body,
            u64::try_from(self.answers.len()).unwrap_or(u64::MAX),
        );
        for answer in &self.answers {
            frame::put_bytes(&mut body, &answer.encode());
        }
        body
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] when the body is not the shape a round takes,
    /// and the encoding's own failure when a record cannot be decoded.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let (count, mut at) = frame::take_u64(body, 0)?;
        let mut answers = Vec::new();
        for _ in 0..count {
            let (bytes, next) = frame::take_bytes(body, at)?;
            at = next;
            answers.push(Collected::decode(&bytes)?);
        }
        Ok(Self { answers })
    }

    /// Whether this round carries no record at all — the case a leader waits
    /// out rather than sending.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        self.answers.iter().all(|answer| answer.records.is_empty())
    }

    /// Whether this is a heartbeat: no answer at all, which a leader sends only
    /// while nothing after the follower's positions has landed on it.
    #[must_use]
    pub fn is_heartbeat(&self) -> bool {
        self.answers.is_empty()
    }
}

/// Answer one stream ask out of `origin`, for the follower the handshake proved.
///
/// Every log through [`Origin::collected`], so the grant and the follower's
/// recorded position are exactly what a collection round would leave.
///
/// # Errors
///
/// The first log's refusal, unchanged — the caller ends the stream on it.
pub(crate) fn answer(
    origin: &dyn Origin,
    follower: [u8; NODE_ID_LEN],
    asked: &StreamAsk,
) -> Result<Streamed> {
    let answers = asked
        .asks
        .iter()
        .map(|ask| origin.collected(follower, *ask))
        .collect::<Result<Vec<_>>>()?;
    Ok(Streamed { answers })
}

/// A follower's held stream to one leader.
///
/// Synchronous, like every dial on the peer link (`link.rs`): it is driven from
/// a thread of its own, because a round blocks on the leader's next commit and
/// a runtime worker must never be the thing that blocks.
pub struct Following {
    session: ClientConnection,
    socket: TcpStream,
    /// What the handshake judged the leader by, asked again for every frame:
    /// a leader whose certificate is revoked while it streams is followed no
    /// further (ADR-0108 D6).
    keys: PeerKeys,
}

impl std::fmt::Debug for Following {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Following").finish_non_exhaustive()
    }
}

impl Following {
    /// Reach `peer`, exchange greetings, and hold the connection, reading
    /// under `silence` — the longest the leader may say nothing before the
    /// stream counts as lost (several heartbeats).
    ///
    /// # Errors
    ///
    /// Whatever the handshake refuses with, exactly as a collection round's dial.
    pub fn open(
        peer: ([u8; NODE_ID_LEN], SocketAddr),
        keys: &PeerKeys,
        said: &Hello,
        silence: std::time::Duration,
    ) -> Result<Self> {
        let (mut session, mut socket) = open(peer.1, keys, peer.0)?;
        {
            let mut link = rustls::Stream::new(&mut session, &mut socket);
            say(&mut link, said)?;
            hear(&mut link)?;
        }
        // After the greeting, which keeps the greeting's own deadline.
        socket.set_read_timeout(Some(silence))?;
        Ok(Self {
            session,
            socket,
            keys: keys.clone(),
        })
    }

    /// Ask from where this node stands. The leader answers with any number of
    /// heartbeats and then exactly one round carrying records, read by
    /// [`Following::heard`].
    ///
    /// # Errors
    ///
    /// The transport's failure.
    pub fn ask(&mut self, asked: &StreamAsk) -> Result<()> {
        let mut link = rustls::Stream::new(&mut self.session, &mut self.socket);
        frame::write_tagged(&mut link, PeerFrame::Stream.tag(), &asked.encode())
    }

    /// The leader's next frame: a heartbeat ([`Streamed::is_heartbeat`]) or the
    /// round answering the last ask.
    ///
    /// Blocks until the leader sends; the read deadline set at
    /// [`Following::open`] is what turns a leader that went silent into an
    /// error rather than a wait.
    ///
    /// # Errors
    ///
    /// [`Error::Uncollectable`] or [`Error::Unsubscribed`] as the leader sent
    /// them, and the transport's failure otherwise.
    pub fn heard(&mut self) -> Result<Streamed> {
        let admitted = self
            .session
            .peer_certificates()
            .and_then(<[_]>::first)
            .is_some_and(|presented| self.keys.still_admits(presented));
        if !admitted {
            return Err(Error::Refused {
                message: "the leader's certificate is no longer admitted here".to_owned(),
            });
        }
        let mut link = rustls::Stream::new(&mut self.session, &mut self.socket);
        let (tag, body) = frame::read_tagged(&mut link)?.ok_or(Error::Truncated)?;
        match PeerFrame::from_tag(tag) {
            Some(PeerFrame::Streamed) => Streamed::decode(&body),
            Some(PeerFrame::Uncollectable) => {
                let (from, _) = frame::take_u64(&body, 0)?;
                Err(Error::Uncollectable { from })
            }
            Some(PeerFrame::Unsubscribed) => Err(Error::Unsubscribed),
            Some(_) => Err(Error::OutOfTurn { tag }),
            None => Err(Error::UnknownFrame { tag }),
        }
    }
}

impl Drop for Following {
    fn drop(&mut self) {
        // Said properly for the reason `call` says it: a dropped socket reaches
        // the leader as a fault it did not have.
        self.session.send_close_notify();
        drop(self.session.write_tls(&mut self.socket));
    }
}

#[cfg(test)]
mod tests {
    use super::{StreamAsk, Streamed};
    use crate::collection::{Collect, Collected};
    use tessari_encoding::LogId;
    use tessari_storage::{Reach, Writer};
    use tessari_types::{Epoch, Sequence};

    #[test]
    fn an_ask_and_a_round_cross_the_wire_unchanged() {
        let asked = StreamAsk {
            asks: vec![
                Collect {
                    home: Reach::Store,
                    from: Sequence::new(4),
                    limit: 500,
                },
                Collect {
                    home: Reach::Namespace(tessari_types::NamespaceId::new(2)),
                    from: Sequence::new(1),
                    limit: 500,
                },
            ],
        };
        assert_eq!(
            StreamAsk::decode(&asked.encode()).expect("an ask decodes"),
            asked
        );
        let round = Streamed {
            answers: vec![Collected {
                log: LogId::new(Reach::Store, Writer::new([3; 16])),
                previous: Epoch::new(1),
                records: Vec::new(),
                stopped_early: false,
                over: Some(Reach::Store),
                order: Some(Sequence::new(9)),
                epoch: None,
            }],
        };
        assert_eq!(
            Streamed::decode(&round.encode()).expect("a round decodes"),
            round
        );
        assert!(round.is_quiet());
    }

    #[test]
    fn an_ask_naming_more_logs_than_the_bound_is_refused() {
        let mut body = Vec::new();
        crate::frame::put_u64(&mut body, tessari_constants::STREAM_LOGS_MAX + 1);
        assert!(matches!(
            StreamAsk::decode(&body),
            Err(crate::error::Error::Malformed)
        ));
    }

    #[test]
    fn a_count_larger_than_the_body_is_refused_not_allocated() {
        let mut body = Vec::new();
        crate::frame::put_u64(&mut body, u64::MAX);
        assert!(StreamAsk::decode(&body).is_err());
        assert!(Streamed::decode(&body).is_err());
    }
}
