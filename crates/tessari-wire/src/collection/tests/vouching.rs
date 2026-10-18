//! What an answer's order vouches for (R4).
//!
//! The order is this node's own commit counter, and a follower holds back every
//! record above it. Stated for a line some other node wrote, it bounds that
//! writer's records by a counter that is not theirs: a candidate catching up
//! from a holder (Q-897) was handed the record it was behind on and never
//! applied it.

use super::*;
use crate::collection::{Collect, Origin, Serving};
use tessari_storage::{LEASE_TTL, Lease, LogId};
use tessari_types::Reach;

/// A node holding the store line at epoch 5 that wrote `other`'s database
/// under it, its member row for `THERE` ending in `clause`; and that log.
fn written_under_the_store_line(clause: &str) -> (Arc<Db>, LogId) {
    let db = Arc::new(Db::in_memory().now().expect("an in-memory store"));
    db.store()
        .hold(Epoch::new(5), Lease::taken(LEASE_TTL))
        .now();
    db.session()
        .run_now(
            "DEFINE NAMESPACE other; USE NAMESPACE other; DEFINE DATABASE ledger; \
             USE DATABASE ledger; DEFINE COLLECTION secrets; \
             CREATE secrets:1 = { word: 'shibboleth' };",
        )
        .expect("the leader's own statements run");
    db.session()
        .run_now(&format!(
            "DEFINE REPLICA follower AT '127.0.0.1:1' NODE '{}'{clause};",
            spelled(THERE)
        ))
        .expect("the member row commits");
    let log = db
        .store()
        .logs()
        .now()
        .expect("the logs")
        .into_iter()
        .find(|log| matches!(log.home, Reach::Database(..)))
        .expect("the database's log");
    (db, log)
}

fn served(db: &Db, log: LogId) -> Collected {
    Serving::declared(db.store())
        .collected(
            THERE,
            Collect {
                home: log.home,
                from: Sequence::new(1),
                limit: 1024,
            },
        )
        .now()
        .expect("the follower is subscribed")
}

#[test]
fn an_answer_for_a_line_this_node_does_not_lead_states_no_order() {
    // `other` is placed on THERE after this node wrote it, so the line is no
    // longer this node's to write: what it holds of it, it serves as a holder.
    let (db, log) = written_under_the_store_line(" REPLICATES STORE LEADS NAMESPACE other");
    let answer = served(&db, log);
    assert!(
        !answer.records.is_empty(),
        "the holder still serves what it holds"
    );
    assert_eq!(answer.epoch, None, "it leads no line governing that home");
    assert_eq!(
        answer.order, None,
        "an order this node did not write the line under"
    );
}

#[test]
fn an_answer_for_a_line_this_node_leads_states_its_order() {
    // The control: the same writes, nothing placed, so this node leads the
    // line it serves and its order is the one the records were written under.
    let (db, log) = written_under_the_store_line(" REPLICATES STORE");
    let answer = served(&db, log);
    assert_eq!(answer.epoch, Some(Epoch::new(5)));
    assert!(answer.order.is_some(), "the leader's order bounds its line");
}
