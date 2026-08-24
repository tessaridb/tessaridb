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
use tessari_types::TableId;

use crate::{Db, Sequence, Session};

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
#[derive(Default)]
pub struct Commits {
    count: Mutex<u64>,
    happened: Condvar,
}

impl Commits {
    /// Something was committed.
    pub fn signal(&self) {
        if let Ok(mut count) = self.count.lock() {
            *count = count.saturating_add(1);
        }
        self.happened.notify_all();
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
}

/// Whether a delivered change reached its subscriber.
///
/// A sink says `false` when the connection is gone, which ends the feed the way
/// a write error would — without this module needing to know what a connection
/// is.
pub type Delivered = bool;

/// Push changes to `deliver` until `stop` says otherwise or delivery fails.
///
/// The returned error is a refusal to start, phrased for the subscriber: it is
/// the answer to "why am I not following anything", and every one of them is a
/// state the subscriber can correct.
///
/// # Errors
///
/// Returns the refusal when the session may not read, has selected no database,
/// has selected one that has since gone, or named a table that does not exist or
/// that it was not granted.
pub fn follow(
    db: &Db,
    session: &Session<'_>,
    asked: &Following<'_>,
    committed: &Commits,
    stop: &dyn Fn() -> bool,
    deliver: &mut dyn FnMut(&Change, Option<&str>, &Visible) -> Delivered,
) -> Result<(), String> {
    if let Err(refusal) = session.may_read(db.store()) {
        return Err(refusal.to_string());
    }
    // The *table* question, which `may_read` does not answer. A grant-governed
    // subscriber sees what they were granted and nothing else — the same answer
    // their `SELECT` per table would give.
    let readable = session
        .readable(db.store())
        .map_err(|failure| failure.to_string())?;
    let (Some(namespace), Some(database)) = (session.namespace(), session.database()) else {
        return Err("no database is selected to follow the changes to".to_owned());
    };
    let Some(tenancy) = db.tenancy_in(namespace, database).ok().flatten() else {
        return Err("that namespace and database are not both there".to_owned());
    };
    let watch = match asked.table {
        None => Watch::default(),
        Some(name) => match db.table_in(namespace, database, name) {
            Ok(Some(table)) => {
                // Named explicitly, so the refusal is better than a feed that
                // silently delivers nothing forever. Watching *everything*
                // filters instead — a different answer on purpose, because
                // "everything I was granted" is what the same user's reads say.
                if readable.as_ref().is_some_and(|held| !held.contains(&table)) {
                    return Err(format!(
                        "{name:?} is not a table this session has been granted to read"
                    ));
                }
                Watch::table(table)
            }
            Ok(None) => return Err(format!("no table named {name:?} to watch")),
            Err(failure) => return Err(failure.to_string()),
        },
    };

    // What a subscriber may see of each table, resolved once per table rather
    // than once per change. The feed pushes whole records and never passes
    // through a session's read path, so a field grant reaches it here or not at
    // all — and "not at all" means pushing a field nobody granted.
    let mut visible: BTreeMap<TableId, Visible> = BTreeMap::new();
    let mut subscription = Db::subscribe(asked.from, watch);
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
        let changes = db
            .poll(&mut subscription, MOUTHFUL)
            .map_err(|failure| failure.to_string())?;
        for change in &changes {
            // The log is every tenancy's. `Watch` filters by table and knows
            // nothing about namespaces, so this is where a subscriber is kept
            // inside the database it selected.
            if (change.namespace, change.database) != tenancy {
                continue;
            }
            // And the grant, for a subscriber watching everything.
            if readable
                .as_ref()
                .is_some_and(|held| !held.contains(&change.table))
            {
                continue;
            }
            let allowed = match visible.get(&change.table) {
                Some(held) => held.clone(),
                None => {
                    let held = session.visible(db.store(), change.table).unwrap_or(None);
                    visible.insert(change.table, held.clone());
                    held
                }
            };
            // A change whose table has been dropped has no name to give, and
            // inventing one would be worse than not sending it. The cursor has
            // already moved past it either way.
            let named = db.table_name(change.table).unwrap_or(None);
            if !deliver(change, named.as_deref(), &allowed) {
                return Ok(());
            }
        }
        if changes.is_empty() {
            seen = committed.wait(seen);
        }
    }
}
