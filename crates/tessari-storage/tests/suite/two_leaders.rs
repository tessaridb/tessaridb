//! Two nodes, two namespaces, and each refuses the other's write — G025 S6.1.
//!
//! This is the goal's **kill criterion**, and it is posed here rather than at
//! the end because a kill criterion checked at the end is decoration. The goal
//! it tests is *two masters with different data*: partitioned multi-leader, in
//! which two leaders never lead the same range and therefore need no rule for
//! resolving a conflict between them. If it turns out they do need one, that
//! variant was never distinct from true multi-master on one dataset and the
//! goal folds into the deferred one instead of being rescued.
//!
//! # What was wrong before this module existed
//!
//! The write gate was store-wide. `Store::awaiting` asks the in-memory lease and
//! then the membership catalog, and **neither question mentions the range being
//! written**. A node that had been granted *a* leadership therefore wrote
//! *everything*, including a namespace another node leads, with no error and no
//! log line. It was invisible because no test had ever put two leaderships in
//! one catalog, and one lease over one store is a shape in which the question
//! cannot come up.
//!
//! # The scenario is two stores and no socket
//!
//! Each node is a `Store` on its own backend, and the arrangement they share is
//! written into each one's catalog the way an applied log record would leave it:
//! a membership row for the peer and a leadership row per namespace. That is a
//! stronger form of *the peer link is down* than cutting a live one — there is
//! no link in this module at all — and it is the same construction S1.2 used.
//!
//! # The refusal carries three fields and the third is what makes it checkable
//!
//! Endpoint, node and epoch. A redirect naming only a place cannot be verified
//! on arrival, and a client that dialled it and met a different node would have
//! no way to notice; a redirect carrying no epoch cannot be refused by a client
//! that has already been told about a newer leadership.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Instant;

use tessari_encoding::{NODE_ID_LEN, Roles};
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_storage::{Catalog, Error, LEASE_TTL, Lease, Reach, RecordAddress, Store};
use tessari_types::{DatabaseId, Epoch, NamespaceId, RecordId, ReplicationClass, TableId};

/// The namespace the store under test leads.
const MINE: u32 = 1;
/// The namespace the other node leads.
const THEIRS: u32 = 2;

const THEIR_NODE: [u8; NODE_ID_LEN] = [9; NODE_ID_LEN];
const THEIR_ENDPOINT: &str = "10.0.0.2:9081";
const MY_EPOCH: u64 = 4;
const THEIR_EPOCH: u64 = 7;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// A store that leads `Namespace(MINE)` beside a peer that leads
/// `Namespace(THEIRS)`.
///
/// The whole arrangement is declared in **one** transaction, which is not a
/// convenience: ADR-0069 made the first committed membership row put the node in
/// a cluster, so a second statement would be judged against a catalog that had
/// already fenced it.
fn between_two_leaders(leading: Reach, followed: Reach, their_node: [u8; NODE_ID_LEN]) -> Store {
    let store = store();
    let me = store.node_identity().unwrap().id;
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    catalog
        .create_replica(
            "other",
            THEIR_ENDPOINT,
            Roles::SERVING.and(Roles::WRITABLE).and(Roles::COORDINATING),
            Some(their_node),
            None,
        )
        .unwrap();
    catalog
        .record_leadership(leading, me, Epoch::new(MY_EPOCH))
        .unwrap();
    catalog
        .record_leadership(followed, their_node, Epoch::new(THEIR_EPOCH))
        .unwrap();
    transaction.commit().unwrap();
    store.hold(
        Epoch::new(MY_EPOCH),
        Lease::taken_at(Instant::now(), LEASE_TTL),
    );
    store
}

/// The node of the two-leader arrangement, leading `MINE`.
fn one_of_two() -> Store {
    between_two_leaders(
        Reach::Namespace(NamespaceId::new(MINE)),
        Reach::Namespace(NamespaceId::new(THEIRS)),
        THEIR_NODE,
    )
}

fn at(namespace: u32, id: &str) -> RecordAddress {
    RecordAddress::new(
        NamespaceId::new(namespace),
        DatabaseId::new(1),
        TableId::new(1),
        RecordId::Text(id.to_owned()),
    )
}

fn write(store: &Store, namespace: u32, id: &str) -> Result<(), Error> {
    let mut transaction = store.begin()?;
    transaction.put(at(namespace, id), b"{}".to_vec());
    transaction.commit().map(|_| ())
}

#[test]
fn a_node_writes_the_namespace_it_leads() {
    write(&one_of_two(), MINE, "ours").unwrap();
}

#[test]
fn a_node_refuses_a_write_into_the_namespace_another_node_leads() {
    let refused = write(&one_of_two(), THEIRS, "theirs").unwrap_err();
    assert!(
        matches!(refused, Error::WriteIsElsewhere { .. }),
        "a write into a namespace another node leads was not refused as a \
         redirect: {refused:?}"
    );
}

#[test]
fn the_refusal_names_the_other_leaders_endpoint_and_epoch() {
    // The criterion's own assertion. A refusal that merely says no leaves the
    // client with nowhere to go, and this goal's whole claim is that a write
    // arriving at the wrong leader is a routing answer rather than a failure.
    let Error::WriteIsElsewhere {
        endpoint,
        node,
        epoch,
    } = write(&one_of_two(), THEIRS, "theirs").unwrap_err()
    else {
        panic!("expected a redirect");
    };
    assert_eq!(endpoint, THEIR_ENDPOINT);
    assert_eq!(node, THEIR_NODE);
    assert_eq!(epoch, Epoch::new(THEIR_EPOCH));
}

#[test]
fn the_refusal_carries_the_other_leaders_epoch_and_never_this_nodes_own() {
    // Separated from the assertion above because the two numbers differ here on
    // purpose: a redirect built from this node's own leadership would still pass
    // a test whose fixture gave both nodes the same epoch, and would date every
    // redirect with a number the target never published.
    let Error::WriteIsElsewhere { epoch, .. } = write(&one_of_two(), THEIRS, "theirs").unwrap_err()
    else {
        panic!("expected a redirect");
    };
    assert_ne!(epoch, Epoch::new(MY_EPOCH));
}

#[test]
fn the_two_leaders_refuse_each_other_symmetrically() {
    // A rule that works one way round is a coincidence until it works the other.
    // The second node is built by swapping the two ranges, so it leads THEIRS
    // and follows MINE.
    let theirs = between_two_leaders(
        Reach::Namespace(NamespaceId::new(THEIRS)),
        Reach::Namespace(NamespaceId::new(MINE)),
        THEIR_NODE,
    );
    write(&theirs, THEIRS, "ours").unwrap();
    let refused = write(&theirs, MINE, "not ours").unwrap_err();
    assert!(
        matches!(refused, Error::WriteIsElsewhere { node, .. } if node == THEIR_NODE),
        "the mirrored node did not refuse-and-redirect: {refused:?}"
    );
}

#[test]
fn a_leadership_over_the_whole_store_still_lets_its_holder_write_everywhere() {
    // The row every deployment that exists today would carry, because
    // `Reach::Store` is the only range anything has ever recorded. `contains`
    // must keep answering yes for every namespace under it, or this wave breaks
    // every single-leader cluster in the field.
    let store = store();
    let me = store.node_identity().unwrap().id;
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    catalog
        .create_replica(
            "other",
            THEIR_ENDPOINT,
            Roles::SERVING,
            Some(THEIR_NODE),
            None,
        )
        .unwrap();
    catalog
        .record_leadership(Reach::Store, me, Epoch::new(MY_EPOCH))
        .unwrap();
    transaction.commit().unwrap();
    store.hold(
        Epoch::new(MY_EPOCH),
        Lease::taken_at(Instant::now(), LEASE_TTL),
    );
    write(&store, MINE, "one").unwrap();
    write(&store, THEIRS, "two").unwrap();
}

#[test]
fn a_store_with_no_leadership_row_writes_exactly_as_it_did_before() {
    // The standalone path, and the one every existing test in this workspace
    // runs on. An empty leadership table must resolve to *nobody leads this*,
    // which falls through to the admission predicate ADR-0069 left in place.
    write(&store(), MINE, "alone").unwrap();
}

#[test]
fn a_clustered_node_with_no_leadership_anywhere_still_meets_the_older_refusal() {
    // The two refusals are different sentences and both must survive. This node
    // is in a cluster, holds no lease and no range is led by anyone: the honest
    // answer is *wait*, not *go there*, because there is nowhere to send it.
    let store = store();
    let mut transaction = store.begin().unwrap();
    Catalog::new(&mut transaction)
        .create_replica(
            "other",
            THEIR_ENDPOINT,
            Roles::SERVING.and(Roles::WRITABLE),
            Some(THEIR_NODE),
            None,
        )
        .unwrap();
    transaction.commit().unwrap();
    let refused = write(&store, MINE, "nobody leads this").unwrap_err();
    assert!(
        matches!(refused, Error::NoLeadershipYet),
        "a clustered node with no leadership anywhere answered something other \
         than the older refusal: {refused:?}"
    );
}

#[test]
fn a_rehearsal_meets_the_redirect_the_commit_would() {
    // `VERIFY` runs every check a commit runs and discards the work. A fence a
    // rehearsal cannot see is one an operator meets for the first time in
    // production, which is the argument the lease fence already makes.
    let store = one_of_two();
    let mut transaction = store.begin().unwrap();
    transaction.put(at(THEIRS, "theirs"), b"{}".to_vec());
    let refused = transaction.dry_run().unwrap_err();
    assert!(
        matches!(refused, Error::WriteIsElsewhere { .. }),
        "a rehearsal did not meet the redirect: {refused:?}"
    );
}

#[test]
fn a_clustered_node_with_no_lease_is_redirected_rather_than_told_to_wait() {
    // The case that decides the ORDER of the two questions. This node is in a
    // cluster, holds no lease at all, and another node leads the range: both
    // refusals are true of it, and only one is useful. `NoLeadershipYet` tells a
    // client to wait for a round that may never concern it; the redirect names
    // the node that can take the write now.
    //
    // The range question therefore goes first. It is also the reason it cannot
    // simply go last: `Store::awaiting` returns early on a live lease, so a
    // question asked after it never reaches a leader at all.
    let store = store();
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    catalog
        .create_replica(
            "other",
            THEIR_ENDPOINT,
            Roles::SERVING.and(Roles::WRITABLE).and(Roles::COORDINATING),
            Some(THEIR_NODE),
            None,
        )
        .unwrap();
    catalog
        .record_leadership(
            Reach::Namespace(NamespaceId::new(MINE)),
            THEIR_NODE,
            Epoch::new(THEIR_EPOCH),
        )
        .unwrap();
    transaction.commit().unwrap();
    // No `hold`: this node was granted nothing.
    let refused = write(&store, MINE, "not mine either").unwrap_err();
    assert!(
        matches!(refused, Error::WriteIsElsewhere { node, .. } if node == THEIR_NODE),
        "a clustered node with no lease and a known leader elsewhere was told to \
         wait instead of where to go: {refused:?}"
    );
}

/// G025 S6.2, the epoch half — a leadership row is judged by the range it
/// describes, not by the tenancy it is stored in.
///
/// Q-597, and it was the wall S6.2 walked into rather than a defect anybody had
/// met: nothing narrowed a leadership from `Reach::Store` until S6.1, so a node
/// under somebody else's store-wide leadership had never had its own to record.
///
/// The shape is worth stating because it is silent. `Db::record_leadership`
/// commits into the system tenancy, so by ADDRESS the row is a write into
/// `Reach::Database(0, 0)` — which a store-wide leadership covers. A node that
/// has just won a round for its own namespace would therefore be refused
/// permission to write down the leadership a majority granted it. `Store::hold`
/// installs the lease and cannot fail, so the node believes it leads; the log
/// never learns, so every other node goes on routing that namespace's writes
/// elsewhere. Nothing is in an error state anywhere.
#[test]
fn a_node_under_somebody_elses_store_wide_leadership_can_record_its_own() {
    let store = between_two_leaders(
        Reach::Namespace(NamespaceId::new(MINE)),
        Reach::Store,
        THEIR_NODE,
    );
    let me = store.node_identity().unwrap().id;

    // A SECOND transaction, and that is the whole test. The fixture wrote both
    // rows in one commit, before the first membership row had fenced anything;
    // this is the write a node takes after a majority has granted it a round,
    // which is the one that has to pass the gate.
    let mut transaction = store.begin().unwrap();
    Catalog::new(&mut transaction)
        .record_leadership(
            Reach::Namespace(NamespaceId::new(MINE)),
            me,
            Epoch::new(MY_EPOCH + 1),
        )
        .unwrap();
    transaction
        .commit()
        .expect("a node cannot be refused permission to record a leadership it holds");

    let mut transaction = store.begin().unwrap();
    let held = Catalog::new(&mut transaction).leaderships().unwrap();
    let mine = held
        .iter()
        .find(|row| row.range == Reach::Namespace(NamespaceId::new(MINE)))
        .expect("this node's own leadership row");
    assert_eq!(mine.node, me);
    assert_eq!(mine.epoch, Epoch::new(MY_EPOCH + 1));
}

/// The exemption is a change of question, not a bypass.
///
/// Judging the row by what it describes must not let a node write a leadership
/// for a range somebody else leads — which is the failure the other way out of
/// Q-597 would have had, exempting the system tenancy wholesale and letting any
/// node with any lease write any system row, another node's membership included.
#[test]
fn a_node_cannot_record_a_leadership_over_a_range_another_node_leads() {
    let store = one_of_two();
    let me = store.node_identity().unwrap().id;

    let mut transaction = store.begin().unwrap();
    Catalog::new(&mut transaction)
        .record_leadership(
            Reach::Namespace(NamespaceId::new(THEIRS)),
            me,
            Epoch::new(MY_EPOCH + 1),
        )
        .unwrap();
    let refused = transaction
        .commit()
        .expect_err("a node claimed a leadership over a range it does not lead");
    assert!(
        matches!(refused, Error::WriteIsElsewhere { node, .. } if node == THEIR_NODE),
        "{refused}"
    );
}

/// G025 S6.2 — two ranges carry their own epochs, and one advancing leaves the
/// other where it was.
///
/// The epoch was per-range in the record from `LeadershipDefinition`'s first
/// version; what had never been asserted is the consequence, which is that the
/// two numbers are independent. A store-wide epoch would make this test read the
/// same value twice.
#[test]
fn two_ranges_advance_their_epochs_independently() {
    let store = one_of_two();
    let me = store.node_identity().unwrap().id;

    let epochs = |store: &Store| {
        let mut transaction = store.begin().unwrap();
        let held = Catalog::new(&mut transaction).leaderships().unwrap();
        let of = |range: Reach| {
            held.iter()
                .find(|row| row.range == range)
                .map(|row| row.epoch.get())
        };
        (
            of(Reach::Namespace(NamespaceId::new(MINE))),
            of(Reach::Namespace(NamespaceId::new(THEIRS))),
        )
    };

    assert_eq!(epochs(&store), (Some(MY_EPOCH), Some(THEIR_EPOCH)));

    let mut transaction = store.begin().unwrap();
    Catalog::new(&mut transaction)
        .record_leadership(
            Reach::Namespace(NamespaceId::new(MINE)),
            me,
            Epoch::new(MY_EPOCH + 3),
        )
        .unwrap();
    transaction.commit().unwrap();

    // The one this node leads moved by three; the one it does not is untouched,
    // and it is still the LOWER of the two having started as the higher — which
    // a single store-wide counter could not produce.
    assert_eq!(epochs(&store), (Some(MY_EPOCH + 3), Some(THEIR_EPOCH)));
}

/// A clustered node holding **no lease**, with a namespace declared under
/// `class` and optionally already led by `led_by`.
///
/// Everything in ONE transaction for the reason [`between_two_leaders`] gives:
/// ADR-0069 makes the first committed membership row put this node in a cluster,
/// so a second statement would be judged against a catalog that had already
/// fenced it — including the declaration that is meant to exempt it.
///
/// The namespace is **created** rather than named: `admits_two_writers` reads a
/// definition, and a namespace id nothing defined answers `false` whatever was
/// intended for it — which would make a declared-range test pass for the same
/// reason an undeclared one does.
fn clustered_with(
    class: Option<ReplicationClass>,
    led_by: Option<[u8; NODE_ID_LEN]>,
) -> (Store, NamespaceId, DatabaseId) {
    let store = store();
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    catalog
        .create_replica(
            "other",
            THEIR_ENDPOINT,
            Roles::SERVING.and(Roles::WRITABLE).and(Roles::COORDINATING),
            Some(THEIR_NODE),
            None,
        )
        .unwrap();
    let namespace = catalog.create_namespace("prod").unwrap();
    let database = catalog.create_database(namespace.id, "orders").unwrap();
    if let Some(class) = class {
        catalog.set_replication_class(namespace.id, class).unwrap();
    }
    if let Some(node) = led_by {
        catalog
            .record_leadership(
                Reach::Namespace(namespace.id),
                node,
                Epoch::new(THEIR_EPOCH),
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    (store, namespace.id, database.id)
}

/// One put into that range. No `hold` anywhere — this node was granted nothing.
fn write_into(
    store: &Store,
    namespace: NamespaceId,
    database: DatabaseId,
    id: &str,
) -> Result<(), Error> {
    let mut transaction = store.begin()?;
    transaction.put(
        RecordAddress::new(namespace, database, TableId::new(1), RecordId::from(id)),
        b"{}".to_vec(),
    );
    transaction.commit().map(|_| ())
}

#[test]
fn a_clustered_node_with_no_lease_refuses_an_undeclared_range_and_writes_a_declared_one() {
    // G027 S2.3, both halves in one test because the pair IS the criterion: the
    // older refusal has to survive, and the declaration has to be what lifts it.
    // Asserting only the acceptance would pass with the fence deleted.
    let (silent, namespace, database) = clustered_with(None, None);
    let refused = write_into(&silent, namespace, database, "nobody granted this").unwrap_err();
    assert!(
        matches!(refused, Error::NoLeadershipYet),
        "a clustered node with no lease wrote an undeclared range, or refused it \
         with the wrong sentence: {refused:?}"
    );

    let (declared, namespace, database) = clustered_with(Some(ReplicationClass::MultiMaster), None);
    write_into(&declared, namespace, database, "two masters means two")
        .expect("a range declared MULTI MASTER has no single leadership to wait for");
}

#[test]
fn a_clustered_node_writes_a_declared_range_another_node_already_leads() {
    // The other refusal, and the one that would otherwise make the exemption
    // above useless in a real cluster: the FIRST master's leadership row is in
    // the catalog, so a second master meets `WriteIsElsewhere` rather than
    // `NoLeadershipYet` and is redirected to the node it is supposed to be
    // writing beside.
    let (silent, namespace, database) = clustered_with(None, Some(THEIR_NODE));
    let refused = write_into(&silent, namespace, database, "theirs").unwrap_err();
    assert!(
        matches!(refused, Error::WriteIsElsewhere { node, .. } if node == THEIR_NODE),
        "an undeclared range another node leads stopped redirecting: {refused:?}"
    );

    let (declared, namespace, database) =
        clustered_with(Some(ReplicationClass::MultiMaster), Some(THEIR_NODE));
    write_into(
        &declared,
        namespace,
        database,
        "beside them, not instead of them",
    )
    .expect("a declared range has no elsewhere to be redirected to");
}

#[test]
fn a_declared_range_does_not_exempt_an_undeclared_one_written_beside_it() {
    // The hole a per-transaction exemption would open. Both ranges are written
    // in one transaction; one is declared and the other is not, and the
    // undeclared one still has a single leader that this node is not.
    let store = store();
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    catalog
        .create_replica(
            "other",
            THEIR_ENDPOINT,
            Roles::SERVING.and(Roles::WRITABLE).and(Roles::COORDINATING),
            Some(THEIR_NODE),
            None,
        )
        .unwrap();
    let open = catalog.create_namespace("prod").unwrap();
    let open_db = catalog.create_database(open.id, "orders").unwrap();
    catalog
        .set_replication_class(open.id, ReplicationClass::MultiMaster)
        .unwrap();
    let closed = catalog.create_namespace("ledger").unwrap();
    let closed_db = catalog.create_database(closed.id, "entries").unwrap();
    catalog
        .record_leadership(
            Reach::Namespace(closed.id),
            THEIR_NODE,
            Epoch::new(THEIR_EPOCH),
        )
        .unwrap();
    transaction.commit().unwrap();

    let mut transaction = store.begin().unwrap();
    transaction.put(
        RecordAddress::new(open.id, open_db.id, TableId::new(1), RecordId::from("a")),
        b"{}".to_vec(),
    );
    transaction.put(
        RecordAddress::new(
            closed.id,
            closed_db.id,
            TableId::new(1),
            RecordId::from("b"),
        ),
        b"{}".to_vec(),
    );
    let refused = transaction.commit().unwrap_err();
    assert!(
        matches!(refused, Error::WriteIsElsewhere { node, .. } if node == THEIR_NODE),
        "an undeclared range travelled under a declared one's cover: {refused:?}"
    );
}
