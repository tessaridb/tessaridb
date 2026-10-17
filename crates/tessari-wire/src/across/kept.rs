//! Peer links kept open between the records of transactions across leaders
//! (ADR-0112 D13j).
//!
//! A record used to open its own connection: a TCP connect, a mutual TLS
//! handshake and an exchange of greetings before the record itself — measured
//! at about two milliseconds of a seven-millisecond commit on a three-node
//! Linux cluster, on the caller's path. A link both ends greeted at
//! [`KEPT_FROM`] or later is kept instead: the next record to that peer is
//! written straight onto it, under its own signed assertion, and the door
//! asks again whether the certificate the link proved is still admitted.
//!
//! # What may be retried
//!
//! A record that could not be WRITTEN onto a kept link never left this node,
//! so a fresh link carries it. One that was written and then lost its answer
//! may have been acted on; that is the link refusal it always was, and the
//! coordinator's rules for a part not reached apply.

use std::time::{Duration, Instant};

use dashmap::DashMap;
use tessari_constants::{ACROSS_KEPT_IDLE_SECONDS, ACROSS_KEPT_PER_PEER};
use tessari_encoding::{NODE_ID_LEN, NodeVersion};
use tessari_session::{AcrossAnswer, PartRefused};

use super::Carried;
use crate::error::{Error, Result};
use crate::frame_async;
use crate::keys::PeerKeys;
use crate::link::{Credential, Link, framed, hear, open_within, say};
use crate::peer::{Hello, PeerFrame};

/// The first build whose door keeps a link for the next record, and whose
/// coordinator writes one onto a kept link.
pub(crate) const KEPT_FROM: NodeVersion = NodeVersion {
    major: 0,
    minor: 25,
    patch: 0,
};

/// What the leader answered: its answer, or its refusal in its own words.
pub(crate) type Reply = std::result::Result<AcrossAnswer, PartRefused>;

/// An authenticated link to one peer, open for the next record.
pub(crate) struct Kept {
    link: Link,
    since: Instant,
    /// How long any one step on this link may take, as when it was opened.
    bound: Duration,
}

impl core::fmt::Debug for Kept {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Kept")
            .field("since", &self.since)
            .finish_non_exhaustive()
    }
}

impl Kept {
    /// Whether the door is still keeping this link — it was used recently
    /// enough that the door's own, longer idle limit has not closed it.
    fn fresh(&self) -> bool {
        self.since.elapsed() < Duration::from_secs(ACROSS_KEPT_IDLE_SECONDS)
    }
}

/// Why a record on a kept link has no answer.
#[derive(Debug)]
pub(crate) enum KeptFailed {
    /// It was never written: a fresh link may carry it.
    Unsent(Error),
    /// It was written and its answer was lost: it may have been acted on.
    Sent(Error),
}

/// Open a link to `at`, greet, and write `carried` — keeping the link when both
/// ends greeted at [`KEPT_FROM`] or later and the answer arrived whole.
///
/// # Errors
///
/// A link that could not be opened, greeted or read, and a frame the leader
/// had no business sending.
pub(crate) async fn across_keeping(
    address: &str,
    (keys, mine): (&PeerKeys, Credential),
    at: [u8; NODE_ID_LEN],
    said: &Hello,
    (carried, bound): (&Carried, Duration),
) -> Result<(Reply, Option<Kept>)> {
    let mut link = open_within(address, (keys, mine), at, bound).await?;
    framed(bound, say(&mut link, said)).await?;
    let heard = framed(bound, hear(&mut link)).await?;
    let reply = ask(&mut link, carried, bound).await?;
    let kept = (said.build >= KEPT_FROM && heard.build >= KEPT_FROM).then(|| Kept {
        link,
        since: Instant::now(),
        bound,
    });
    Ok((reply, kept))
}

/// Write `carried` onto a kept link and read the answer.
///
/// # Errors
///
/// [`KeptFailed::Unsent`] when the record could not be written,
/// [`KeptFailed::Sent`] when its answer did not arrive.
pub(crate) async fn across_on(
    kept: &mut Kept,
    carried: &Carried,
) -> std::result::Result<Reply, KeptFailed> {
    let bound = kept.bound;
    let body = carried.encode();
    framed(
        bound,
        frame_async::write_tagged(&mut kept.link, PeerFrame::Across.tag(), &body),
    )
    .await
    .map_err(KeptFailed::Unsent)?;
    let reply = framed(bound, answer(&mut kept.link))
        .await
        .map_err(KeptFailed::Sent)?;
    kept.since = Instant::now();
    Ok(reply)
}

/// Write one record and read its answer on a link already greeted.
async fn ask(link: &mut Link, carried: &Carried, bound: Duration) -> Result<Reply> {
    let body = carried.encode();
    framed(
        bound,
        frame_async::write_tagged(link, PeerFrame::Across.tag(), &body),
    )
    .await?;
    framed(bound, answer(link)).await
}

/// The leader's answer to one record, or its refusal.
async fn answer(link: &mut Link) -> Result<Reply> {
    let (tag, body) = frame_async::read_tagged(link)
        .await?
        .ok_or(Error::Truncated)?;
    match PeerFrame::from_tag(tag) {
        Some(PeerFrame::AcrossDone) => Ok(Ok(
            AcrossAnswer::decode(&body).map_err(|_| Error::Malformed)?
        )),
        Some(PeerFrame::NotAcross) => Ok(Err(PartRefused::decode(&body).ok_or(Error::Malformed)?)),
        Some(_) => Err(Error::OutOfTurn { tag }),
        None => Err(Error::UnknownFrame { tag }),
    }
}

/// The idle kept links of every peer, a few each.
#[derive(Debug, Default)]
pub(crate) struct KeptLinks {
    idle: DashMap<[u8; NODE_ID_LEN], Vec<Kept>>,
}

impl KeptLinks {
    /// A link to `to` still fresh enough to use, if one is idle; stale ones
    /// are closed on the way.
    pub(crate) fn take(&self, to: [u8; NODE_ID_LEN]) -> Option<Kept> {
        let mut links = self.idle.get_mut(&to)?;
        while let Some(link) = links.pop() {
            if link.fresh() {
                return Some(link);
            }
        }
        None
    }

    /// Keep `link` for the next record to `to`, unless enough are idle.
    pub(crate) fn keep(&self, to: [u8; NODE_ID_LEN], link: Kept) {
        let mut links = self.idle.entry(to).or_default();
        links.retain(Kept::fresh);
        if links.len() < ACROSS_KEPT_PER_PEER {
            links.push(link);
        }
    }
}
