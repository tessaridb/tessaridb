//! The read's consistency level — one word naming the contract the read asks
//! for: `LEADER`, `SNAPSHOT`, `STALE WITHIN <span>`.
//!
//! # What a level is, and what it is not
//!
//! A level is the two read axes — authority and freshness — named together as
//! one word, with the anomaly contract that word carries stated in the
//! documentation. It is not a third axis: `LEADER` and `STALE WITHIN <span>`
//! desugar onto `ANSWERED BY LEADER` and `STALENESS <span>` and are enforced by
//! exactly the machinery those clauses have always had, which is why most of
//! the cases below are about the *refusals* — the level earns its place by
//! refusing where its contract cannot be met, not by answering anywhere new.
//! `SNAPSHOT` is the one level with no clause of its own: it names the promise
//! every statement already makes on a node holding everything it reads, and it
//! is refused wherever that promise cannot be kept — where the read would have
//! to gather a part from another node's present.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use core::time::Duration;

use tessari_constants::STALENESS_FLOOR_SECONDS;
use tessari_encoding::NODE_ID_LEN;
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Elsewhere, Error, Note, Peer, Session};
use tessari_storage::Store;
use tessari_types::{Epoch, RecordId};

use super::gathered_reads::{answer, pair, refused as refused_on_the_pair};

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap()
}

/// A tenant with one record to read.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE orders SCHEMALESS;\n\
             CREATE orders:1 = { total: 10 };",
        )
        .unwrap();
    session
}

fn allowed() -> String {
    format!("{}s", STALENESS_FLOOR_SECONDS.saturating_mul(3))
}

const THERE: [u8; NODE_ID_LEN] = [3; NODE_ID_LEN];
const THEIR_EPOCH: Epoch = Epoch::new(7);

/// A cluster of one peer, which may or may not claim it writes, whose copy is
/// `age` old. The same hand-written stub the staleness and answered-by suites
/// keep, for the same reason: the session's half is what these tests own.
#[derive(Debug)]
struct OnePeer {
    endpoint: String,
    leads: bool,
    age: Duration,
}

impl Elsewhere for OnePeer {
    fn within(&self, bound: Duration) -> Option<Peer> {
        (self.age <= bound).then(|| Peer {
            endpoint: self.endpoint.clone(),
            node: THERE,
            epoch: THEIR_EPOCH,
        })
    }

    fn serving(&self, _endpoint: &str, _node: &[u8; NODE_ID_LEN]) -> Option<Peer> {
        None
    }

    fn writable(&self) -> Option<Peer> {
        self.leads.then(|| Peer {
            endpoint: self.endpoint.clone(),
            node: THERE,
            epoch: THEIR_EPOCH,
        })
    }

    fn build_at(&self, _endpoint: &str) -> Option<tessari_encoding::NodeVersion> {
        None
    }

    fn leading(&self, _range: tessari_types::Reach) -> Option<Peer> {
        None
    }
}

fn peer(leads: bool, age: Duration) -> Arc<dyn Elsewhere> {
    Arc::new(OnePeer {
        endpoint: "two.example:9080".to_owned(),
        leads,
        age,
    })
}

/// A session on a node that is already `serving` only, the tenant in place.
fn reader(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

/// Level with the peer it collects from, as of now — through `Store::collected`,
/// the one way this node's currency is set anywhere.
fn level(store: &Store, reached: u64) {
    store.collected(
        tessari_types::Sequence::new(reached),
        tessari_storage::Currency::Level,
    );
}

#[test]
fn the_leader_level_is_the_authority_axis_named() {
    // Desugars to `ANSWERED BY LEADER`: a node that may write answers here; a
    // node that may not is sent to the one that claims it.
    let store = store();
    let mut session = ready(&store);

    session
        .run_now("SELECT * FROM orders LEADER;")
        .expect("a node that may write is the leader of its own records");

    // Drained of the writable role, the same node redirects the level to the
    // peer that claims it.
    session.run_now("DEFINE NODE ROLES serving;").unwrap();
    let mut follower = reader(&store).among(peer(true, Duration::ZERO));
    let sent = follower
        .run_now("SELECT * FROM orders LEADER;")
        .expect_err("a follower is redirected, not answered");
    let Error::ReadIsElsewhere { because, .. } = &sent else {
        panic!("the level desugared into nothing: {sent}");
    };
    assert!(
        because.contains("LEADER"),
        "the refusal names the level that sent the client away: {because}"
    );
}

#[test]
fn the_stale_level_is_the_freshness_axis_named() {
    // Desugars to `STALENESS <span>`: the floor, the copy age, and the refusal
    // discipline are the clause's own, unchanged.
    let store = store();
    let mut session = ready(&store);
    session
        .run_now(&format!("SELECT * FROM orders STALE WITHIN {};", allowed()))
        .expect("a leader is level with itself inside any bound it accepts");

    let refusal = session
        .run_now("SELECT * FROM orders STALE WITHIN 1s;")
        .unwrap_err();
    assert!(
        matches!(refusal, Error::StalenessBelowFloor { .. }),
        "the level is held to the same floor as the clause: {refusal}"
    );

    // A follower whose copy levelled a moment ago answers here, inside a bound
    // three floors wide — the same comparison the staleness suite pins.
    session.run_now("DEFINE NODE ROLES serving;").unwrap();
    level(&store, 1);
    let mut reading = reader(&store).among(peer(false, Duration::ZERO));
    let outcomes = reading
        .run_now(&format!("SELECT * FROM orders STALE WITHIN {};", allowed()))
        .expect("a copy level a moment ago is inside a bound three floors wide");
    assert_eq!(outcomes.len(), 1, "and it is answered here, not redirected");
}

#[test]
fn the_snapshot_level_answers_on_a_node_holding_everything() {
    // On a full holder the statement's own committed point already satisfies
    // the level: the marker changes nothing, which is the point.
    let store = store();
    let mut session = ready(&store);

    let outcomes = session
        .run_now("SELECT * FROM orders SNAPSHOT;")
        .expect("one node holding the whole table is one point");

    assert_eq!(outcomes.len(), 1);
}

#[test]
fn the_leader_level_beside_answered_by_any_is_refused() {
    // The level demands the one node that decides writes; the clause admits
    // any copy. There is no reading of the pair that is not a guess, and the
    // guess would fail in the unsafe direction. The level sits at the
    // statement's tail, after the clauses it can disagree with.
    let store = store();
    let mut session = ready(&store);

    let refusal = session
        .run_now("SELECT * FROM orders ANSWERED BY ANY LEADER;")
        .unwrap_err();

    let message = refusal.to_string();
    assert!(
        message.contains("ANY"),
        "the refusal echoes the clause that contradicts the level: {message}"
    );
    assert!(
        message.contains("LEADER"),
        "and it names the level the clause contradicts: {message}"
    );
}

#[test]
fn the_leader_level_beside_answered_by_leader_agrees() {
    // Redundant but agreeing: both name the authority axis, and agreement is
    // not a contradiction. The statement says the same thing twice.
    let store = store();
    let mut session = ready(&store);

    session
        .run_now("SELECT * FROM orders ANSWERED BY LEADER LEADER;")
        .expect("two names for one axis, said in agreement");
}

#[test]
fn the_stale_level_beside_answered_by_leader_is_legal() {
    // The two axes stay independent under a level: authority and freshness
    // compose exactly as the two clauses always have.
    let store = store();
    let mut session = ready(&store);

    session
        .run_now(&format!(
            "SELECT * FROM orders ANSWERED BY LEADER STALE WITHIN {};",
            allowed()
        ))
        .expect("the level that names freshness composes with authority");
}

#[test]
fn a_level_beside_a_version_is_refused() {
    // `VERSION` names one exact point in history. A level beside it is a
    // second answer to a question already answered — for `SNAPSHOT` literally,
    // and for the others because the level's contract has no room left.
    let store = store();
    let mut session = ready(&store);

    for read in [
        "SELECT * FROM orders SNAPSHOT VERSION 1;",
        "SELECT * FROM orders LEADER VERSION 1;",
        "SELECT * FROM orders STALE WITHIN 30s VERSION 1;",
    ] {
        let refusal = session.run_now(read).unwrap_err();
        assert!(
            refusal.to_string().contains("VERSION"),
            "{read}: the refusal says which two ideas disagree: {refusal}"
        );
    }
}

#[test]
fn the_stale_level_needs_its_span() {
    // No stored default exists: a level without its bound would mean a
    // tolerance the statement never wrote down.
    let store = store();
    let mut session = ready(&store);

    for read in [
        "SELECT * FROM orders STALE;",
        "SELECT * FROM orders STALE 30s;",
    ] {
        let refusal = session.run_now(read).unwrap_err();
        assert!(
            refusal.to_string().contains("WITHIN"),
            "{read}: the refusal names what would make it acceptable: {refusal}"
        );
    }
}

#[test]
fn the_stale_level_beside_staleness_is_refused() {
    // The level carries its own bound; a second bound beside it is two
    // answers to one question. (`STALENESS` may still refine the levels that
    // carry no bound of their own — `LEADER`, `SNAPSHOT` — exactly as it
    // always has; the level comes after it at the statement's tail.)
    let store = store();
    let mut session = ready(&store);

    let refusal = session
        .run_now(&format!(
            "SELECT * FROM orders STALENESS 1m STALE WITHIN {};",
            allowed()
        ))
        .unwrap_err();
    assert!(
        refusal.to_string().contains("STALENESS"),
        "the refusal names the clause that repeats the bound: {refusal}"
    );

    session
        .run_now(&format!(
            "SELECT * FROM orders STALENESS {} LEADER;",
            allowed()
        ))
        .expect("a bound refines a level that carries none, as it always has");
    session
        .run_now(&format!(
            "SELECT * FROM orders STALENESS {} SNAPSHOT;",
            allowed()
        ))
        .expect("and the same beside the snapshot level");
}

#[test]
fn an_unknown_level_word_is_refused_where_the_statement_is_read() {
    // A word at the level's place that names no level refuses rather than
    // defaulting — the direction a guess would fail in is the unsafe one.
    let store = store();
    let mut session = ready(&store);

    let refusal = session.run_now("SELECT * FROM orders FROZEN;").unwrap_err();
    assert!(
        refusal.to_string().contains("FROZEN"),
        "the refusal echoes what was written: {refusal}"
    );
}

#[test]
fn the_snapshot_level_on_a_partial_holder_is_refused_not_gathered() {
    // The one place the snapshot promise cannot be kept: a gathered part would
    // be another node's present, so the level refuses exactly where a
    // transaction or `VERSION` read refuses — never answered as a mix of two
    // points.
    let pair = pair();
    let mut follower = pair.on_the_follower("reader");

    let error = refused_on_the_pair(&mut follower, "SELECT * FROM ledger SNAPSHOT;");
    let Error::NotHeldHere { table, shards, .. } = &error else {
        panic!("the level was answered weaker than it was named: {error}");
    };
    assert_eq!(table, "ledger");
    assert_eq!(
        shards,
        &[1, 3],
        "the shards this node does not hold, by name"
    );

    // The control: the same statement without the level gathers, as it always
    // has — the level, not the table, is what refused.
    let mut again = pair.on_the_follower("reader");
    let (records, _) = answer(&mut again, "SELECT * FROM ledger;");
    assert!(!records.is_empty(), "gathering itself is untouched");
}

#[test]
fn the_snapshot_level_inside_a_transaction_is_legal() {
    // A transaction is one snapshot already; the level restates the promise
    // rather than naming a second point (which is what `VERSION` inside one
    // refuses for).
    let store = store();
    let mut session = ready(&store);

    session
        .run_now("BEGIN; SELECT * FROM orders SNAPSHOT; COMMIT;")
        .expect("a promise restated is not a contradiction");
}

#[test]
fn a_field_called_leader_is_still_a_field() {
    // The level words are contextual, the way `staleness` is: they open a
    // level only at the statement's tail, and read as names everywhere a name
    // can sit.
    let store = store();
    let mut session = Session::new(&store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE tickets SCHEMALESS;\n\
             CREATE tickets:1 = { leader: true, snapshot: 'yes', stale: 1 };",
        )
        .unwrap();

    session
        .run_now("SELECT leader, snapshot, stale FROM tickets;")
        .expect("a word that opens a level only in a level's place");
    session
        .run_now("SELECT * FROM tickets WHERE leader = true AND stale > 0;")
        .expect("and in a condition the words are the fields they always were");
}

#[test]
fn a_bounded_read_answered_from_a_copy_states_its_age() {
    // The lag stated: the bound is the guarantee, and the note says what the
    // copy actually was — information, so a caller that wants to know what it
    // got does not have to measure it.
    let store = store();
    let mut session = ready(&store).among(peer(false, Duration::ZERO));
    session.run_now("DEFINE NODE ROLES serving;").unwrap();
    level(&store, 1);
    // A beat of real time, so the age the note states is unambiguously not a
    // zero. The assertion is presence and non-zero, never the exact figure —
    // the note's value is time since the copy levelled, which no test can
    // pin to the nanosecond.
    std::thread::sleep(Duration::from_millis(2));

    let (records, notes) = answer(
        &mut session,
        &format!("SELECT * FROM orders STALENESS {};", allowed()),
    );
    assert_eq!(records.len(), 1, "answered here, inside the bound");
    let Some(note) = notes.iter().find(|note| note.kind() == "answered-as-of") else {
        panic!("the lag was not stated: {notes:?}");
    };
    let Note::AnsweredAsOf { age } = note else {
        panic!("not the lag note: {note:?}");
    };
    assert_ne!(
        age.to_literal(),
        "0s",
        "the note states a real age, not a placeholder: {note:?}"
    );
}

#[test]
fn the_stale_level_states_its_lag_too() {
    // The level desugars onto the bound at the statement, so its locally
    // served answer carries the same note — one contract, two spellings.
    let store = store();
    let mut session = ready(&store).among(peer(false, Duration::ZERO));
    session.run_now("DEFINE NODE ROLES serving;").unwrap();
    level(&store, 1);
    std::thread::sleep(Duration::from_millis(2));

    let (_, notes) = answer(
        &mut session,
        &format!("SELECT * FROM orders STALE WITHIN {};", allowed()),
    );
    assert!(
        notes.iter().any(|note| note.kind() == "answered-as-of"),
        "the level's answer states its lag: {notes:?}"
    );
}

#[test]
fn a_bounded_read_answered_by_the_leader_carries_no_age_note() {
    // A writable node answers as of now — there is no copy age to state, and
    // saying "0s" on every bounded read would teach readers to skip the note.
    let store = store();
    let mut session = ready(&store);

    let (_, notes) = answer(
        &mut session,
        &format!("SELECT * FROM orders STALENESS {};", allowed()),
    );
    assert!(
        notes.iter().all(|note| note.kind() != "answered-as-of"),
        "a current answer has no lag to state: {notes:?}"
    );
}

#[test]
fn an_unbounded_read_carries_no_age_note() {
    // The note exists only where the caller accepted a bounded answer. The
    // default contract states nothing, so its answers carry nothing new.
    let store = store();
    let mut session = ready(&store).among(peer(false, Duration::ZERO));
    session.run_now("DEFINE NODE ROLES serving;").unwrap();
    level(&store, 1);

    let (_, notes) = answer(&mut session, "SELECT * FROM orders;");
    assert!(
        notes.iter().all(|note| note.kind() != "answered-as-of"),
        "no bound was named, so no lag is stated: {notes:?}"
    );
}

#[test]
fn a_lagging_follower_answers_each_level_per_its_contract() {
    // The cluster shape of C2: a follower whose copy is genuinely behind,
    // asked at each level.
    let pair = pair();
    // The leader writes into the shard the follower holds, after the follower
    // collected: 'm' exists on the leader and is absent from the follower's
    // copy, which is the fact every arm below answers from.
    let mut leader = pair.on_the_leader("root");
    leader
        .run_now("CREATE ledger:'m' = { total: 4, note: 'm' };")
        .unwrap();

    // The follower is drained of the writable role and marked level at the
    // fixture's point: a serving copy with a known age.
    let mut draining = pair.on_the_follower("root");
    draining.run_now("DEFINE NODE ROLES serving;").unwrap();
    level(pair.follower(), 1);

    // `STALE WITHIN` on the window the follower holds: answered from the copy
    // — 'm' absent, the note states the lag. The window stays inside the one
    // shard it holds, so nothing is gathered.
    let bound = format!("{}s", STALENESS_FLOOR_SECONDS.saturating_mul(60));
    let (records, notes) = answer(
        &mut pair.on_the_follower("reader"),
        &format!("SELECT * FROM ledger:'h'..'n' STALE WITHIN {bound};"),
    );
    assert!(
        records.iter().any(|(id, _)| *id == RecordId::from("h")),
        "the copy's records answer: {records:?}"
    );
    assert!(
        !records.iter().any(|(id, _)| *id == RecordId::from("m")),
        "the answer is what the copy holds, and the copy predates 'm'"
    );
    assert!(
        notes.iter().any(|note| note.kind() == "answered-as-of"),
        "the lag is stated with the answer: {notes:?}"
    );

    // `LEADER` on the same window: the follower may not answer, and this
    // fixture knows no directory, so the refusal names the remedy — exactly
    // the discipline the axis has always had.
    let error = refused_on_the_pair(
        &mut pair.on_the_follower("reader"),
        "SELECT * FROM ledger:'h'..'n' LEADER;",
    );
    assert!(
        matches!(error, Error::NoLeaderKnown { .. }),
        "never answered weaker than it was named: {error}"
    );

    // `SNAPSHOT` on the same window: the follower's own committed tail is one
    // point, so the level answers here — still what the copy holds.
    let (records, _) = answer(
        &mut pair.on_the_follower("reader"),
        "SELECT * FROM ledger:'h'..'n' SNAPSHOT;",
    );
    assert!(records.iter().any(|(id, _)| *id == RecordId::from("h")));
    assert!(!records.iter().any(|(id, _)| *id == RecordId::from("m")));

    // The control that the copy is genuinely behind: the same window on the
    // leader — the node the 'm' write landed on — includes it. (The follower's
    // held shard is never gathered from, so a whole-table read could not show
    // this; the same window on the node that holds the write does.)
    let (records, _) = answer(
        &mut pair.on_the_leader("reader"),
        "SELECT * FROM ledger:'h'..'n';",
    );
    assert!(
        records.iter().any(|(id, _)| *id == RecordId::from("m")),
        "the leader's present holds 'm': {records:?}"
    );
}

#[test]
fn a_bounded_read_served_from_a_replica_is_single_digit_milliseconds() {
    // The measured shape of a local read: a bounded read served by a follower
    // copy answers in single-digit milliseconds. The figure prints beside the
    // build and the shape — a measurement, not an assertion the reader has to
    // take on faith; the assert below is the criterion's bar with the headroom
    // the in-memory shape deserves, and a regression that crosses it is a real
    // one. Same-host processes stand in for same-DC: the serving path being
    // measured is the local one, and the deployment's regions are the honest
    // same-DC story.
    let pair = pair();
    let mut draining = pair.on_the_follower("root");
    draining.run_now("DEFINE NODE ROLES serving;").unwrap();
    level(pair.follower(), 1);

    let span = STALENESS_FLOOR_SECONDS.saturating_mul(60);
    let read = format!("SELECT * FROM ledger:'h'..'n' STALE WITHIN {span}s;");
    let mut session = pair.on_the_follower("reader");

    // Warmup: parsing, caches, and the steady state a measurement wants.
    for _ in 0..50 {
        session.run_now(&read).unwrap();
    }
    let mut samples = Vec::with_capacity(200);
    for _ in 0..200 {
        let started = std::time::Instant::now();
        session.run_now(&read).unwrap();
        samples.push(started.elapsed());
    }
    samples.sort_unstable();
    let p50 = samples
        .get(samples.len() / 2)
        .copied()
        .unwrap_or(Duration::ZERO);
    println!(
        "bounded read served from a follower copy: p50 {p50:?} over {} reads \
         (engine {}, ledger split in 3, window 'h'..'n' inside the held shard, \
         ~8 records, STALE WITHIN {span}s, same-process pair — same-host stands \
         in for same-DC)",
        samples.len(),
        env!("CARGO_PKG_VERSION"),
    );
    assert!(
        p50 < Duration::from_millis(10),
        "single-digit milliseconds at the stated shape: p50 {p50:?}, \
         worst {:?}",
        samples.last().copied().unwrap_or(Duration::ZERO),
    );
}
