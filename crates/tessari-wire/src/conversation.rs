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

use std::sync::Arc;
use std::time::Duration;

use tessari_constants::GREETING_SECONDS;
use tessari_serve::{Admitted, Bridge, Bridged, Busy, Stopping};
use tessari_session::Detached;
use tessaridb::feed::{self, Commits, Feed, Following, Round};
use tessaridb::{Db, Sequence};
use tokio::io::{AsyncReadExt, BufReader, BufWriter as AsyncBufWriter};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use crate::error::{Error, Result};
use crate::message::Request;
use crate::push::Follow;
use crate::{READING, frame, frame_async, message, node, push, redirect};

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
    /// The bound on feed rounds, apart from statements — see `Node::rounds`.
    pub(crate) rounds: Arc<Bridge>,
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

/// Push changes down this connection until it ends — as a task, not a thread.
///
/// Opening and every round cross the bridge like a statement, because both read
/// the store. Between rounds the task waits on three things and holds nothing:
/// the commit signal, the socket, and [`feed::PATIENCE_BETWEEN_ROUNDS`] — the
/// last because a commit made through another surface never signals this node.
/// The socket is where a subscriber that hung up on a quiet feed is noticed
/// (F-S1): reading end-of-stream ends the feed, with no write needed to learn it.
async fn feed(
    talk: Conversation,
    session: Detached,
    mut reader: BufReader<OwnedReadHalf>,
    mut writer: AsyncBufWriter<OwnedWriteHalf>,
    asked: Follow,
) -> Result<()> {
    let db = Arc::clone(&talk.db);
    let opened = talk
        .bridge
        .call(session, move |held: Detached| {
            let mut attached = held.attach(db.store());
            let following = Following {
                from: Sequence::new(asked.from),
                table: asked.table.as_deref(),
                cursor: asked.cursor.as_deref(),
            };
            let opened = Feed::open(&db, &mut attached, &following);
            (attached.detach(), opened)
        })
        .await;
    let (mut session, mut following) = match opened {
        Bridged::Answered((back, Ok(opened))) => (back, opened),
        // The store's own words, travelling as a refusal: every one of them is a
        // state the subscriber can correct.
        Bridged::Answered((_, Err(refusal))) => {
            return frame_async::write(
                &mut writer,
                frame::Kind::Refusal,
                refusal.to_string().as_bytes(),
            )
            .await;
        }
        Bridged::Busy(_) => {
            return frame_async::write(&mut writer, frame::Kind::Refusal, BUSY.as_bytes()).await;
        }
        Bridged::Panicked => {
            return Err(Error::Io(std::io::Error::other(
                "opening the feed panicked",
            )));
        }
    };
    let mut commits = talk.committed.watching();
    let mut stray = [0_u8; 64];
    loop {
        // A staged shutdown reaches a feed here, within one wait: the cursor is a
        // position the subscriber holds, so it resumes exactly where it stopped.
        if talk.stopping.asked() {
            return Ok(());
        }
        let db = Arc::clone(&talk.db);
        let ran = talk
            .rounds
            .call(
                (session, following),
                move |(held, mut open): (Detached, Feed)| {
                    let mut attached = held.attach(db.store());
                    let mut frames = Vec::new();
                    let round =
                        open.round(&db, &mut attached, &mut |change, name, allowed, cursor| {
                            // A change whose table has been dropped has no name to give.
                            if let Some(named) =
                                push::named(change, name.map(str::to_owned), cursor)
                            {
                                frames.push(named.hiding(allowed).encode());
                            }
                            true
                        });
                    ((attached.detach(), open), round, frames)
                },
            )
            .await;
        let round = match ran {
            Bridged::Answered(((back, open), round, frames)) => {
                (session, following) = (back, open);
                for change in &frames {
                    // A subscriber that stops reading ends its own feed rather
                    // than holding its place until the process stops.
                    tokio::time::timeout(
                        READING,
                        frame_async::write(&mut writer, frame::Kind::Change, change),
                    )
                    .await
                    .map_err(|_| Error::Io(std::io::ErrorKind::TimedOut.into()))??;
                }
                round
            }
            // Every round slot is taken: this feed waits for the next signal. A
            // busy node delays a feed; it does not refuse a subscriber who was
            // already admitted.
            Bridged::Busy((back, open)) => {
                (session, following) = (back, open);
                Ok(Round::Empty)
            }
            Bridged::Panicked => {
                return Err(Error::Io(std::io::Error::other("a feed round panicked")));
            }
        };
        match round {
            Ok(Round::Delivered) => continue,
            Ok(Round::Empty | Round::Ended) => {}
            Err(refusal) => {
                return frame_async::write(
                    &mut writer,
                    frame::Kind::Refusal,
                    refusal.to_string().as_bytes(),
                )
                .await;
            }
        }
        tokio::select! {
            biased;
            read = reader.read(&mut stray) => match read {
                // End of stream, or a socket error: the subscriber has gone.
                Ok(0) | Err(_) => return Ok(()),
                // Bytes mean the subscriber is still there; a feed carries
                // nothing in that direction, so they are not answered.
                Ok(_) => {}
            },
            _ = commits.changed() => {}
            () = tokio::time::sleep(feed::PATIENCE_BETWEEN_ROUNDS) => {}
        }
    }
}
