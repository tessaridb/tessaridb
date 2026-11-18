//! `DEFINE`/`ALTER`/`DROP TOPOLOGY` and the member row's `DOMAIN` clause — the
//! declared scheme a cluster is arranged over, as catalog data.
//!
//! The claim these hold is the one the scheme itself makes: a topology is a
//! checked structure stored as one row per cluster, ordered by the leadership
//! pair, and the refusals name what broke — the held scheme a second declare
//! would replace, the realm or domain named twice, the realm no scheme holds.
//! The relations that need the membership — the claim arithmetic against the
//! voters, the copies spanning realms — are the layer above this suite and
//! arrive with them.
//!
//! The read-back asserted here is the catalog row, the same surface the
//! membership relations read; `INFO FOR TOPOLOGY` arrives with the read-back
//! wave and asserts the operator-facing shape there.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Session};
use tessari_storage::{Catalog, Store};

const PASSWORD: &str = "correct horse battery";

fn backend() -> Arc<dyn KvBackend> {
    Arc::new(MemoryBackend::new())
}

/// A store closed by an owner, so that permission is actually being tested.
fn closed(backend: &Arc<dyn KvBackend>) -> Store {
    let store = Store::open(Arc::clone(backend)).now().unwrap();
    {
        let mut session = Session::new(&store);
        session
            .run_now(&format!(
                "DEFINE USER root ROLE owner PASSWORD '{PASSWORD}';"
            ))
            .unwrap();
    }
    store
}

/// An owner session, retrying the node's admission bound (the configuration
/// suite's helper and its reason, verbatim).
fn signed_in<'store>(store: &'store Store, name: &str) -> Session<'store> {
    let mut session = Session::new(store);
    for _ in 0..1_000 {
        match session.sign_in(name, PASSWORD).now() {
            Ok(()) => return session,
            Err(Error::SignInThrottled) => std::thread::sleep(Duration::from_millis(2)),
            Err(refused) => panic!("sign-in failed for a reason other than load: {refused}"),
        }
    }
    panic!("the node never admitted a sign-in for {name}");
}

fn owner(store: &Store) -> Session<'_> {
    signed_in(store, "root")
}

/// The declared scheme, straight from the catalog row the statement wrote.
fn declared(store: &Store) -> Option<tessari_storage::TopologyDefinition> {
    let mut transaction = store.begin().now().unwrap();
    Catalog::new(&mut transaction).topology().now().unwrap()
}

#[test]
fn a_topology_is_stored_as_one_row_and_reads_back_as_stated() {
    let store = closed(&backend());
    owner(&store)
        .run_now(
            "DEFINE TOPOLOGY main \
             REALM 'dc-a' DOMAIN 'az-1', 'az-2' \
             REALM 'dc-b' \
             SURVIVES REALM LOSS;",
        )
        .unwrap();

    let held = declared(&store).expect("the scheme the statement wrote");
    assert_eq!(held.name, "main");
    assert_eq!(held.topology.realms().len(), 2);
    let dc_a = held.topology.realm("dc-a").expect("the realm as written");
    assert_eq!(dc_a.domains(), ["az-1", "az-2"]);
    assert!(
        held.topology
            .realm("dc-b")
            .expect("held")
            .domains()
            .is_empty()
    );
    assert!(
        held.topology.survives_realm_loss(),
        "the claim is stored, and read back, as what was stated"
    );
    // The first scheme under no leadership is version zero: the number counts
    // settings under a leadership, and this is the first.
    assert_eq!(held.version, 0);
}

#[test]
fn a_second_topology_is_refused_naming_the_scheme_it_would_replace() {
    let store = closed(&backend());
    owner(&store)
        .run_now("DEFINE TOPOLOGY main REALM 'dc-a';")
        .unwrap();
    let refused = owner(&store)
        .run_now("DEFINE TOPOLOGY other REALM 'dc-b';")
        .expect_err("a second scheme parsed into a store that holds one");
    match refused {
        Error::TopologyAlreadyDeclared { held, written, .. } => {
            assert_eq!(held, "main");
            assert_eq!(written, "other");
        }
        other => panic!("expected the one-per-cluster refusal, got {other}"),
    }
    // The refused statement left nothing behind.
    assert_eq!(declared(&store).expect("still held").name, "main");
}

#[test]
fn a_structure_that_does_not_hold_is_refused_with_the_direction() {
    let store = closed(&backend());
    // Two realms with one name: the refusal carries the store's own words for
    // the relation and the realm that broke it.
    let refused = owner(&store)
        .run_now("DEFINE TOPOLOGY main REALM 'dc-a' REALM 'dc-a';")
        .expect_err("two realms with one name parsed");
    match refused {
        Error::TopologyRefused { reason, .. } => {
            assert!(
                reason.contains("dc-a"),
                "the refusal names the realm: {reason}"
            );
        }
        other => panic!("expected the structural refusal, got {other}"),
    }
    assert!(
        declared(&store).is_none(),
        "a refused declare leaves nothing"
    );
    // Two domains with one name, inside one realm.
    let refused = owner(&store)
        .run_now("DEFINE TOPOLOGY main REALM 'dc-a' DOMAIN 'az-1', 'az-1';")
        .expect_err("two domains with one name parsed");
    let Error::TopologyRefused { reason, .. } = refused else {
        panic!("expected the structural refusal, got {refused}");
    };
    assert!(
        reason.contains("az-1"),
        "the refusal names the domain: {reason}"
    );
}

#[test]
fn a_topology_amends_by_the_realm_and_replaces_the_row() {
    let store = closed(&backend());
    owner(&store)
        .run_now("DEFINE TOPOLOGY main REALM 'dc-a' REALM 'dc-b' SURVIVES REALM LOSS;")
        .unwrap();
    owner(&store)
        .run_now("ALTER TOPOLOGY main ADD REALM 'dc-c' DOMAIN 'az-1';")
        .unwrap();
    let held = declared(&store).expect("held");
    assert_eq!(held.topology.realms().len(), 3);
    assert_eq!(held.version, 1, "an amend is the next setting of the pair");
    assert!(
        held.topology.survives_realm_loss(),
        "amending a realm leaves the claim alone"
    );

    owner(&store)
        .run_now("ALTER TOPOLOGY main DROP REALM 'dc-b';")
        .unwrap();
    let held = declared(&store).expect("held");
    assert_eq!(held.topology.realms().len(), 2);
    assert!(held.topology.realm("dc-b").is_none());

    // A realm the scheme does not hold refuses, naming it.
    let refused = owner(&store)
        .run_now("ALTER TOPOLOGY main DROP REALM 'dc-x';")
        .expect_err("dropping a realm nobody declared");
    let Error::Unknown { entity, name, .. } = refused else {
        panic!("expected the unknown-realm refusal, got {refused}");
    };
    assert_eq!(entity, "realm of this topology");
    assert_eq!(name, "dc-x");

    // Amending a scheme nobody declared refuses naming the scheme.
    let fresh = closed(&backend());
    let refused = owner(&fresh)
        .run_now("ALTER TOPOLOGY main ADD REALM 'dc-a';")
        .expect_err("amending a scheme nobody declared");
    let Error::Unknown { entity, name, .. } = refused else {
        panic!("expected the unknown-topology refusal, got {refused}");
    };
    assert_eq!(entity, "topology");
    assert_eq!(name, "main");
}

#[test]
fn dropping_the_scheme_leaves_the_member_rows_labels_alone() {
    let store = closed(&backend());
    owner(&store)
        .run_now("DEFINE TOPOLOGY main REALM 'dc-a' DOMAIN 'az-1';")
        .unwrap();
    owner(&store)
        .run_now(
            "DEFINE REPLICA n1 AT 'a:9001' ROLES coordinating, writable \
             REPLICATES STORE REGION 'dc-a' DOMAIN 'az-1';",
        )
        .unwrap();
    owner(&store).run_now("DROP TOPOLOGY main;").unwrap();
    assert!(declared(&store).is_none());
    // The member row survives: its labels read as what they are without a
    // scheme to declare them.
    let mut transaction = store.begin().now().unwrap();
    let rows = Catalog::new(&mut transaction).replicas().now().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].region.as_deref(), Some("dc-a"));
    assert_eq!(rows[0].domain.as_deref(), Some("az-1"));

    let refused = owner(&store)
        .run_now("DROP TOPOLOGY main;")
        .expect_err("dropping a scheme nobody holds");
    let Error::Unknown { entity, name, .. } = refused else {
        panic!("expected the unknown-topology refusal, got {refused}");
    };
    assert_eq!(entity, "topology");
    assert_eq!(name, "main");
}

#[test]
fn a_member_row_carries_its_domain_and_amends_it_in_place() {
    let store = closed(&backend());
    owner(&store)
        .run_now(
            "DEFINE REPLICA n1 AT 'a:9001' ROLES coordinating, writable \
             REPLICATES STORE REGION 'dc-a' DOMAIN 'az-1';",
        )
        .unwrap();
    {
        let mut transaction = store.begin().now().unwrap();
        let rows = Catalog::new(&mut transaction).replicas().now().unwrap();
        assert_eq!(rows[0].region.as_deref(), Some("dc-a"));
        assert_eq!(rows[0].domain.as_deref(), Some("az-1"));
    }
    owner(&store)
        .run_now("ALTER REPLICA n1 DOMAIN 'az-2';")
        .unwrap();
    {
        let mut transaction = store.begin().now().unwrap();
        let rows = Catalog::new(&mut transaction).replicas().now().unwrap();
        assert_eq!(rows[0].domain.as_deref(), Some("az-2"));
    }
    owner(&store)
        .run_now("ALTER REPLICA n1 DOMAIN NONE;")
        .unwrap();
    let mut transaction = store.begin().now().unwrap();
    let rows = Catalog::new(&mut transaction).replicas().now().unwrap();
    assert_eq!(rows[0].domain, None, "NONE clears the domain alone");
    assert_eq!(rows[0].region.as_deref(), Some("dc-a"));
}

#[test]
fn amending_or_dropping_under_another_name_refuses_naming_it() {
    // The row is the scheme's and the scheme has a name: a statement under a
    // different name would move, or remove, a scheme it did not name.
    let store = closed(&backend());
    owner(&store)
        .run_now("DEFINE TOPOLOGY main REALM 'dc-a';")
        .unwrap();
    for statement in [
        "ALTER TOPOLOGY other ADD REALM 'dc-b';",
        "DROP TOPOLOGY other;",
    ] {
        let refused = owner(&store).run_now(statement).expect_err(statement);
        let Error::Unknown { entity, name, .. } = refused else {
            panic!("expected the unknown-topology refusal, got {refused}");
        };
        assert_eq!(entity, "topology");
        assert_eq!(name, "other");
    }
    // The scheme the wrong name would have touched is untouched.
    assert_eq!(declared(&store).expect("held").topology.realms().len(), 1);
}

#[test]
fn r1_a_member_label_the_scheme_does_not_hold_is_refused_where_it_is_written() {
    // The relations run at the member-row write: a region or domain the held
    // scheme does not declare is refused naming the row and the label.
    let store = closed(&backend());
    owner(&store)
        .run_now("DEFINE TOPOLOGY main REALM 'dc-a' DOMAIN 'az-1';")
        .unwrap();
    let refused = owner(&store)
        .run_now(
            "DEFINE REPLICA n1 AT 'a:9001' ROLES coordinating REPLICATES \
             STORE REGION 'dc-x';",
        )
        .expect_err("an orphan region parsed as a position");
    let Error::TopologyRefused { reason, .. } = refused else {
        panic!("expected the relations refusal, got {refused}");
    };
    assert!(
        reason.contains("'n1'"),
        "the refusal names the row: {reason}"
    );
    assert!(
        reason.contains("'dc-x'"),
        "the refusal names the label: {reason}"
    );

    // And the declare of the scheme itself is refused when the membership it
    // meets already holds a label the scheme does not cover.
    let fresh = closed(&backend());
    owner(&fresh)
        .run_now(
            "DEFINE REPLICA n1 AT 'a:9001' ROLES coordinating REPLICATES \
             STORE REGION 'dc-x';",
        )
        .unwrap();
    let refused = owner(&fresh)
        .run_now("DEFINE TOPOLOGY main REALM 'dc-a';")
        .expect_err("a scheme that orphans an existing label declared");
    let Error::TopologyRefused { reason, .. } = refused else {
        panic!("expected the relations refusal, got {refused}");
    };
    assert!(
        reason.contains("'dc-x'"),
        "the refusal names the orphan label: {reason}"
    );
}

#[test]
fn r2_the_claim_is_refused_where_the_arithmetic_does_not_hold() {
    // Two realms claim survival; two voters stand in one of them. The node
    // running the check is its own unlabelled electorate, and even counting
    // it the loss of dc-a leaves two of four — not more than half. The declare
    // is refused naming the realm and the arithmetic.
    let store = closed(&backend());
    for (name, region) in [("n1", "dc-a"), ("n2", "dc-a"), ("n3", "dc-b")] {
        owner(&store)
            .run_now(&format!(
                "DEFINE REPLICA {name} AT '{name}:9001' ROLES coordinating, \
                 writable REPLICATES STORE REGION '{region}';"
            ))
            .unwrap();
    }
    let refused = owner(&store)
        .run_now("DEFINE TOPOLOGY main REALM 'dc-a' REALM 'dc-b' SURVIVES REALM LOSS;")
        .expect_err("a claim the membership cannot keep declared");
    let Error::TopologyRefused { reason, .. } = refused else {
        panic!("expected the relations refusal, got {refused}");
    };
    assert!(
        reason.contains("'dc-a'"),
        "the refusal names the realm: {reason}"
    );
    assert!(
        reason.contains("more than half"),
        "the refusal names the direction: {reason}"
    );
    // The refused declare left no scheme behind.
    assert!(declared(&store).is_none());
}

#[test]
fn r3_copies_of_a_kept_namespace_must_span_realms() {
    let store = closed(&backend());
    owner(&store)
        .run_now("DEFINE TOPOLOGY main REALM 'dc-a' REALM 'dc-b';")
        .unwrap();
    // A namespace asked to keep copies before any realm holds one is refused
    // at the ask: the only holder is the unlabelled node the check runs on,
    // and copies in no arrangement are the same fiction one degree worse.
    let refused = owner(&store)
        .run_now("DEFINE NAMESPACE prod REPLICATION FACTOR 3;")
        .expect_err("copies in no realm read as spanning");
    let Error::TopologyRefused { reason, .. } = refused else {
        panic!("expected the relations refusal, got {refused}");
    };
    assert!(
        reason.contains("'prod'"),
        "the refusal names the namespace: {reason}"
    );

    // A single copy spans nothing and needs to; holders then place themselves
    // one realm at a time.
    owner(&store).run_now("DEFINE NAMESPACE prod;").unwrap();
    owner(&store)
        .run_now(
            "DEFINE REPLICA n1 AT 'a:9001' ROLES coordinating, writable \
             REPLICATES STORE REGION 'dc-a';",
        )
        .unwrap();
    let refused = owner(&store)
        .run_now("ALTER NAMESPACE prod REPLICATION FACTOR 2;")
        .expect_err("one-realm copies read as spanning");
    let Error::TopologyRefused { reason, .. } = refused else {
        panic!("expected the relations refusal, got {refused}");
    };
    assert!(
        reason.contains("'dc-a'"),
        "the refusal names the realm: {reason}"
    );

    // The second realm's holder satisfies the relation at the ask.
    owner(&store)
        .run_now(
            "DEFINE REPLICA n2 AT 'b:9001' ROLES coordinating, writable \
             REPLICATES STORE REGION 'dc-b';",
        )
        .unwrap();
    owner(&store)
        .run_now("ALTER NAMESPACE prod REPLICATION FACTOR 2;")
        .expect("copies spanning two realms hold the ask");
}

#[test]
fn r4_a_local_majority_namespace_needs_a_majority_where_the_leader_stands() {
    // The leader's region is its own row's region: bind the node's own row,
    // place one non-voter beside it in that region and a second voter in the
    // other realm, and the namespace asking LOCAL MAJORITY is refused at the
    // ask — the region holds two members but one voter, and one is not a
    // majority of two.
    let store = closed(&backend());
    let id = match owner(&store).run_now("INFO FOR NODE;").unwrap().first() {
        Some(tessari_session::Outcome::Value(tessari_types::Value::Object(fields))) => {
            match fields.get("id") {
                Some(tessari_types::Value::String(text)) => text.clone(),
                other => panic!("id is {other:?}"),
            }
        }
        other => panic!("not a report: {other:?}"),
    };
    owner(&store)
        .run_now("DEFINE TOPOLOGY main REALM 'dc-a' REALM 'dc-b';")
        .unwrap();
    owner(&store)
        .run_now(&format!(
            "DEFINE REPLICA selfrow AT 's:9001' NODE '{id}' ROLES coordinating, \
             writable REPLICATES STORE REGION 'dc-a';"
        ))
        .unwrap();
    owner(&store)
        .run_now(
            "DEFINE REPLICA n1 AT 'a:9001' ROLES serving REPLICATES STORE \
             REGION 'dc-a';",
        )
        .unwrap();
    owner(&store)
        .run_now(
            "DEFINE REPLICA n2 AT 'b:9001' ROLES coordinating, writable \
             REPLICATES STORE REGION 'dc-b';",
        )
        .unwrap();
    let refused = owner(&store)
        .run_now(
            "DEFINE NAMESPACE prod REPLICATION FACTOR 2 ACKNOWLEDGE LOCAL \
             MAJORITY;",
        )
        .expect_err("a LOCAL MAJORITY with no majority where the leader stands");
    let Error::TopologyRefused { reason, .. } = refused else {
        panic!("expected the relations refusal, got {refused}");
    };
    assert!(
        reason.contains("'prod'"),
        "the refusal names the namespace: {reason}"
    );
    assert!(
        reason.contains("'dc-a'"),
        "the refusal names the region: {reason}"
    );

    // The level the region CAN meet is accepted at the ask.
    owner(&store)
        .run_now("DEFINE NAMESPACE prod REPLICATION FACTOR 2 ACKNOWLEDGE MAJORITY;")
        .expect("MAJORITY meets the membership as it stands");
}

#[test]
fn info_for_topology_answers_null_when_nobody_declared_one() {
    // The M17 rule: built-in nothing is not a policy. `null` is the answer,
    // not an empty report — an empty object would read as a scheme that
    // declares nothing, which is a different fact from a scheme that never
    // arrived.
    let store = closed(&backend());
    match owner(&store)
        .run_now("INFO FOR TOPOLOGY main;")
        .unwrap()
        .first()
    {
        Some(tessari_session::Outcome::Value(tessari_types::Value::Null)) => {}
        other => panic!("expected null, got {other:?}"),
    }
    match owner(&store)
        .run_now("INFO FOR TOPOLOGIES;")
        .unwrap()
        .first()
    {
        Some(tessari_session::Outcome::Value(tessari_types::Value::Object(fields))) => {
            assert!(
                matches!(
                    fields.get("topologies"),
                    Some(tessari_types::Value::Array(names)) if names.is_empty()
                ),
                "no schemes, an empty list: {fields:?}"
            );
        }
        other => panic!("expected a report, got {other:?}"),
    }
}

#[test]
fn info_for_topology_reports_the_scheme_and_its_live_verdicts() {
    let store = closed(&backend());
    owner(&store)
        .run_now(
            "DEFINE REPLICA n1 AT 'a:9001' ROLES coordinating, writable \
             REPLICATES STORE REGION 'dc-a';",
        )
        .unwrap();
    owner(&store)
        .run_now(
            "DEFINE REPLICA n2 AT 'b:9001' ROLES coordinating, writable \
             REPLICATES STORE REGION 'dc-b';",
        )
        .unwrap();
    owner(&store)
        .run_now(
            "DEFINE TOPOLOGY main REALM 'dc-a' DOMAIN 'az-1' REALM 'dc-b' \
             SURVIVES REALM LOSS;",
        )
        .unwrap();
    owner(&store)
        .run_now("DEFINE NAMESPACE prod REPLICATION FACTOR 2;")
        .unwrap();

    let Some(tessari_session::Outcome::Value(tessari_types::Value::Object(report))) = owner(&store)
        .run_now("INFO FOR TOPOLOGY main;")
        .unwrap()
        .first()
        .cloned()
    else {
        panic!("not a report")
    };
    assert_eq!(
        report.get("name"),
        Some(&tessari_types::Value::from("main"))
    );
    assert_eq!(
        report.get("satisfied"),
        Some(&tessari_types::Value::Bool(true))
    );
    assert_eq!(
        report.get("survives_realm_loss"),
        Some(&tessari_types::Value::Bool(true))
    );
    let Some(tessari_types::Value::Array(realms)) = report.get("realms") else {
        panic!("no realms in {report:?}");
    };
    assert_eq!(realms.len(), 2);
    let Some(tessari_types::Value::Object(dc_a)) = realms.first() else {
        panic!("no realm in {realms:?}");
    };
    assert_eq!(dc_a.get("name"), Some(&tessari_types::Value::from("dc-a")));
    // The claim's arithmetic, live: losing dc-a leaves two of three voters —
    // the two peers outside it plus this node, which no realm claims.
    let Some(tessari_types::Value::Object(loss)) = dc_a.get("loss") else {
        panic!("no loss verdict in {dc_a:?}");
    };
    assert_eq!(
        loss.get("survived"),
        Some(&tessari_types::Value::Bool(true))
    );
    assert_eq!(
        loss.get("voters_left"),
        Some(&tessari_types::Value::from(2))
    );
    assert_eq!(
        loss.get("voters_needed"),
        Some(&tessari_types::Value::from(2))
    );
    // The member assignment a console renders its table and tree from.
    let Some(tessari_types::Value::Array(members)) = dc_a.get("members") else {
        panic!("no members in {dc_a:?}");
    };
    assert_eq!(members.len(), 1);
    // The namespace fit: copies spanning the two realms the holders stand in.
    let Some(tessari_types::Value::Array(namespaces)) = report.get("namespaces") else {
        panic!("no namespaces in {report:?}");
    };
    assert_eq!(namespaces.len(), 1);
    let Some(tessari_types::Value::Object(prod)) = namespaces.first() else {
        panic!("no namespace in {namespaces:?}");
    };
    assert_eq!(prod.get("name"), Some(&tessari_types::Value::from("prod")));
    let Some(tessari_types::Value::Object(copies_in)) = prod.get("copies_in") else {
        panic!("no copies_in in {prod:?}");
    };
    assert_eq!(
        copies_in.get("realms"),
        Some(&tessari_types::Value::from(2))
    );

    // The plural names the scheme.
    match owner(&store)
        .run_now("INFO FOR TOPOLOGIES;")
        .unwrap()
        .first()
    {
        Some(tessari_session::Outcome::Value(tessari_types::Value::Object(fields))) => assert!(
            matches!(
                fields.get("topologies"),
                Some(tessari_types::Value::Array(names))
                    if names == &vec![tessari_types::Value::from("main")]
            ),
            "the scheme, listed: {fields:?}"
        ),
        other => panic!("expected a report, got {other:?}"),
    }
}

#[test]
fn the_report_names_the_relation_the_layout_broke() {
    // The scheme reports the gap; it does not enforce it at write time. A
    // member row's removal is not re-checked — dropping the second realm's
    // holder leaves every copy in one realm, and the read-back says so with
    // the relation named.
    let store = closed(&backend());
    owner(&store)
        .run_now(
            "DEFINE REPLICA n1 AT 'a:9001' ROLES coordinating, writable \
             REPLICATES STORE REGION 'dc-a';",
        )
        .unwrap();
    owner(&store)
        .run_now(
            "DEFINE REPLICA n2 AT 'b:9001' ROLES coordinating, writable \
             REPLICATES STORE REGION 'dc-b';",
        )
        .unwrap();
    owner(&store)
        .run_now("DEFINE TOPOLOGY main REALM 'dc-a' REALM 'dc-b';")
        .unwrap();
    owner(&store)
        .run_now("DEFINE NAMESPACE prod REPLICATION FACTOR 2;")
        .unwrap();
    owner(&store).run_now("DROP REPLICA n2;").unwrap();

    let Some(tessari_session::Outcome::Value(tessari_types::Value::Object(report))) = owner(&store)
        .run_now("INFO FOR TOPOLOGY main;")
        .unwrap()
        .first()
        .cloned()
    else {
        panic!("not a report")
    };
    assert_eq!(
        report.get("satisfied"),
        Some(&tessari_types::Value::Bool(false))
    );
    assert_eq!(
        report.get("relation"),
        Some(&tessari_types::Value::from("copies span realms"))
    );
}

#[test]
fn reading_the_scheme_is_the_operators_not_an_editors() {
    let store = closed(&backend());
    owner(&store)
        .run_now("DEFINE TOPOLOGY main REALM 'dc-a';")
        .unwrap();
    owner(&store)
        .run_now(&format!(
            "DEFINE USER app ROLE editor PASSWORD '{PASSWORD}';"
        ))
        .unwrap();
    let refused = format!(
        "{:?}",
        signed_in(&store, "app").run_now("INFO FOR TOPOLOGY main;")
    );
    assert!(refused.contains("RoleForbids"), "{refused}");
}

#[test]
fn declaring_the_scheme_is_the_operators_not_an_editors() {
    let store = closed(&backend());
    owner(&store)
        .run_now(&format!(
            "DEFINE USER app ROLE editor PASSWORD '{PASSWORD}';"
        ))
        .unwrap();
    let refused = format!(
        "{:?}",
        signed_in(&store, "app").run_now("DEFINE TOPOLOGY main REALM 'dc-a';")
    );
    assert!(refused.contains("RoleForbids"), "{refused}");
}
