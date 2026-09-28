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

use std::io::BufWriter;
use std::sync::Arc;
use std::time::Duration;

use tessari_constants::GREETING_SECONDS;
use tessari_serve::{Admitted, Bridge, Bridged, Busy, Stopping};
use tessari_session::Detached;
use tessaridb::Db;
use tessaridb::feed::Commits;
use tokio::io::{BufReader, BufWriter as AsyncBufWriter};
use tokio::net::TcpStream;

use crate::error::{Error, Result};
use crate::message::Request;
use crate::push::Follow;
use crate::{frame, frame_async, message, node, redirect};

/// What a client is told when every store call slot is taken.
pub(crate) const BUSY: &str =
    "this node is running as many statements as it will; try again shortly";

/// What one conversation shares with the node that accepted it.
pub(crate) struct Conversation {
    /// Names this connection across every line it produces.
    pub(crate) id: u64,
    pub(crate) db: Arc<Db>,
    pub(crate) committed: Arc<Commits>,
    pub(crate) stopping: Arc<Stopping>,
    pub(crate) bridge: Arc<Bridge>,
}

/// One answer, decided on the blocking pool and written from the task.
struct Answer {
    kind: frame::Kind,
    body: Vec<u8>,
    /// Whether a pusher should look at the log: a statement ran to an answer.
    signal: bool,
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
    while let Some((kind, body)) = frame_async::read(&mut reader).await? {
        if kind == frame::Kind::Subscribe {
            // The connection stops being a conversation and becomes a feed; see
            // `push.rs` for why one connection does one job. Moved to the feed
            // count so a shutdown's drain does not wait for it.
            busy.became_a_feed();
            log::info!("connection {} became a subscription", talk.id);
            let asked = Follow::decode(&body)?;
            let stream = reader
                .into_inner()
                .reunite(writer.into_inner())
                .map_err(|_| Error::Io(std::io::ErrorKind::InvalidData.into()))?
                .into_std()?;
            stream.set_nonblocking(false)?;
            let fed = feed(talk, session, stream, asked).await;
            drop((busy, place));
            return fed;
        }
        if kind != frame::Kind::Request {
            // A client sending an answer is a client this build does not
            // understand, and continuing would be guessing at what it meant.
            return Err(Error::UnknownFrame { tag: kind.tag() });
        }
        let request = Request::decode(&body)?;
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
                if answer.signal {
                    // Every commit against this store arrives through some
                    // connection of this node, so this is where a pusher learns
                    // there is something to look at.
                    talk.committed.signal();
                }
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
fn respond(
    id: u64,
    db: &Db,
    session: &mut tessaridb::Session<'_>,
    request: &Request,
    theirs: u8,
) -> Answer {
    let refusal = |message: String| Answer {
        kind: frame::Kind::Refusal,
        body: message.into_bytes(),
        signal: false,
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
            Ok((kind, body)) => Answer {
                kind,
                body,
                signal: false,
            },
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
            signal: false,
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
            // A statement that wrote nothing signals too: a pusher woken for
            // nothing polls, finds nothing and waits again, which is cheaper
            // than reading the tail here to find out.
            Answer {
                kind: frame::Kind::Answer,
                body: answer,
                signal: true,
            }
        }
        // A refusal does not close the connection: a client that mistyped a
        // statement has not stopped being a client.
        Err(refused) => refusal(refused.to_string()),
    }
}

/// Push changes down this connection on a thread of its own, until it ends.
///
/// A feed still holds a thread: the async feed — waiting on the commit signal,
/// the socket and the stop token at once — is the next step (ADR-0085 §4). It
/// runs on a dedicated thread rather than the blocking pool, because a feed
/// lives as long as its subscriber and would otherwise hold a slot every
/// statement on the node competes for. The task waits for it, so the
/// connection's place at the door is held exactly as long as the feed is.
async fn feed(
    talk: Conversation,
    session: Detached,
    stream: std::net::TcpStream,
    asked: Follow,
) -> Result<()> {
    let (done, finished) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name(format!("tessaridb-feed-{}", talk.id))
        .spawn(move || {
            let mut attached = session.attach(talk.db.store());
            let mut writer = BufWriter::new(stream);
            let fed = node::follow(
                &talk.db,
                &talk.committed,
                &talk.stopping,
                &mut attached,
                &mut writer,
                &asked,
            );
            drop(done.send(fed));
        });
    spawned?;
    // A feed thread that panicked drops its sender, which reads as an ended
    // connection — the same outcome a panicking connection thread always had.
    finished.await.unwrap_or_else(|_| {
        Err(Error::Io(std::io::Error::other(
            "the feed's thread ended without an answer",
        )))
    })
}
