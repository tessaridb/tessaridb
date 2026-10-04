//! One connection, from hello to hang-up, as a task rather than a thread.
//!
//! # One session, not one per statement
//!
//! The session is opened once and lives as long as the connection, because that
//! is what a connection *is*: `USE NAMESPACE prod;` selects something, and a
//! selection that does not survive to the next statement is not a selection.
//!
//! The session borrows the store and a task cannot hold a borrow across the hop
//! each statement takes to the blocking pool, so between statements it is held
//! [`Detached`] and each statement gives it its store back for the length of the
//! call. What the connection established — who signed in, what was selected —
//! is carried whole, never re-derived.
//!
//! # Every store call crosses the bridge
//!
//! A statement, a sign-in (a password hash, tens of milliseconds by design) and a
//! forwarded write all block, so all of them run through the node's [`Bridge`].
//! When every slot is taken the statement is refused at once with a refusal the
//! client can read, and the session comes back untouched for the next one.
//!
//! # A busy connection is served on a thread
//!
//! A statement that arrives close behind its connection's last answer takes the
//! whole connection to a store thread instead of hopping there and back, and the
//! thread keeps it until the client falls quiet (`hot.rs`). The slots and the
//! refusal are the same on both paths.

mod carried;
mod feeding;
mod responding;
#[cfg(test)]
mod tests;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tessari_constants::GREETING_SECONDS;
use tessari_serve::{Admitted, Bridge, Bridged, Busy, Stopping};
use tessari_session::Detached;
use tessaridb::Db;
use tessaridb::feed::Commits;
use tokio::io::{BufReader, BufWriter as AsyncBufWriter};
use tokio::sync::Semaphore;

use crate::error::{Error, Result};
use crate::hot::{self, Cooled};
use crate::message::Request;
use crate::push::Follow;
use crate::{frame, frame_async, message, redirect};
pub(crate) use carried::Carried;
use feeding::feed;
pub use responding::render_coordinated;
pub(crate) use responding::{respond, respond_vault};

/// What a client is told when every store call slot is taken.
pub(crate) const BUSY: &str =
    "this node is running as many statements as it will; try again shortly";

/// What one conversation shares with the node that accepted it.
#[derive(Clone)]
pub(crate) struct Conversation {
    pub(crate) db: Arc<Db>,
    pub(crate) committed: Arc<Commits>,
    pub(crate) stopping: Arc<Stopping>,
    pub(crate) bridge: Arc<Bridge>,
    /// The bound on feed rounds, apart from statements — see `Node::rounds`.
    pub(crate) rounds: Arc<Bridge>,
    /// How many connections may be served on a store thread at once (`hot.rs`).
    pub(crate) hot: Arc<Semaphore>,
}

/// One answer, decided on the blocking pool and written from the task.
pub(crate) struct Answer {
    pub(crate) kind: frame::Kind,
    pub(crate) body: Vec<u8>,
    /// Whether this answer sends the caller elsewhere, and if so whether the
    /// redirect is settled — kept beside the frame so the surface counts it
    /// without reading back what it just encoded (G053 C6).
    pub(crate) redirect: Option<bool>,
}

/// Hold one connection until it ends.
///
/// `place` and `busy` are held for the connection's whole life and released on
/// drop, panic included — a feed hands them to its own thread's lifetime by
/// waiting for it here.
pub(crate) async fn converse<C: Carried>(
    talk: Conversation,
    mut busy: Busy,
    place: Admitted,
    session: Detached,
    stream: C,
) -> Result<()> {
    let (read_half, write_half) = stream.halves();
    let mut reader = BufReader::new(read_half);
    let mut writer = AsyncBufWriter::new(write_half);
    // A client that connects and sends nothing would otherwise hold its place at
    // the door for the life of the process. The deadline covers the greeting
    // only — see `GREETING_SECONDS` for why an idle prompt after it is not cut.
    let theirs = tokio::time::timeout(
        Duration::from_secs(GREETING_SECONDS),
        frame_async::greet(&mut reader, &mut writer),
    )
    .await
    .map_err(|_| Error::Io(std::io::ErrorKind::TimedOut.into()))??;

    let mut session = session;
    // When this connection last had an answer written from the task, for
    // `hot.rs`'s rule: a statement arriving close behind it goes to a thread.
    let mut answered_at: Option<Instant> = None;
    // A frame a busy spell read and handed back rather than handling itself.
    let mut handed_back: Option<(frame::Kind, Vec<u8>)> = None;
    loop {
        let (kind, body) = match handed_back.take() {
            Some(frame) => frame,
            None => match frame_async::read(&mut reader).await? {
                Some(frame) => frame,
                None => break,
            },
        };
        if kind == frame::Kind::Subscribe {
            // The connection stops being a conversation and becomes a feed; see
            // `push.rs` for why one connection does one job. Moved to the feed
            // count so a shutdown's drain does not wait for it.
            busy.became_a_feed();
            tracing::info!("connection became a subscription");
            let asked = Follow::decode(&body)?;
            let fed = feed(talk, session, reader, writer, asked).await;
            drop((busy, place));
            return fed;
        }
        if kind == frame::Kind::Vault {
            // Always on the bridge, never on the task: an unseal is an Argon2id
            // derivation, and a derivation on the runtime stalls every other
            // connection it shares a worker with.
            let asked = crate::VaultAsk::decode(&body)?;
            let db = Arc::clone(&talk.db);
            let bridged = talk
                .bridge
                .call(session, move |held: Detached| {
                    let mut attached = held.attach(db.store());
                    let answer = respond_vault(&mut attached, &asked);
                    (attached.detach(), answer)
                })
                .await;
            match bridged {
                Bridged::Answered((back, answer)) => {
                    session = back;
                    reply(&mut writer, &talk.stopping, answer.kind, &answer.body).await?;
                }
                Bridged::Busy(back) => {
                    session = back;
                    reply(
                        &mut writer,
                        &talk.stopping,
                        frame::Kind::Refusal,
                        BUSY.as_bytes(),
                    )
                    .await?;
                }
                Bridged::Panicked => {
                    return Err(Error::Io(std::io::Error::other(
                        "the vault call panicked, and the session it held went with it",
                    )));
                }
            }
            continue;
        }
        if kind != frame::Kind::Request {
            // A client sending an answer is a client this build does not
            // understand, and continuing would be guessing at what it meant.
            return Err(Error::UnknownFrame { tag: kind.tag() });
        }
        let request = Request::decode(&body)?;
        // Nothing read ahead, because the thread reads from the socket itself.
        let close_behind =
            answered_at.is_some_and(|at| at.elapsed() < hot::QUIET) && reader.buffer().is_empty();
        if C::THREADED
            && close_behind
            && let Ok(held) = Arc::clone(&talk.hot).try_acquire_owned()
        {
            let stream = C::to_thread(reader.into_inner(), writer.into_inner())?;
            let spell = talk.clone();
            let cooled = tokio::task::spawn_blocking(move || {
                let cooled = hot::serve(&spell, session, stream, request, theirs);
                drop(held);
                cooled
            })
            .await
            // As on the bridge: the session went down with the statement.
            .map_err(|_| {
                Error::Io(std::io::Error::other(
                    "the statement panicked, and the session it held went with it",
                ))
            })??;
            let (back, stream) = match cooled {
                Cooled::Closed => return Ok(()),
                Cooled::Quiet(back, stream) => (back, stream),
                Cooled::Frame(back, stream, kind, body) => {
                    handed_back = Some((kind, body));
                    (back, stream)
                }
            };
            session = back;
            answered_at = None;
            let (read_half, write_half) = C::from_thread(stream)?;
            reader = BufReader::new(read_half);
            writer = AsyncBufWriter::new(write_half);
            continue;
        }
        let db = Arc::clone(&talk.db);
        let bridged = talk
            .bridge
            .call(session, move |held: Detached| {
                let mut attached = held.attach(db.store());
                let answer = respond(&db, &mut attached, &request, theirs);
                (attached.detach(), answer)
            })
            .await;
        match bridged {
            Bridged::Answered((back, answer)) => {
                session = back;
                reply(&mut writer, &talk.stopping, answer.kind, &answer.body).await?;
                if let Some(settled) = answer.redirect {
                    talk.stopping.redirected(settled);
                }
                answered_at = Some(Instant::now());
            }
            Bridged::Busy(back) => {
                session = back;
                tracing::warn!("statement refused: every store call slot is taken");
                reply(
                    &mut writer,
                    &talk.stopping,
                    frame::Kind::Refusal,
                    BUSY.as_bytes(),
                )
                .await?;
            }
            // The session went down with the statement that panicked, so there
            // is no conversation left to continue.
            Bridged::Panicked => {
                return Err(Error::Io(std::io::Error::other(
                    "the statement panicked, and the session it held went with it",
                )));
            }
        }
    }
    Ok(())
}

/// Write one answer to a request, and count it.
///
/// This surface has **three** places that answer a request — an answer, a
/// refused sign-in and a refused statement — so the mapping onto the one word
/// *refusal* lives here rather than at each of them. Only replies to requests
/// pass through; the greeting and a pushed change are not answers to anything.
async fn reply(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    counting: &Stopping,
    kind: frame::Kind,
    body: &[u8],
) -> Result<()> {
    counting.answered(kind == frame::Kind::Refusal);
    frame_async::write(writer, kind, body).await
}

/// The redirect a refusal stands for, if it stands for one.
///
/// Four refusals mean *go there* and they differ in what they promise. A read
/// beyond a bound is about **this read** — the node's own copy may satisfy the
/// same bound at the next request — so it is `Transient`, and so is a read a
/// node holding part of a split table could not gather, sent to a peer holding
/// the whole of it (G051 C4): the same table read outside a transaction is one
/// this node gathers itself. So is a gathered read whose leader holds a
/// different map of the table (G051 SG3): the maps come back into agreement on
/// their own, and the whole holder answers meanwhile. A write into a range
/// another node leads names a **leadership**, which holds until its epoch is
/// superseded, so it is `Settled` and a client may remember it per range.
///
/// The address is the one a **client** reaches the node at when its member row
/// says so (`CLIENTS AT`, ADR-0101); otherwise the address the refusal carried,
/// which is what every redirect named before that clause existed.
fn redirected(
    db: &Db,
    ran: &tessaridb::Result<Vec<tessaridb::Outcome>>,
) -> Option<redirect::Elsewhere> {
    let (endpoint, node, epoch, settlement) = match ran {
        Err(tessaridb::Error::ReadIsElsewhere {
            endpoint,
            node,
            epoch,
            ..
        }) => (endpoint, node, epoch, redirect::Settlement::Transient),
        Err(
            tessaridb::Error::NotHeldHere {
                holder: Some(holder),
                ..
            }
            | tessaridb::Error::ShardMapMoved {
                holder: Some(holder),
                ..
            },
        ) => (
            &holder.endpoint,
            &holder.node,
            &holder.epoch,
            redirect::Settlement::Transient,
        ),
        Err(tessaridb::Error::Store(tessari_storage::Error::WriteIsElsewhere {
            endpoint,
            node,
            epoch,
        })) => (endpoint, node, epoch, redirect::Settlement::Settled),
        _ => return None,
    };
    // A catalog that cannot be read leaves the address the refusal carried: the
    // redirect is still right about who leads, and a refusal would be worse.
    let clients = db.member(node).ok().flatten().and_then(|row| row.clients);
    Some(redirect::Elsewhere {
        endpoint: clients.unwrap_or_else(|| endpoint.clone()),
        node: *node,
        epoch: *epoch,
        settlement,
    })
}
