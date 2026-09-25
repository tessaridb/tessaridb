//! A node granted a higher epoch writes over the leadership it replaced — Q-605.
//!
//! The sibling of [`super::two_leaders`] and a different arrangement. There, two
//! nodes lead two ranges at the same time and each refuses the other; here one
//! range changes hands, and the question is whether the row describing the
//! **old** leader may stop the new one.
//!
//! # The latch this exists to prevent
//!
//! Recording a leadership is a write, so it meets the range gate like any other.
//! A gate that refuses on the node alone therefore refuses the very row that
//! would replace it: the first election in a cluster's life succeeds because no
//! row exists yet, and every one after it is un-recordable, permanently. It was
//! measured in a three-process run before it was fixed — a survivor won epoch 18
//! ten seconds after the leader was killed, could not record it against the dead
//! leader's epoch-15 row, and re-stood every six seconds up to epoch 36 while the
//! cluster had no recorded leader at all.
//!
//! # Why a higher epoch is allowed to win and an equal one is not
//!
//! A voter grants an epoch at most once and a round concludes only on a strict
//! majority, so two nodes cannot hold the same epoch. A row naming somebody else
//! at the epoch this node holds is a catalog disagreeing with itself, and the
//! safe reading of that is the refusal — which is why the equal and the absent
//! cases are asserted here beside the one that changed.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Instant;

use tessari_encoding::{NODE_ID_LEN, Roles};
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_storage::{Catalog, Error, LEASE_TTL, Lease, Reach, RecordAddress, Store};
use tessari_types::{DatabaseId, Epoch, NamespaceId, RecordId, TableId};

const OLD_NODE: [u8; NODE_ID_LEN] = [9; NODE_ID_LEN];
const OLD_ENDPOINT: &str = "10.0.0.2:9081";
/// The epoch the departed leader was recorded under.
const OLD_EPOCH: u64 = 15;
/// The epoch a majority granted this node afterwards.
const NEW_EPOCH: u64 = 18;

/// A clustered store whose catalog says `OLD_NODE` leads the whole store under
/// `OLD_EPOCH`, holding whatever epoch `granted` names.
///
/// `None` is the node nobody elected, and it is a different statement from
/// `Some(Epoch::ZERO)`.
fn after_a_failover(granted: Option<u64>) -> Store {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    catalog
        .create_replica(
            "departed",
            OLD_ENDPOINT,
            Roles::SERVING.and(Roles::WRITABLE).and(Roles::COORDINATING),
            Some(OLD_NODE),
            None,
            None,
        )
        .unwrap();
    catalog
        .record_leadership(Reach::Store, OLD_NODE, Epoch::new(OLD_EPOCH))
        .unwrap();
    transaction.commit().unwrap();
    if let Some(epoch) = granted {
        store.hold(
            Epoch::new(epoch),
            Lease::taken_at(Instant::now(), LEASE_TTL),
        );
    }
    store
}

fn write(store: &Store, id: &str) -> Result<(), Error> {
    let mut transaction = store.begin()?;
    transaction.put(
        RecordAddress::new(
            NamespaceId::new(1),
            DatabaseId::new(1),
            TableId::new(1),
            RecordId::Text(id.to_owned()),
        ),
        b"{}".to_vec(),
    );
    transaction.commit().map(|_| ())
}

#[test]
fn a_node_holding_a_greater_epoch_writes_a_range_the_log_says_another_node_leads() {
    write(&after_a_failover(Some(NEW_EPOCH)), "after the failover").unwrap();
}

#[test]
fn a_node_holding_a_greater_epoch_can_record_the_leadership_it_won() {
    // The exact call the instrumented run watched fail, once every six seconds,
    // for twenty-one consecutive epochs.
    let store = after_a_failover(Some(NEW_EPOCH));
    let me = store.node_identity().unwrap().id;
    let mut transaction = store.begin().unwrap();
    Catalog::new(&mut transaction)
        .record_leadership(Reach::Store, me, Epoch::new(NEW_EPOCH))
        .unwrap();
    transaction.commit().unwrap();

    let mut transaction = store.begin().unwrap();
    let recorded = Catalog::new(&mut transaction)
        .leader_of(Reach::Store)
        .unwrap()
        .expect("the store has a leadership row");
    assert_eq!(
        (recorded.node, recorded.epoch),
        (me, Epoch::new(NEW_EPOCH)),
        "the winner recorded its leadership and the log still names the node it \
         replaced"
    );
}

#[test]
fn a_node_holding_an_equal_epoch_is_still_refused() {
    // Two nodes cannot hold one epoch, so this is a catalog that disagrees with
    // itself rather than a succession. It refuses.
    let refused = write(&after_a_failover(Some(OLD_EPOCH)), "same epoch").unwrap_err();
    assert!(
        matches!(refused, Error::WriteIsElsewhere { node, epoch, .. }
            if node == OLD_NODE && epoch == Epoch::new(OLD_EPOCH)),
        "a node holding the recorded leadership's own epoch was allowed past the \
         range gate: {refused:?}"
    );
}

#[test]
fn a_node_holding_no_epoch_is_still_refused() {
    // The state every redirect to a known leader is served from: this node was
    // granted nothing, so it has nothing to supersede the row with.
    let refused = write(&after_a_failover(None), "never elected").unwrap_err();
    assert!(
        matches!(refused, Error::WriteIsElsewhere { node, .. } if node == OLD_NODE),
        "a node nobody elected was allowed past the range gate: {refused:?}"
    );
}
