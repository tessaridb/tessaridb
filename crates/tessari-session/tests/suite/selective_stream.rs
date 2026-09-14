//! A stream carries only what the subscription reaches — G024 S3.2.
//!
//! W214 built the door: a subscription is its own authority, and
//! `Session::replicate_from` is the only way through it. It served the whole
//! store, because a narrower subscription needs a filter that preserves sequence
//! numbers across the records it drops. This is that filter, asserted from the
//! only position that proves anything about it — **as the replica's own
//! reader**.
//!
//! That position is the point rather than a detail. A test that inspects the
//! delivered records as an administrator asserts what the leader *sent*. A test
//! that signs in on the follower and tries to name another tenant's table
//! asserts what the follower can *do*, which is the question a tenant is
//! actually asking, and it is answered by the follower's own catalog rather than
//! by anybody's filter being correct.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::{Reach, Store};
use tessari_types::{NamespaceId, Sequence};

/// Any follower. These tests are about who may collect and what they receive,
/// never about which node did the collecting, so one id serves them all.
const A_FOLLOWER: [u8; 16] = [7; 16];

const PASSWORD: &str = "correct horse battery";

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// Two tenants with data, a store owner, and a reader inside each.
///
/// Two rather than one so that a reach can be **wrong** rather than merely
/// absent: a filter that answered `prod` for everything would pass every
/// single-tenant test ever written.
fn two_tenants(store: &Store) {
    let mut opening = Session::new(store);
    // `DEFINE USER root` is the last statement of this script and not the first,
    // because declaring the first user *closes the store* — and a closed store
    // refuses the next statement of the very session that closed it, with a span
    // pointing at innocent text.
    opening
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE orders SCHEMALESS;\n\
             CREATE orders:1 = { total: 10 };\n\
             DEFINE NAMESPACE staging; USE NAMESPACE staging;\n\
             DEFINE DATABASE sandbox; USE DATABASE sandbox;\n\
             DEFINE TABLE payroll SCHEMALESS;\n\
             CREATE payroll:1 = { salary: 99 };\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();

    let mut root = signed_in(store, "root");
    // `node` holds `replicate` over the **store** and subscribes over one
    // namespace, which is not a contradiction — it is the separation S4.1 drew.
    // The authority answers *who may open a subscription at all*, and the log it
    // opens carries every tenancy's users, so the only principal it discloses
    // nothing new to is one already entitled to the whole store. The `over`
    // argument answers *what this subscription carries*, and that is still one
    // namespace. Declaring `node` at `ON NAMESPACE prod` is now refused outright.
    root.run(
        "DEFINE USER prod_reader ON NAMESPACE prod AUTHORITIES read \
         PASSWORD 'correct horse battery';\n\
         DEFINE USER node AUTHORITIES replicate \
         PASSWORD 'correct horse battery';",
    )
    .unwrap();
}

/// Replay a subscription into a fresh store, as a follower does.
///
/// `apply_from_stream` rather than `apply_record`, because the predecessor claim
/// is exactly what a filtered stream must not disturb: a record that vanished
/// from the numbering would be refused here as a parted history, and that
/// refusal is the assertion the empty-record test below leans on.
fn follow(leader: &Store, over: Reach, as_user: &str) -> Store {
    let follower = store();
    let mut node = Session::new(leader);
    node.sign_in(as_user, PASSWORD).unwrap();
    let carried = node
        .replicate_from(leader, A_FOLLOWER, over, Sequence::new(1), 256)
        .unwrap();
    let mut previous = tessari_types::Epoch::ZERO;
    for (sequence, record) in carried {
        follower
            .apply_from_stream(sequence, previous, &record)
            .unwrap();
        previous = record.epoch();
    }
    follower
}

fn signed_in<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, PASSWORD).unwrap();
    session
}

/// The criterion, in the position it has to be asserted from.
///
/// A reader who belongs to `prod` signs in **on the follower** and reads `prod`'s
/// own table. Nothing here is administrative: the catalog the read resolves
/// against is whatever the stream actually delivered.
#[test]
fn a_selective_follower_serves_its_own_tenants_reader_from_its_own_catalog() {
    let leader = store();
    two_tenants(&leader);
    let follower = follow(&leader, Reach::Namespace(NamespaceId::new(1)), "node");

    let mut reader = signed_in(&follower, "prod_reader");
    let answer = reader
        .run("USE NAMESPACE prod; USE DATABASE shop; SELECT * FROM orders;")
        .unwrap();
    let Some(Outcome::Records { records, .. }) = answer.last() else {
        panic!("the follower's own reader must be served its own table, got {answer:?}");
    };
    assert_eq!(
        records.len(),
        1,
        "the subscribed namespace's records travel with its catalog"
    );
}

/// The other half of the same criterion, and the half that is the product.
///
/// Two assertions, because there are two mechanisms and only one of them is this
/// wave's. The reader's refusal is the **tenancy** check, which stood before any
/// of this and would stand if the filter were deleted — so it is asserted for
/// what it is and nothing is claimed from it. What the filter has to be proved
/// by is the follower's **catalog**: `staging` is not a namespace it has ever
/// heard of, so there is no name there to refuse.
///
/// Writing only the first would have produced a test that passes with the filter
/// disabled. It did, and that is how this one came to be written.
#[test]
fn a_selective_followers_reader_cannot_name_another_tenants_table() {
    let leader = store();
    two_tenants(&leader);
    let follower = follow(&leader, Reach::Namespace(NamespaceId::new(1)), "node");

    let mut reader = signed_in(&follower, "prod_reader");
    let refusal = reader
        .run("USE NAMESPACE staging; USE DATABASE sandbox; SELECT * FROM payroll;")
        .unwrap_err();
    assert!(
        matches!(refusal, tessari_session::Error::OutsideTenancy { ref name, .. } if name == "staging"),
        "the reader is outside that tenancy before anything else is asked, got {refusal:?}"
    );

    // And the reason nothing could have answered even without that check: the
    // follower's own catalog holds one tenant.
    let mut transaction = follower.begin().unwrap();
    let catalog = tessari_storage::Catalog::new(&mut transaction);
    assert!(
        catalog.namespace_id("prod").unwrap().is_some(),
        "the subscribed namespace is on the follower"
    );
    assert!(
        catalog.namespace_id("staging").unwrap().is_none(),
        "the other tenant's namespace must not exist on this follower at all"
    );
    let names: Vec<String> = catalog
        .namespaces()
        .unwrap()
        .into_iter()
        .map(|declared| declared.name)
        .collect();
    assert_eq!(
        names,
        vec!["prod".to_owned()],
        "and it is the only one, so nothing was carried in by another route"
    );
}

/// Q-535, and it is the reason this wave departs from the concept's own text.
///
/// The concept put the identity class in an *everywhere, always* class. That
/// stopped being safe when `replicate` joined a closed set whose owner role
/// expands to every kind: every namespace owner gained it over their own
/// namespace, and an *everywhere* identity class would hand them every
/// credential hash in the store.
///
/// Asserted against the hash itself rather than against a count, because a count
/// that happened to match is not evidence and a PHC string is unmistakable.
#[test]
fn a_namespace_subscriber_does_not_receive_the_stores_credentials() {
    let leader = store();
    two_tenants(&leader);

    let mut node = Session::new(&leader);
    node.sign_in("node", PASSWORD).unwrap();
    let carried = node
        .replicate_from(
            &leader,
            A_FOLLOWER,
            Reach::Namespace(NamespaceId::new(1)),
            Sequence::new(1),
            256,
        )
        .unwrap();

    let mut hashes = 0;
    let mut names = Vec::new();
    for (_, record) in &carried {
        for mutation in record.mutations() {
            // Decoded rather than rendered from the stored bytes. A `RecordValue`
            // holds an encoded payload, so its `Debug` is a run of integers in
            // which a PHC string is present and invisible — a test written
            // against it passes while the hash travels, which is the exact
            // failure this one exists to catch.
            let rendered = match &mutation.value {
                tessari_encoding::RecordValue::Present(payload) => {
                    format!("{:?}", tessari_encoding::decode_payload(payload).unwrap())
                }
                tessari_encoding::RecordValue::Tombstone => String::new(),
            };
            if rendered.contains("$argon2id$") {
                hashes += 1;
            }
            if rendered.contains("\"root\"")
                || rendered.contains("\"prod_reader\"")
                || rendered.contains("\"node\"")
            {
                names.push(rendered);
            }
        }
    }
    // One, and naming it is the assertion: `prod_reader` lives `ON NAMESPACE
    // prod`, so it is this follower's to hold — the reader in the test above
    // signs in with it, and a follower that could not would have no identity at
    // all. `root` and `node` are the **store's**, and those hashes are what must
    // not be here.
    //
    // It was two until `replicate` became store-only: `node` used to be declared
    // at `ON NAMESPACE prod` and so travelled with the tenancy it named. Moving
    // it to the store moved its credential record out of this stream, which
    // sharpens the test rather than weakening it — the subscriber's own hash is
    // now among the ones that must not arrive.
    assert_eq!(
        hashes, 1,
        "exactly the subscribed namespace's own users travel, got {hashes} credential records: {names:?}"
    );
    assert!(
        !names.iter().any(|rendered| rendered.contains("\"root\"")),
        "the store owner's credential record must not reach a namespace subscriber: {names:?}"
    );
    assert!(
        !names.iter().any(|rendered| rendered.contains("\"node\"")),
        "nor the subscriber's own, which is a store user like any other: {names:?}"
    );
}

/// Every sequence arrives, including the ones filtered to nothing.
///
/// This is what lets the gap rule stay exactly as it was. A record dropped
/// outright would leave a hole, and `apply_from_stream` compares the epoch of
/// the record **before** the one it is offered — so a hole reads as a parted
/// history and refuses the stream. An empty record costs a frame and keeps the
/// arithmetic.
#[test]
fn a_commit_entirely_outside_the_reach_arrives_empty_and_the_tail_advances() {
    let leader = store();
    two_tenants(&leader);

    // The whole-store baseline is taken as the owner. `node` could now take it
    // too — it holds `replicate` over the store — and the owner is kept here on
    // purpose, so that the two subscriptions being compared differ in their
    // `over` argument and in nothing else about them that is easy to overlook.
    let mut owner = Session::new(&leader);
    owner.sign_in("root", PASSWORD).unwrap();
    let whole = owner
        .replicate_from(&leader, A_FOLLOWER, Reach::Store, Sequence::new(1), 256)
        .unwrap();
    let mut node = Session::new(&leader);
    node.sign_in("node", PASSWORD).unwrap();
    let carried = node
        .replicate_from(
            &leader,
            A_FOLLOWER,
            Reach::Namespace(NamespaceId::new(1)),
            Sequence::new(1),
            256,
        )
        .unwrap();

    assert_eq!(
        whole.len(),
        carried.len(),
        "a selective subscription receives every sequence, not a subset of them"
    );
    assert!(
        carried
            .iter()
            .any(|(_, record)| record.mutations().is_empty()),
        "a commit entirely outside the reach must arrive as an elided record"
    );
    assert!(
        carried
            .iter()
            .any(|(_, record)| !record.mutations().is_empty()),
        "and the filter must not have elided everything, which would pass the line above"
    );

    let follower = follow(&leader, Reach::Namespace(NamespaceId::new(1)), "node");
    assert_eq!(
        follower.committed_tail().unwrap().get(),
        u64::try_from(whole.len()).unwrap(),
        "the follower's tail advances over an elided record exactly as over a full one"
    );
}

/// A store-reach subscription is unchanged by any of this.
///
/// The whole-node case is a distinct subscription kind rather than sugar for
/// every namespace, and this pins that it still delivers the store: a filter
/// that quietly narrowed it would break replication for every follower that
/// exists today while every test about the narrow case passed.
#[test]
fn a_store_reach_subscription_still_carries_the_whole_log() {
    let leader = store();
    two_tenants(&leader);
    let follower = follow(&leader, Reach::Store, "root");

    let mut reader = signed_in(&follower, "root");
    for script in [
        "USE NAMESPACE prod; USE DATABASE shop; SELECT * FROM orders;",
        "USE NAMESPACE staging; USE DATABASE sandbox; SELECT * FROM payroll;",
    ] {
        let answer = reader.run(script).unwrap();
        let Some(Outcome::Records { records, .. }) = answer.last() else {
            panic!("a store subscription carries both tenants, {script} gave {answer:?}");
        };
        assert_eq!(
            records.len(),
            1,
            "both tenants' records reach a store subscriber"
        );
    }
}
