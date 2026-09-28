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

mod feeding;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tessari_constants::GREETING_SECONDS;
use tessari_serve::{Admitted, Bridge, Bridged, Busy, Stopping};
use tessari_session::Detached;
use tessaridb::Db;
use tessaridb::feed::Commits;
use tokio::io::{BufReader, BufWriter as AsyncBufWriter};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;

use crate::error::{Error, Result};
use crate::hot::{self, Cooled};
use crate::message::Request;
use crate::push::Follow;
use crate::{frame, frame_async, message, node, redirect};
use feeding::feed;

/// What a client is told when every store call slot is taken.
pub(crate) const BUSY: &str =
    "this node is running as many statements as it will; try again shortly";

/// What one conversation shares with the node that accepted it.
#[derive(Clone)]
pub(crate) struct Conversation {
    /// Names this connection across every line it produces.
    pub(crate) id: u64,
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
}

/// Hold one connection until it ends.
///
/// `place` and `busy` are held for the connection's whole life and released on
/// drop, panic included — a feed hands them to its own thread's lifetime by
/// waiting for it here.
pub(crate) async fn converse(
    talk: Conversation,
    mut busy: Busy,
    place: Admitted,
    session: Detached,
    stream: TcpStream,
) -> Result<()> {
    let (read_half, write_half) = stream.into_split();
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
            log::info!("connection {} became a subscription", talk.id);
            let asked = Follow::decode(&body)?;
            let fed = feed(talk, session, reader, writer, asked).await;
            drop((busy, place));
            return fed;
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
        if close_behind && let Ok(held) = Arc::clone(&talk.hot).try_acquire_owned() {
            let stream = reader
                .into_inner()
                .reunite(writer.into_inner())
                .map_err(|_| Error::Io(std::io::Error::other("a connection's halves parted")))?
                .into_std()?;
            stream.set_nonblocking(false)?;
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
            stream.set_nonblocking(true)?;
            let (read_half, write_half) = TcpStream::from_std(stream)?.into_split();
            reader = BufReader::new(read_half);
            writer = AsyncBufWriter::new(write_half);
            continue;
        }
        let db = Arc::clone(&talk.db);
        let id = talk.id;
        let bridged = talk
            .bridge
            .call(session, move |held: Detached| {
                let mut attached = held.attach(db.store());
                let answer = respond(id, &db, &mut attached, &request, theirs);
                (attached.detach(), answer)
            })
            .await;
        match bridged {
            Bridged::Answered((back, answer)) => {
                session = back;
                reply(&mut writer, &talk.stopping, answer.kind, &answer.body).await?;
                answered_at = Some(Instant::now());
            }
            Bridged::Busy(back) => {
                session = back;
                log::warn!("connection {id} refused a statement: every store call slot is taken");
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

/// Answer one request: sign in if it carries credentials, then run it.
///
/// Runs on the blocking pool — a password hash, a statement and a forwarded
/// write all block.
pub(crate) fn respond(
    id: u64,
    db: &Db,
    session: &mut tessaridb::Session<'_>,
    request: &Request,
    theirs: u8,
) -> Answer {
    let refusal = |message: String| Answer {
        kind: frame::Kind::Refusal,
        body: message.into_bytes(),
    };
    if let Some((name, password)) = &request.credentials
        && let Err(refused) = session.sign_in(name, password)
    {
        // The session's own refusal, travelling as one. A second rule here
        // would be a second place for "who may do this" to be decided.
        log::warn!("connection {id} refused: {refused}");
        return refusal(refused.to_string());
    }
    let ran = session.run_with(&request.script, &request.parameters);
    // A write that arrived at a node which may not take it is routed, not
    // refused (ADR-0019 §2, case *forward*). Matched on the **variant**, not on
    // the message text: a routing decision taken by string comparison changes
    // meaning the day somebody rewords an error.
    if matches!(ran, Err(tessaridb::Error::NotWritable { .. })) {
        return match node::forward(db, request) {
            Ok((kind, body)) => Answer { kind, body },
            // The hop failed, and the client is told that rather than being
            // told the statement was wrong. It was not.
            Err(why) => refusal(why.to_string()),
        };
    }
    // A redirect is an **instruction** and leaves as its own frame rather than
    // as a refusal carrying a hint (`redirect.rs`), gated on what the client
    // said at the greeting: a client built before tag 13 cannot name the frame,
    // and the refusal it has always received is the better answer for it.
    if let Err(tessaridb::Error::ReadIsElsewhere {
        endpoint,
        node,
        epoch,
        ..
    }) = &ran
        && theirs >= frame::REDIRECTS
    {
        let sent = redirect::Elsewhere {
            endpoint: endpoint.clone(),
            node: *node,
            epoch: *epoch,
            // This read, and not this arrangement: the bound that sent the
            // client away is a bound on *currency*, and this node's own copy may
            // satisfy the same bound at the next request.
            settlement: redirect::Settlement::Transient,
        };
        return Answer {
            kind: frame::Kind::Elsewhere,
            body: sent.encode(),
        };
    }
    match ran {
        Ok(outcomes) => {
            let mut answer = Vec::new();
            frame::put_u32(
                &mut answer,
                u32::try_from(outcomes.len()).unwrap_or(u32::MAX),
            );
            for outcome in &outcomes {
                // Resolved here because the catalog is here. `names_in` walks
                // the answer first and touches nothing when it holds no
                // reference, which is most answers.
                let names = message::names_for(db, outcome);
                answer.extend_from_slice(&message::encode_outcome(outcome, &names));
            }
            Answer {
                kind: frame::Kind::Answer,
                body: answer,
            }
        }
        // A refusal does not close the connection: a client that mistyped a
        // statement has not stopped being a client.
        Err(refused) => refusal(refused.to_string()),
    }
}
