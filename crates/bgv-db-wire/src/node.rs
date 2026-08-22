//! The node: listening, conversing, and pushing.
//!
//! Everything about being the server end of this protocol. The client end is
//! `client.rs`, and the two share only the frames.

use std::io::{BufReader, BufWriter};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Condvar, Mutex};

use bgv_db::{DatabaseId, Db, NamespaceId, Sequence, Watch};

use crate::error::{Error, Result};
use crate::message::Request;
use crate::push::Follow;
use crate::{MOUTHFUL, PATIENCE, READING, frame, message, push};

/// A node listening for connections.
pub struct Node {
    listener: TcpListener,
    db: Arc<Db>,
    committed: Arc<Commits>,
}

/// The signal that a commit happened, and nothing else.
///
/// Deliberately not a registry of subscribers: it holds a counter, so it knows
/// how many commits have gone by and nothing at all about who is waiting. The
/// feed design keeps the store free of subscriber state, and this keeps the node
/// free of it too.
///
/// It lives here rather than in the store because a store is opened by one
/// process — the file lock proves it — so every commit against this store
/// arrives through a connection of this node. Putting a condvar in the commit
/// path would be reaching into the one place the feed design deliberately left
/// lock-free, to learn something this node already knows.
#[derive(Default)]
struct Commits {
    count: Mutex<u64>,
    happened: Condvar,
}

impl Commits {
    /// Something was committed.
    fn signal(&self) {
        if let Ok(mut count) = self.count.lock() {
            *count = count.saturating_add(1);
        }
        self.happened.notify_all();
    }

    /// Wait for a commit after `seen`, and answer the count now.
    ///
    /// Returns after [`PATIENCE`] regardless, so a missed signal delays a change
    /// rather than losing it.
    fn wait(&self, seen: u64) -> u64 {
        let Ok(count) = self.count.lock() else {
            return seen;
        };
        if *count != seen {
            return *count;
        }
        self.happened
            .wait_timeout(count, PATIENCE)
            .map_or(seen, |(held, _)| *held)
    }
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

    /// Serve until the listener fails.
    ///
    /// One thread per connection — see the module documentation for why that is
    /// the decision rather than the shortfall.
    pub fn serve(&self) {
        for stream in self.listener.incoming() {
            let Ok(stream) = stream else { continue };
            let db = Arc::clone(&self.db);
            let committed = Arc::clone(&self.committed);
            // A connection that goes wrong takes its own thread down and nothing
            // else: a node that could be stopped by one client's malformed frame
            // would be a node anybody can stop.
            drop(std::thread::spawn(move || {
                drop(converse(&db, &committed, stream));
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
        converse(&self.db, &self.committed, stream)
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
fn converse(db: &Db, committed: &Commits, stream: TcpStream) -> Result<()> {
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
            return follow(
                db,
                committed,
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
            frame::write(
                &mut writer,
                frame::Kind::Refusal,
                refusal.to_string().as_bytes(),
            )?;
            continue;
        }
        match session.run(&request.script) {
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
                frame::write(&mut writer, frame::Kind::Answer, &answer)?;
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
            Err(refusal) => frame::write(
                &mut writer,
                frame::Kind::Refusal,
                refusal.to_string().as_bytes(),
            )?,
        }
    }
    Ok(())
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
/// - **May this caller read at all.** [`bgv_db::Session::may_read`] — on a
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
    session: &bgv_db::Session<'_>,
    writer: &mut BufWriter<TcpStream>,
    asked: &Follow,
) -> Result<()> {
    if let Err(refusal) = session.may_read(db.store()) {
        return refuse(writer, &refusal.to_string());
    }
    // The *table* question, which `may_read` does not answer. A grant-governed
    // subscriber sees what they were granted and nothing else — the same answer
    // their `SELECT` per table would give. Forgetting this is how a feed hands
    // somebody a table nobody granted them, which is the shape of hole this
    // surface has now produced twice.
    let readable = match session.readable(db.store()) {
        Ok(readable) => readable,
        Err(failure) => return refuse(writer, &failure.to_string()),
    };
    let (Some(namespace), Some(database)) = (session.namespace(), session.database()) else {
        return refuse(writer, "no database is selected to follow the changes to");
    };
    let Some(tenancy) = tenancy(db, namespace, database) else {
        return refuse(writer, "that namespace and database are not both there");
    };
    let watch = match &asked.table {
        None => Watch::default(),
        Some(name) => match db.table_in(namespace, database, name) {
            Ok(Some(table)) => {
                // Named explicitly, so the refusal is better than a feed that
                // silently delivers nothing forever.
                if readable.as_ref().is_some_and(|held| !held.contains(&table)) {
                    return refuse(
                        writer,
                        &format!("{name:?} is not a table this session has been granted to read"),
                    );
                }
                Watch::table(table)
            }
            Ok(None) => return refuse(writer, &format!("no table named {name:?} to watch")),
            Err(failure) => return refuse(writer, &failure.to_string()),
        },
    };

    // The socket, not a buffer here, is what a slow subscriber pushes back on —
    // and a client that never reads at all ends its own connection rather than
    // holding this thread until the process stops.
    writer.get_ref().set_write_timeout(Some(READING))?;

    // What a subscriber may see of each table, resolved once per table rather
    // than once per change. The feed pushes whole records and never passes
    // through a session's read path, so a field grant reaches it here or not at
    // all — and "not at all" means pushing a field nobody granted.
    let mut visible: std::collections::BTreeMap<bgv_db::TableId, bgv_db::Visible> =
        std::collections::BTreeMap::new();
    let mut subscription = Db::subscribe(Sequence::new(asked.from), watch);
    let mut seen = 0;
    loop {
        let changes = db
            .poll(&mut subscription, MOUTHFUL)
            .map_err(|failure| Error::Refused {
                message: failure.to_string(),
            })?;
        for change in &changes {
            // The log is every tenancy's. `Watch` filters by table and knows
            // nothing about namespaces, so this is where a subscriber is kept
            // inside the database it selected.
            if (change.namespace, change.database) != tenancy {
                continue;
            }
            // And the grant, for a subscriber watching everything. Filtered
            // rather than refused, because "everything I was granted" is what
            // the same user's reads answer.
            if readable
                .as_ref()
                .is_some_and(|held| !held.contains(&change.table))
            {
                continue;
            }
            // A change whose table has been dropped has no name to give, and
            // inventing one would be worse than not sending it. The cursor has
            // already moved past it either way.
            let allowed = match visible.get(&change.table) {
                Some(held) => held.clone(),
                None => {
                    let held = session.visible(db.store(), change.table).unwrap_or(None);
                    visible.insert(change.table, held.clone());
                    held
                }
            };
            let Some(named) = push::named(change, db.table_name(change.table).unwrap_or(None))
            else {
                continue;
            };
            frame::write(
                writer,
                frame::Kind::Change,
                &named.hiding(&allowed).encode(),
            )?;
        }
        if changes.is_empty() {
            seen = committed.wait(seen);
        }
    }
}

/// The ids of the namespace and database a session has selected.
///
/// `None` when either has gone since the `USE` that named it, which is a
/// refusal rather than a subscription to nothing.
fn tenancy(db: &Db, namespace: &str, database: &str) -> Option<(NamespaceId, DatabaseId)> {
    db.tenancy_in(namespace, database).ok().flatten()
}

/// The store's own words, travelling as a refusal.
fn refuse(writer: &mut BufWriter<TcpStream>, message: &str) -> Result<()> {
    frame::write(writer, frame::Kind::Refusal, message.as_bytes())
}
