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
         PASSWORD 'correct horse battery';\n\
         DEFINE USER prod_owner ON NAMESPACE prod ROLE owner \
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

/// The cluster has one set of users, and a selective follower holds all of them.
///
/// This test asserted the opposite until S4.2, and the reversal is the point
/// rather than an embarrassment: the departure it used to pin was taken because
/// a namespace owner could hold `replicate` over their own namespace, and S4.1
/// removed that. What used to protect a tenancy was the hash not arriving; what
/// protects it now is `administers`, asserted below on the follower itself.
///
/// Asserted against the hash itself rather than against a count alone, because a
/// count that happened to match is not evidence and a PHC string is unmistakable.
#[test]
fn a_selective_follower_receives_every_tenancys_users() {
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
                || rendered.contains("\"prod_owner\"")
                || rendered.contains("\"node\"")
            {
                names.push(rendered);
            }
        }
    }
    // Four, one per declared user: `root` and `node` at the store, `prod_reader`
    // and `prod_owner` in `prod`. Naming them matters more than counting them —
    // a count can be met by the wrong four — and `root` is the one the old rule
    // held back, so it is named explicitly.
    assert_eq!(
        hashes, 4,
        "every user in the cluster travels, got {hashes} credential records: {names:?}"
    );
    assert!(
        names.iter().any(|rendered| rendered.contains("\"root\"")),
        "including the store owner's, which is what one identity per cluster means: {names:?}"
    );
    assert!(
        names
            .iter()
            .any(|rendered| rendered.contains("\"prod_reader\"")),
        "and the subscribed tenancy's own, which it always did: {names:?}"
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

// G025 S4.2 — the identity class replicates everywhere, always.
//
// The cluster has ONE set of users (concept §5.2, and the owner's instruction on
// 2026-09-13 that overruled the departure W215 shipped). What changed to make it
// safe is W279: `replicate` is held over the whole store or not at all, so the
// only principal who can open any subscription is one already entitled to the
// whole store. Before that, an *everywhere* identity class would have handed
// every credential hash in the store to any namespace owner, because the owner
// role expanded to every kind at their own reach and that included this one.
//
// So the protection moved. It is no longer *the hash does not arrive*; it is
// *the hash arrives and the tenant cannot read it*. The test below asserts the
// second, which is why the control above it exists at all.

/// The control, and it is asserted on the **leader** on purpose.
///
/// A refusal that is only ever observed where the record is absent proves
/// nothing about the refusal. This one runs where every record is certainly
/// present, so what it measures is the check rather than the carriage.
#[test]
fn a_namespace_owner_cannot_read_a_store_users_credentials() {
    let leader = store();
    two_tenants(&leader);

    let mut tenant = signed_in(&leader, "prod_owner");
    let refusal = tenant.run("INFO FOR USER root;").unwrap_err();
    assert!(
        matches!(refusal, tessari_session::Error::NotYours { ref user, .. } if user == "root"),
        "a namespace owner does not administer a store-level user, got {refusal:?}"
    );

    // And the listing draws the same boundary, which is the road that leaks
    // quietly: a refusal on the singular form with the plural form still naming
    // everybody would hide nothing at all.
    let listed = tenant.run("INFO FOR USERS;").unwrap();
    let rendered = format!("{listed:?}");
    assert!(
        !rendered.contains("\"root\""),
        "nor may they be shown one in a listing: {rendered}"
    );
}

/// S4.2's first half: a **store-level** user signs in on a selective follower.
///
/// The one thing a per-tenancy identity class could never do. A follower
/// subscribed to `prod` held `prod`'s users and nobody else, so it had no
/// store-level identity at all — nobody on it could declare anything, and an
/// operator who could sign in on the leader was a stranger there.
///
/// Signing in is the assertion rather than a lookup, because `sign_in` matches
/// over the user **records** and never over the name table: a record that
/// reached the follower is one it can authenticate against, and asking the
/// catalog whether the row is present would prove the carriage without proving
/// the consequence.
#[test]
fn a_store_level_user_signs_in_on_a_selective_follower() {
    let leader = store();
    two_tenants(&leader);
    let follower = follow(&leader, Reach::Namespace(NamespaceId::new(1)), "node");

    let mut session = Session::new(&follower);
    session
        .sign_in("root", PASSWORD)
        .expect("the cluster has one set of users, so the store owner is known here too");
}

/// S4.2's second half, run **as the namespace owner** and on the **follower**.
///
/// This is the half S4.1 exists to make safe, and the position is the criterion.
/// An administrator refused proves nothing — an administrator is refused
/// everywhere. A namespace owner is the principal who holds every kind their
/// reach can hold, on a node that now physically holds every credential in the
/// cluster, and they must still not be able to read one that is not theirs.
///
/// The presence check is not decoration. Without it this test passes on a build
/// where the record never arrived, which is the exact shape of assertion that
/// passes with the feature disabled.
#[test]
fn a_namespace_owner_on_a_selective_follower_cannot_read_another_tenancys_credentials() {
    let leader = store();
    two_tenants(&leader);
    let follower = follow(&leader, Reach::Namespace(NamespaceId::new(1)), "node");

    // The record is here. That is what makes the refusal below a refusal rather
    // than an absence.
    {
        let mut transaction = follower.begin().unwrap();
        let catalog = tessari_storage::Catalog::new(&mut transaction);
        assert!(
            catalog
                .users()
                .unwrap()
                .iter()
                .any(|user| user.name == "root"),
            "the store owner's record must be on this follower for the refusal to mean anything"
        );
    }

    let mut tenant = Session::new(&follower);
    tenant.sign_in("prod_owner", PASSWORD).unwrap();
    let refusal = tenant.run("INFO FOR USER root;").unwrap_err();
    assert!(
        matches!(refusal, tessari_session::Error::NotYours { ref user, .. } if user == "root"),
        "the tenancy boundary is what protects the hash now, got {refusal:?}"
    );

    // The listing draws the same boundary. A refusal on the singular form beside
    // a listing that names everybody would hide nothing, and the listing is the
    // road an operator reaches for first.
    let listed = tenant.run("INFO FOR USERS;").unwrap();
    let rendered = format!("{listed:?}");
    assert!(
        !rendered.contains("\"root\""),
        "a namespace owner is shown their own tenancy and no other: {rendered}"
    );
    assert!(
        rendered.contains("\"prod_reader\""),
        "and they are shown their own, so the listing is bounded rather than empty: {rendered}"
    );
}
