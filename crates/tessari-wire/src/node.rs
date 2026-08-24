//! The node: listening, conversing, and pushing.
//!
//! Everything about being the server end of this protocol. The client end is
//! `client.rs`, and the two share only the frames.

use std::io::{BufReader, BufWriter};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;

use tessaridb::feed::{self, Commits, Following};
use tessaridb::{Db, Sequence};
use tessari_serve::{Busy, Stopping};

use crate::error::{Error, Result};
use crate::message::Request;
use crate::push::Follow;
use crate::{READING, client, frame, message, push};

/// A node listening for connections.
pub struct Node {
    listener: TcpListener,
    db: Arc<Db>,
    committed: Arc<Commits>,
    stopping: Arc<Stopping>,
}

impl Node {
    /// Listen on `address`.
    ///
    /// # Errors
    ///
    /// Returns the operating system's failure when the address cannot be bound.
    pub fn bind(db: Arc<Db>, address: impl ToSocketAddrs) -> Result<Self> {
        Ok(Self {
            listener: TcpListener::bind(address)?,
            db,
            committed: Arc::new(Commits::default()),
            stopping: Stopping::new(),
        })
    }

    /// Where it is listening, which a caller needs when it asked for port zero.
    ///
    /// # Errors
    ///
    /// Returns the operating system's failure when the socket cannot say.
    pub fn address(&self) -> Result<String> {
        Ok(self.listener.local_addr()?.to_string())
    }

    /// What this node counts as in flight, and how it is told to stop.
    ///
    /// Taken **before** [`Node::serve`], which consumes the node's borrow for
    /// as long as it runs: a caller that waited until afterwards would be asking
    /// a node that has already stopped.
    #[must_use]
    pub fn stopping(&self) -> Arc<Stopping> {
        Arc::clone(&self.stopping)
    }

    /// Serve until the listener fails, or until stopping is asked for.
    ///
    /// One thread per connection — see the module documentation for why that is
    /// the decision rather than the shortfall.
    ///
    /// # Ending it
    ///
    /// `accept` blocks, and setting the flag does not wake it. A `TcpListener`
    /// has no equivalent of an unblock, so whoever asks this node to stop makes
    /// one throwaway connection to the address it printed — that connection is
    /// accepted, the loop checks the flag before serving it, and both end. The
    /// flag is set **first** or the loop can check it and block again, which is
    /// the race this ordering exists to avoid.
    pub fn serve(&self) {
        for stream in self.listener.incoming() {
            if self.stopping.asked() {
                break;
            }
            let Ok(stream) = stream else { continue };
            let db = Arc::clone(&self.db);
            let committed = Arc::clone(&self.committed);
            let stopping = Arc::clone(&self.stopping);
            let busy = self.stopping.busy();
            // A connection that goes wrong takes its own thread down and nothing
            // else: a node that could be stopped by one client's malformed frame
            // would be a node anybody can stop.
            drop(std::thread::spawn(move || {
                let mut busy = busy;
                drop(converse(&db, &committed, &stopping, &mut busy, stream));
            }));
        }
    }

    /// Serve exactly one connection, for a caller driving the loop itself.
    ///
    /// # Errors
    ///
    /// Returns the failure that ended the conversation.
    pub fn serve_one(&self) -> Result<()> {
        let (stream, _) = self.listener.accept()?;
        let mut busy = self.stopping.busy();
        converse(&self.db, &self.committed, &self.stopping, &mut busy, stream)
    }
}

/// One connection, from hello to hang-up.
///
/// # One session, not one per statement
///
/// The session is opened once and lives as long as the connection, because that
/// is what a connection *is*: `USE NAMESPACE prod;` selects something, and a
/// selection that does not survive to the next statement is not a selection. A
/// session per request would make a prompt over this protocol a sequence of
/// unrelated sessions that happen to share a socket, and every statement would
/// have to re-say where it was.
///
/// It also gives the thread-per-connection cost something to buy. A thread here
/// holds state a request cannot carry, which is the difference between a
/// connection and a datagram.
/// Write one answer to a request, and count it.
///
/// This surface has **three** places that answer a request — an answer, a
/// refused sign-in and a refused statement — where the HTTP surface has one. So
/// the mapping from this protocol's vocabulary onto the single word *refusal*
/// lives here rather than at each of the three, which is what stops the fourth
/// one from being written without it.
///
/// Only replies to requests pass through here. The greeting and a pushed frame
/// are not answers to anything and are not counted.
fn reply(
    writer: &mut impl std::io::Write,
    counting: &Stopping,
    kind: frame::Kind,
    body: &[u8],
) -> Result<()> {
    counting.answered(kind == frame::Kind::Refusal);
    frame::write(writer, kind, body)
}

fn converse(
    db: &Db,
    committed: &Commits,
    stopping: &Stopping,
    busy: &mut Busy,
    stream: TcpStream,
) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    {
        // The greeting needs both directions on one object; after it they are
        // used independently, which is what lets a push frame be written while a
        // read is waiting.
        let mut both = frame::Duplex {
            reader: &mut reader,
            writer: &mut writer,
        };
        frame::greet(&mut both)?;
    }

    let mut session = db.session();
    while let Some((kind, body)) = frame::read(&mut reader)? {
        if kind == frame::Kind::Subscribe {
            // The connection stops being a conversation and becomes a feed. See
            // `push.rs` for why one connection does one job.
            // No longer a request. A subscription never ends on its own, so a
            // shutdown that waited for it would always time out — moving it to
            // the feed count is what lets the drain finish and the stage after
            // it end the feeds deliberately.
            busy.became_a_feed();
            return follow(
                db,
                committed,
                stopping,
                &session,
                &mut writer,
                &Follow::decode(&body)?,
            );
        }
        if kind != frame::Kind::Request {
            // A client sending an answer is a client this build does not
            // understand, and continuing would be guessing at what it meant.
            return Err(Error::UnknownFrame { tag: kind.tag() });
        }
        let request = Request::decode(&body)?;
        if let Some((name, password)) = &request.credentials
            && let Err(refusal) = session.sign_in(name, password)
        {
            // The session's own refusal, travelling as one. A second rule here
            // would be a second place for "who may do this" to be decided.
            reply(
                &mut writer,
                stopping,
                frame::Kind::Refusal,
                refusal.to_string().as_bytes(),
            )?;
            continue;
        }
        let ran = session.run_with(&request.script, &request.parameters);
        // A write that arrived at a node which may not take it is routed, not
        // refused (ADR-0019 §2, case *forward*). Matched on the **variant**, not
        // on the message text: a routing decision taken by string comparison
        // changes meaning the day somebody rewords an error.
        if matches!(ran, Err(tessaridb::Error::NotWritable { .. })) {
            match forward(db, &request) {
                Ok((kind, body)) => reply(&mut writer, stopping, kind, &body)?,
                // The hop failed, and the client is told that rather than being
                // told the statement was wrong. It was not.
                Err(why) => reply(
                    &mut writer,
                    stopping,
                    frame::Kind::Refusal,
                    why.to_string().as_bytes(),
                )?,
            }
            continue;
        }
        match ran {
            Ok(outcomes) => {
                let mut answer = Vec::new();
                frame::put_u32(
                    &mut answer,
                    u32::try_from(outcomes.len()).unwrap_or(u32::MAX),
                );
                for outcome in &outcomes {
                    // Resolved here because the catalog is here. `names_in`
                    // walks the answer first and touches nothing when it holds
                    // no reference, which is most answers.
                    let names = message::names_for(db, outcome);
                    answer.extend_from_slice(&message::encode_outcome(outcome, &names));
                }
                reply(&mut writer, stopping, frame::Kind::Answer, &answer)?;
                // Every commit against this store arrives through some
                // connection of this node, so this is where a pusher learns
                // there is something to look at. A statement that wrote nothing
                // signals too: a pusher woken for nothing polls, finds nothing
                // and waits again, which is cheaper than reading the tail here
                // to find out.
                committed.signal();
            }
            // A refusal does not close the connection: a client that mistyped a
            // statement has not stopped being a client.
            Err(refusal) => reply(
                &mut writer,
                stopping,
                frame::Kind::Refusal,
                refusal.to_string().as_bytes(),
            )?,
        }
    }
    Ok(())
}

/// Send a write to the peer that may take it, and bring back what it said.
///
/// Routing case *forward* (ADR-0019 §2). Three things it deliberately does not
/// do, each of which is a feature this milestone does not have rather than an
/// oversight:
///
/// - **No retry.** ADR-0019 §2 names "the peer is down mid-request" as this
///   case's new failure mode. Naming it is the deliverable; a retry that
///   re-sent a statement whose first attempt may already have committed would
///   turn one failure into two writes.
/// - **No connection kept.** One dial per forwarded write, which is a real cost
///   and a measured one later — a pool is state shared between connections, and
///   it earns its complexity against a number nobody has yet.
/// - **No rewriting of the answer.** What the leader said travels back as it
///   was said, refusals included.
///
/// The caller's credentials go with it, so the leader authorises the same user
/// against its own grants. A forward that signed in as the forwarding node would
/// make every follower an authority its operator never granted.
///
/// # It does not yet detect being sent back to itself
///
/// Nothing here compares the target against this node. If the one peer declared
/// `writable` **is** this node — which an operator produces by draining the
/// leader, since dropping `WRITABLE` is local and leaves the replica row saying
/// otherwise — each hop dials this node again and spends another thread and
/// another connection. It does not recurse on one stack; it exhausts the node.
///
/// The endpoint cannot be used to recognise the loop, for the reason the target
/// is not found by endpoint in the first place (Q-108): a node's own declared
/// endpoints are empty by default and need not be spelled the way a peer spells
/// them, so the check would pass in exactly the deployments that need it. The
/// answer is a hop marker on the request, which is a wire-format change and is
/// held as **Q-109** rather than approximated here.
///
/// Until then this is an operational constraint and is written down as one: a
/// node being drained has its replica row corrected first, or it is drained
/// while nothing writes to it.
fn forward(db: &Db, request: &Request) -> Result<(frame::Kind, Vec<u8>)> {
    // The store's own words travel verbatim, as `Refused` documents — reading
    // the peer list can fail by naming two writable peers, and "two leaders are
    // declared" is precisely what the operator needs to be told.
    let declared = db.writable_peer().map_err(|why| Error::Refused {
        message: why.to_string(),
    })?;
    let Some(endpoint) = declared else {
        return Err(Error::NoWritablePeer);
    };
    let mut peer = client::Client::connect(endpoint)?;
    peer.relay(request)
}

/// Push changes down this connection until it ends.
///
/// # It reads records, so it answers to the same identity a read does
///
/// A subscription takes records from the log directly and never reaches the
/// executor, so nothing about running a statement applies to it automatically.
/// Two things therefore have to be asked here, and both are asked by the
/// session rather than decided again:
///
/// - **May this caller read at all.** [`tessaridb::Session::may_read`] — on a
///   closed store an anonymous connection is refused, exactly as a `SELECT`
///   would be. A client signs in by running a request with credentials first;
///   the session is the connection's, so it is still signed in here.
/// - **Whose changes.** The log is global — every namespace and every database
///   in the store is in it — so a subscription that did not confine itself would
///   hand a caller every write in the store regardless of the tenancy they are
///   in. It is confined to the namespace and database the session selected, and
///   selecting one already went through the tenancy check. That is also what
///   makes "watch everything" mean *everything in this database*, which is what
///   a caller who said `USE` means by it.
///
/// # What it refuses, and why refusing is the point
///
/// A table nobody has defined is refused rather than watched, because a
/// subscription to a name that does not exist looks exactly like a subscription
/// to a quiet table: it delivers nothing, forever, and says nothing about why.
fn follow(
    db: &Db,
    committed: &Commits,
    stopping: &Stopping,
    session: &tessaridb::Session<'_>,
    writer: &mut BufWriter<TcpStream>,
    asked: &Follow,
) -> Result<()> {
    // The socket, not a buffer here, is what a slow subscriber pushes back on —
    // and a client that never reads at all ends its own connection rather than
    // holding this thread until the process stops.
    writer.get_ref().set_write_timeout(Some(READING))?;

    // Everything between the request and the bytes — the grant, the tenancy,
    // the table, the field visibility, the polling — belongs to `tessaridb::feed`
    // and is shared with the socket surface, so the two cannot disagree about
    // who may see what. What is left here is this protocol's two ends.
    let following = Following {
        from: Sequence::new(asked.from),
        table: asked.table.as_deref(),
    };
    let mut failure = None;
    let outcome = feed::follow(
        db,
        session,
        &following,
        committed,
        &|| stopping.asked(),
        &mut |change, name, allowed| {
            let Some(named) = push::named(change, name.map(str::to_owned)) else {
                // A change whose table has been dropped has no name to give.
                return true;
            };
            match frame::write(writer, frame::Kind::Change, &named.hiding(allowed).encode()) {
                Ok(()) => true,
                Err(why) => {
                    // The connection is gone. Keep the reason so it reaches the
                    // caller rather than being reported as a clean end.
                    failure = Some(why);
                    false
                }
            }
        },
    );
    if let Some(why) = failure {
        return Err(why);
    }
    match outcome {
        Ok(()) => Ok(()),
        Err(refusal) => refuse(writer, &refusal),
    }
}

/// The store's own words, travelling as a refusal.
fn refuse(writer: &mut BufWriter<TcpStream>, message: &str) -> Result<()> {
    frame::write(writer, frame::Kind::Refusal, message.as_bytes())
}
