use super::{
    COLLECTION_BUDGET_BYTES, Catalog, Collect, Collected, Collector, LogId, NODE_ID_LEN, Reach,
    Result, Serving, StoreValue, logs_to_collect,
};
use tessari_types::{DatabaseId, NamespaceId};

use crate::error::Error;
use crate::grant::Deciding;
use crate::link::tests::{Authority, THERE, hello, settled};
use crate::link::{Answered, Ask};
use crate::peer::Purpose;
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use tessari_encoding::{LogRecord, Writer};
use tessari_types::{Epoch, Sequence};
use tessaridb::{Db, Outcome};

mod applying;
mod budget;
mod door;
mod frames;
mod reach;

/// The node every door in this module belongs to.
const LEADER: [u8; NODE_ID_LEN] = [70_u8; NODE_ID_LEN];

/// A cluster in which everybody is subscribed to everything.
///
/// The transfer is what this module's tests are about — that a batch
/// applies in order, that a chain refuses a substituted record, that a short
/// answer means level — and every one of them would otherwise have to
/// declare a peer in a catalog to say so. What the catalog actually answers
/// is tested where it is decided, against a real declaration.
#[derive(Debug)]
struct Everything;

impl super::Subscriptions for Everything {
    fn granted(&self, _follower: [u8; NODE_ID_LEN]) -> Result<Option<Reach>> {
        Ok(Some(Reach::Store))
    }
}

/// A store holding one empty record per epoch named, at 1, 2, 3…
///
/// Empty records because what is being tested is the transfer and the
/// leadership it states, and a mutation would only make the assertions
/// longer. The epochs are what a run of leaderships actually looks like in
/// the log, written straight in so a test does not have to hold a lease per
/// epoch to produce them. Filed in the log the store's commits go to — the
/// line's on the store (ADR-0107), which is the one a leader serves.
fn logged(epochs: &[u64]) -> Arc<Db> {
    let db = Arc::new(Db::in_memory().expect("an in-memory store"));
    let writer = store_log(&db).writer;
    logging(&db, writer, epochs);
    db
}

/// The same, in the log `writer` allocates into.
///
/// A follower's copy of a leader's log is filed under the log's writer, so
/// a fixture that stands a follower part-way through one has to say whose
/// log it is standing in. Seeding it under the follower's own name builds a
/// second log that the collect below never reads, and the symptom is a gap
/// at position one rather than the disagreement the test is about.
fn logged_as(writer: Writer, epochs: &[u64]) -> Arc<Db> {
    let db = Arc::new(Db::in_memory().expect("an in-memory store"));
    logging(&db, writer, epochs);
    db
}

/// Apply one empty record per epoch, into `writer`'s log.
fn logging(db: &Arc<Db>, writer: Writer, epochs: &[u64]) {
    for (index, epoch) in epochs.iter().enumerate() {
        let at = Sequence::new(
            u64::try_from(index)
                .expect("a handful of records")
                .saturating_add(1),
        );
        db.store()
            .apply_record(writer, at, &LogRecord::at(Epoch::new(*epoch), Vec::new()))
            .expect("an empty record applies at the next position");
    }
}

/// The store's log as a leader serves it — the line's (ADR-0107).
fn store_log(db: &Arc<Db>) -> LogId {
    db.store()
        .line_log(Reach::Store)
        .expect("the store's own identity")
}

/// What one empty record costs in the answer, measured rather than assumed.
///
/// The budget is in bytes, so a test that hard-coded a size would be
/// asserting today's encoding instead of the bound.
fn one_record() -> usize {
    LogRecord::at(Epoch::new(1), Vec::new()).encode().len()
}

/// Every namespace a store can name, in catalog order.
fn declared(db: &Db) -> Vec<String> {
    let mut transaction = db.store().begin().expect("a read");
    let names = Catalog::new(&mut transaction)
        .namespaces()
        .expect("the catalog answers")
        .into_iter()
        .map(|namespace| namespace.name)
        .collect();
    transaction.rollback();
    names
}

/// A leader whose catalog actually grants, built by running statements.
///
/// The other helper in this module applies log records directly, which is
/// the right shape for a test about the transfer and the wrong one here: a
/// subscription is a catalog record, so the only honest way to have one is
/// to have declared it.
fn granting(clause: &str) -> Arc<Db> {
    let db = Arc::new(Db::in_memory().expect("an in-memory store"));
    db.session()
        .run(&format!(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE orders; \
                 USE DATABASE orders; DEFINE COLLECTION users; \
                 CREATE users:1 = {{ name: 'ada' }}; \
                 DEFINE NAMESPACE other; USE NAMESPACE other; DEFINE DATABASE ledger; \
                 USE DATABASE ledger; DEFINE COLLECTION secrets; \
                 CREATE secrets:1 = {{ word: 'shibboleth' }}; \
                 DEFINE REPLICA follower AT '127.0.0.1:1' NODE '{}'{clause};",
            spelled(THERE)
        ))
        .expect("the leader's own statements run");
    db
}

/// A node id as a `NODE` clause takes it: thirty-two hex digits.
fn spelled(node: [u8; NODE_ID_LEN]) -> String {
    node.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A peer door for `LEADER` serving one connection, asking `db`'s own
/// catalog who may collect.
fn declaring(authority: &Authority, db: &Arc<Db>) -> (SocketAddr, JoinHandle<()>) {
    declaring_for(authority, db, 1)
}

/// The same door, serving `rounds` connections.
///
/// A collection is one connection, and a follower walking the chain from the
/// store's log down to its own reach makes one per log — so a test about the
/// chain has to say how many, and has to then make exactly that many or the
/// join waits on an accept nobody ever performs.
fn declaring_for(
    authority: &Authority,
    db: &Arc<Db>,
    rounds: usize,
) -> (SocketAddr, JoinHandle<()>) {
    let peers = crate::link::tests::bind_with(
        "127.0.0.1:0",
        authority.issue(LEADER, Purpose::Peer),
        &authority.der(),
    )
    .expect("a peer door on loopback");
    let address = peers.address().expect("the door's address");
    let mine = hello(LEADER);
    let db = Arc::clone(db);
    let door = std::thread::spawn(move || {
        for _ in 0..rounds {
            drop(peers.greet(
                || Ok(mine),
                &LEADER,
                &Deciding::holding(settled()),
                &Serving::declared(db.store()),
            ));
        }
    });
    (address, door)
}

/// A peer door for `LEADER` that serves `rounds` connections out of `db`.
fn serving(authority: &Authority, db: &Arc<Db>, rounds: usize) -> (SocketAddr, JoinHandle<()>) {
    serving_within(authority, db, rounds, COLLECTION_BUDGET_BYTES)
}

/// The same door, serving under a byte budget a test can actually reach.
fn serving_within(
    authority: &Authority,
    db: &Arc<Db>,
    rounds: usize,
    budget: usize,
) -> (SocketAddr, JoinHandle<()>) {
    let peers = crate::link::tests::bind_with(
        "127.0.0.1:0",
        authority.issue(LEADER, Purpose::Peer),
        &authority.der(),
    )
    .expect("a peer door on loopback");
    let address = peers.address().expect("the door's address");
    let mine = hello(LEADER);
    let db = Arc::clone(db);
    let door = std::thread::spawn(move || {
        for _ in 0..rounds {
            drop(peers.greet(
                || Ok(mine),
                &LEADER,
                &Deciding::holding(settled()),
                &Serving::within(db.store(), &Everything, budget),
            ));
        }
    });
    (address, door)
}

/// Ask the door at `address` for the store log's records after `from`.
fn collect(
    authority: &Authority,
    address: SocketAddr,
    from: u64,
    limit: u64,
) -> crate::error::Result<Answered> {
    collect_from(authority, address, Reach::Store, from, limit)
}

/// The same, naming the log.
fn collect_from(
    authority: &Authority,
    address: SocketAddr,
    home: Reach,
    from: u64,
    limit: u64,
) -> crate::error::Result<Answered> {
    Ok(crate::link::tests::call_with(
        address,
        authority.issue(THERE, Purpose::Peer),
        &authority.der(),
        LEADER,
        &hello(THERE),
        Ask::Records(Collect {
            home,
            from: Sequence::new(from),
            limit,
        }),
    )?
    .1)
}

/// The records in an answer, or `None` when the peer answered otherwise.
///
/// An `Option` and not a panicking unwrap because the workspace denies
/// panicking paths, and `.expect` at the call site says what was expected
/// in the same place the assertion about it lives.
fn served(answered: Answered) -> Option<Collected> {
    match answered {
        Answered::Collected(collected) => Some(collected),
        _ => None,
    }
}

/// A follower that collects from `address`, with a bound of `limit`.
fn collector<'a>(
    keys: &'a crate::keys::PeerKeys,
    said: &'a crate::peer::Hello,
    address: SocketAddr,
    limit: u64,
) -> Collector<'a> {
    Collector {
        keys,
        said,
        peer: (LEADER, address),
        limit,
    }
}
