//! Following changes — the part every surface needs and none of them owns.
//!
//! # Why this is here and not in a serving crate
//!
//! Two surfaces now push changes: the wire protocol and a browser's WebSocket.
//! Almost nothing they need in order to do it is about their protocol. Between
//! the request and the bytes sits a body of work that decides **who may see
//! what** — the session's read grant, the tenancy it selected, whether a named
//! table is one it was granted, and which fields of each record it may be shown.
//! Only two things at either end are protocol-specific: how the request arrived,
//! and how a change is encoded on the way out.
//!
//! A second copy of that middle would be two mechanisms that must agree about
//! an access decision, and the wire feed's own comments record that this surface
//! has already produced a hole of exactly that shape twice. A divergence between
//! two feeds about who may read a table does not announce itself; it just serves
//! somebody a table nobody granted them. So the middle lives here, once, and the
//! surfaces bring their own ends (ADR-0012 — what two layers both need moves
//! down).
//!
//! # It takes closures rather than a server
//!
//! `stop` is a predicate and `deliver` is a sink, so this function knows nothing
//! about shutdown types or frame types and can be exercised without a server at
//! all. That is also what the dependency rule requires: the crate that owns
//! shutdown has no dependencies and is not depended on from here.

use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use tessari_session::redact::Visible;
use tessari_storage::{Change, Watch};
use tessari_types::{DatabaseId, NamespaceId, TableId};

use crate::{Db, Sequence, Session};

mod cursor;
mod refused;
mod source;

pub use refused::FeedRefused;

/// How long a pusher blocks before looking at the world again.
///
/// The upper bound on noticing a shutdown, and on delivering a change whose
/// commit did not signal this node.
const PATIENCE: Duration = Duration::from_millis(250);

/// How many changes one poll may take.
const MOUTHFUL: usize = 256;

/// A wake-up shared by the connections of one node.
///
/// Deliberately **not** a condvar in the commit path: the feed design leaves
/// that path lock-free, and reaching into it would be to learn something a node
/// already knows when a commit arrives through one of its own connections.
///
/// A node whose commit happened elsewhere — through a different surface, or a
/// different process — is never signalled, and that costs latency rather than
/// correctness, because [`Commits::wait`] returns on a timeout regardless.
pub struct Commits {
    count: Mutex<u64>,
    happened: Condvar,
    /// The same count, for a feed that waits as a task rather than a thread.
    ///
    /// A `watch` and not a second condvar: a waiting task holds no thread, and a
    /// receiver that missed several signals sees only that the count moved,
    /// which is all a feed does with it.
    announced: tokio::sync::watch::Sender<u64>,
}

impl Default for Commits {
    fn default() -> Self {
        Self {
            count: Mutex::default(),
            happened: Condvar::default(),
            announced: tokio::sync::watch::Sender::new(0),
        }
    }
}

impl Commits {
    /// Something was committed.
    pub fn signal(&self) {
        if let Ok(mut count) = self.count.lock() {
            *count = count.saturating_add(1);
        }
        self.happened.notify_all();
        self.announced
            .send_modify(|count| *count = count.wrapping_add(1));
    }

    /// A handle an async feed awaits instead of calling [`Commits::wait`].
    #[must_use]
    pub fn watching(&self) -> tokio::sync::watch::Receiver<u64> {
        self.announced.subscribe()
    }

    /// Wait for a commit after `seen`, and answer the count now.
    ///
    /// Returns after [`PATIENCE`] regardless, so a missed signal delays a change
    /// rather than losing it.
    #[must_use]
    pub fn wait(&self, seen: u64) -> u64 {
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

/// What a subscriber asked to follow.
pub struct Following<'a> {
    /// The first position to read, inclusive.
    pub from: Sequence,
    /// The table to watch, or every table in the session's database.
    pub table: Option<&'a str>,
    /// Where to resume a feed over a split table: the cursor the last change it
    /// handled carried. Without one, `from` counts in the database's log and
    /// each shard's log is read from its beginning.
    pub cursor: Option<&'a str>,
}

/// Whether a delivered change reached its subscriber.
///
/// A sink says `false` when the connection is gone, which ends the feed the way
/// a write error would — without this module needing to know what a connection
/// is.
pub type Delivered = bool;

/// Where a feed delivers: the change, its table's name, what of it the
/// subscriber may see, and — on a feed over a split table — the cursor to
/// resume after it.
pub type Deliver<'s> = dyn FnMut(&Change, Option<&str>, &Visible, Option<&str>) -> Delivered + 's;

/// How long an async feed waits for a commit signal before looking anyway.
///
/// The same bound [`Commits::wait`] keeps: a commit made elsewhere never signals
/// this node, so a feed looks at least this often whatever it is told.
pub const PATIENCE_BETWEEN_ROUNDS: Duration = PATIENCE;

/// What one round of a feed did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Round {
    /// Changes were read and handed to the sink; look again at once.
    Delivered,
    /// Nothing new in the log; wait for a commit before looking again.
    Empty,
    /// The sink refused a change — the subscriber is gone.
    Ended,
}

/// A feed that has been opened: what it follows, and where it has reached.
///
/// Opening asks every question a subscription is refused on; a [`Feed::round`]
/// asks the ones that can change while it runs again, then reads. Held between
/// rounds by whoever drives it — a thread that waits on a condvar, or a task
/// that waits on the socket, the commit signal and a stop token at once.
pub struct Feed {
    table: Option<String>,
    tenancy: (NamespaceId, DatabaseId),
    split: Vec<source::Split>,
    source: source::Source,
    /// What a subscriber may see of each table, resolved once per table per
    /// round rather than once per change.
    visible: BTreeMap<TableId, Visible>,
}

impl Feed {
    /// Open a feed for `session`, or say why it is refused.
    ///
    /// # Errors
    ///
    /// Returns the refusal when the session may not read, has selected no
    /// database, has selected one that has since gone, or named a table that
    /// does not exist or that it was not granted; a feed over a split table is
    /// also refused a cursor it cannot read and a log holding another node's
    /// writes.
    pub fn open(
        db: &Db,
        session: &mut Session<'_>,
        asked: &Following<'_>,
    ) -> Result<Self, FeedRefused> {
        if let Err(refusal) = session.may_read(db.store()) {
            return Err(refusal.into());
        }
        // The *table* question, which `may_read` does not answer. A
        // grant-governed subscriber sees what they were granted and nothing
        // else — the same answer their `SELECT` per table would give.
        let readable = session.readable(db.store())?;
        let (Some(namespace), Some(database)) = (session.namespace(), session.database()) else {
            return Err(FeedRefused::NoDatabaseSelected);
        };
        let Some(tenancy) = db.tenancy_in(namespace, database).ok().flatten() else {
            return Err(FeedRefused::TenancyGone);
        };
        let watch = match asked.table {
            None => Watch::default(),
            Some(name) => match db.table_in(namespace, database, name) {
                Ok(Some(table)) => {
                    // Named explicitly, so the refusal is better than a feed
                    // that silently delivers nothing forever. Watching
                    // *everything* filters instead — a different answer on
                    // purpose, because "everything I was granted" is what the
                    // same user's reads say.
                    if readable.as_ref().is_some_and(|held| !held.contains(&table)) {
                        return Err(FeedRefused::TableNotGranted {
                            table: name.to_owned(),
                        });
                    }
                    Watch::table(table)
                }
                Ok(None) => {
                    return Err(FeedRefused::NoSuchTable {
                        table: name.to_owned(),
                    });
                }
                Err(failure) => return Err(failure.into()),
            },
        };
        let table = asked.table.map(str::to_owned);
        // A split table's single-shard writes are in its shards' logs and a
        // write to two shards is in its database's (G031, ADR-0080), so a feed
        // whose scope holds one follows all of them, in the writer's order,
        // with a cursor per log (Q-791). A scope with none reads the database's
        // log alone.
        let split = in_scope(table.as_deref(), db.split_tables_in(tenancy.0, tenancy.1)?);
        let source = source::Source::open(db, tenancy, &split, asked.from, asked.cursor, watch)?;
        Ok(Self {
            table,
            tenancy,
            split,
            source,
            visible: BTreeMap::new(),
        })
    }

    /// Ask every authority again, then hand `deliver` what the log holds next.
    ///
    /// # Errors
    ///
    /// Returns a refusal **mid-feed** when the authority it was reading on is
    /// taken away, which is what ends a subscription a revocation was meant to
    /// end, and when a table it follows is split under it.
    pub fn round(
        &mut self,
        db: &Db,
        session: &mut Session<'_>,
        deliver: &mut Deliver<'_>,
    ) -> Result<Round, FeedRefused> {
        // Every authority this feed rests on is asked again, here, once a
        // round. Asked only at the start they would be bounded by the
        // subscription's lifetime — which for a connection that stays open is no
        // bound at all, and is the longest-lived hole a revocation can leave.
        if let Err(refusal) = session.may_read(db.store()) {
            return Err(refusal.into());
        }
        let readable = session.readable(db.store())?;
        // The field grant too: cached per table for the round, and a round is
        // what the cache lives for. A revocation that reached the table list but
        // not the field list would keep pushing a column nobody grants.
        self.visible.clear();
        // And the logs: a table split, or split again, after this feed began
        // writes into logs it is not reading, with nothing in an error state.
        let now = in_scope(
            self.table.as_deref(),
            db.split_tables_in(self.tenancy.0, self.tenancy.1)?,
        );
        if now != self.split {
            return Err(FeedRefused::SplitAfterStart);
        }
        let changes = self.source.next(db, MOUTHFUL)?;
        if changes.is_empty() {
            return Ok(Round::Empty);
        }
        for (change, resume) in &changes {
            // The log is every tenancy's. `Watch` filters by table and knows
            // nothing about namespaces, so this is where a subscriber is kept
            // inside the database it selected.
            if (change.namespace, change.database) != self.tenancy {
                continue;
            }
            // And the grant, for a subscriber watching everything.
            if readable
                .as_ref()
                .is_some_and(|held| !held.contains(&change.table))
            {
                continue;
            }
            let allowed = match self.visible.get(&change.table) {
                Some(held) => held.clone(),
                None => {
                    let held = session.visible(db.store(), change.table).unwrap_or(None);
                    self.visible.insert(change.table, held.clone());
                    held
                }
            };
            // A change whose table has been dropped has no name to give, and
            // inventing one would be worse than not sending it. The cursor has
            // already moved past it either way.
            let named = db.table_name(change.table).unwrap_or(None);
            if !deliver(change, named.as_deref(), &allowed, resume.as_deref()) {
                return Ok(Round::Ended);
            }
        }
        Ok(Round::Delivered)
    }
}

/// The split tables in a feed's scope: all of them, or the one it named.
fn in_scope(table: Option<&str>, split: Vec<source::Split>) -> Vec<source::Split> {
    split
        .into_iter()
        .filter(|(name, _, _)| table.is_none_or(|asked| asked == name))
        .collect()
}

/// Push changes to `deliver` until `stop` says otherwise or delivery fails.
///
/// `deliver` is given the change, its table's name, what of it the subscriber
/// may see, and — on a feed over a split table — the cursor to resume after it.
///
/// The returned error is a refusal to start, phrased for the subscriber: it is
/// the answer to "why am I not following anything", and every one of them is a
/// state the subscriber can correct.
///
/// # Errors
///
/// As [`Feed::open`] before the first change, and as [`Feed::round`] after it.
pub fn follow(
    db: &Db,
    session: &mut Session<'_>,
    asked: &Following<'_>,
    committed: &Commits,
    stop: &dyn Fn() -> bool,
    deliver: &mut Deliver<'_>,
) -> Result<(), FeedRefused> {
    let mut feed = Feed::open(db, session, asked)?;
    let mut seen = 0;
    loop {
        // A staged shutdown reaches a feed here. A pusher spends its life
        // blocked in `wait` below, which returns after `PATIENCE` whether or not
        // anything happened — so the flag is noticed within that, and a
        // subscription ends by its own loop rather than by its socket being
        // taken from under it. What a subscriber loses is nothing: the cursor is
        // a position it holds, so it resumes exactly where it stopped.
        if stop() {
            return Ok(());
        }
        match feed.round(db, session, deliver)? {
            Round::Delivered => {}
            Round::Empty => seen = committed.wait(seen),
            Round::Ended => return Ok(()),
        }
    }
}
