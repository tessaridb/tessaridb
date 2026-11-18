//! The pooled-tenant proofs — G090 SG2.
//!
//! The pooled shape (`.bgv` worksheet `183200-g090-sg2-pooled-tenant-cluster-worksheet.md`)
//! puts every tenant namespace on every node and shares compute through the leadership
//! machinery, so a single store holding every tenant at once IS the full-pool placement —
//! a test against it asserts what a pooled node's reader can do, which is the question a
//! tenant is actually asking. The individual rules it walks are proven in their own suites
//! (`authorities.rs`, `grants.rs`, `revocation.rs`, `selective_stream.rs`); what is new
//! here is the composition this file binds: per-tenant users at database reach inside
//! namespace tenants, an operator who alone runs the topology, and the audit read over
//! the pool. The multi-process half of the proof set — the same refusals across real
//! nodes that all hold everything — lives in `tessari-cli/tests/serving.rs`.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

const PASSWORD: &str = "correct horse battery";

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap()
}

fn signed_in<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, PASSWORD).now().unwrap();
    session
}

/// The refusal a statement produces, as text.
fn refused(session: &mut Session<'_>, statement: &str) -> String {
    refusal(session, statement).to_string()
}

/// The refusal a statement produces, typed — so a test names the variant and
/// not only the words.
fn refusal(session: &mut Session<'_>, statement: &str) -> Error {
    match session.run_now(statement) {
        Ok(outcome) => panic!("{statement} was permitted: {outcome:?}"),
        Err(refusal) => refusal,
    }
}

/// The pool, brought up the way the worksheet's bring-up script will run it:
/// two tenant namespaces with their replication STATED (silence is a third
/// state a cluster refuses), databases and data under each, the operator
/// declared last by the anonymous session — the first `DEFINE USER` closes the
/// store, so anything that must run unsigned runs before it — and then, signed
/// in as the operator, one application user and one admin per tenant at
/// DATABASE reach inside the tenant's own namespace.
fn pooled(store: &Store) {
    let mut opening = Session::new(store);
    opening
        .run_now(
            "DEFINE NAMESPACE tenant_a REPLICATION FACTOR 3;\n\
             USE NAMESPACE tenant_a; DEFINE DATABASE app; USE DATABASE app;\n\
             DEFINE TABLE orders SCHEMALESS; CREATE orders:1 = { total: 10 };\n\
             DEFINE NAMESPACE tenant_b REPLICATION FACTOR 3;\n\
             USE NAMESPACE tenant_b; DEFINE DATABASE app; USE DATABASE app;\n\
             DEFINE TABLE payroll SCHEMALESS; CREATE payroll:1 = { salary: 99 };\n\
             DEFINE USER operator ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();

    signed_in(store, "operator")
        .run_now(
            "DEFINE USER a_app ON tenant_a.app AUTHORITIES read, write \
             PASSWORD 'correct horse battery';\n\
             DEFINE USER a_admin ON tenant_a.app \
             AUTHORITIES read, write, manage, govern PASSWORD 'correct horse battery';\n\
             DEFINE USER b_app ON tenant_b.app AUTHORITIES read, write \
             PASSWORD 'correct horse battery';",
        )
        .unwrap();
}

#[test]
fn the_pool_comes_up_and_every_tenant_works() {
    let store = store();
    pooled(&store);

    let mut a_app = signed_in(&store, "a_app");
    a_app
        .run_now("USE NAMESPACE tenant_a; USE DATABASE app; SELECT * FROM orders;")
        .expect("tenant_a's reader reads its own table");
    a_app
        .run_now(
            "USE NAMESPACE tenant_a; USE DATABASE app; \
                  CREATE orders:2 = { total: 20 };",
        )
        .expect("and writes it");

    let mut b_app = signed_in(&store, "b_app");
    b_app
        .run_now("USE NAMESPACE tenant_b; USE DATABASE app; SELECT * FROM payroll;")
        .expect("tenant_b's reader reads its own table, beside tenant_a in the same store");
}

#[test]
fn the_tenant_seam_is_the_namespace_and_it_refuses_without_naming_existence() {
    // Rows P1/P2 of the proof set, in the shape a namespace-as-tenant pool can
    // actually express them: every cross-tenant attack begins at `USE`, because
    // a record address resolves inside the selected tenancy — there is no
    // cross-tenant address to try. What the seam must not do is answer whether
    // the other tenancy exists, and the two refusals are pinned indistinguishable
    // exactly as `authorities.rs` pins them for a namespace grant.
    let store = store();
    pooled(&store);

    let mut a_app = signed_in(&store, "a_app");
    a_app
        .run_now("USE NAMESPACE tenant_a; USE DATABASE app;")
        .expect("a_app starts inside its own tenancy");

    let present = refused(&mut a_app, "USE NAMESPACE tenant_b;");
    let absent = refused(&mut a_app, "USE NAMESPACE tenant_c;");
    assert!(present.contains("tenant_b"), "{present}");
    assert!(absent.contains("tenant_c"), "{absent}");
    assert!(
        present.replace("tenant_b", "X") == absent.replace("tenant_c", "X"),
        "a tenancy that is pooled here and one that does not exist must refuse alike: \
         {present} vs {absent}"
    );
    // The named refusal, not only the shaped message: both the tenancy-first
    // check and the holds-nothing check return this one variant, and the proof
    // pins it so a retargeted refusal cannot pass unnoticed.
    for named in ["tenant_b", "tenant_c"] {
        let seam = refusal(&mut a_app, &format!("USE NAMESPACE {named};"));
        assert!(
            matches!(seam, Error::OutsideTenancy { .. }),
            "the seam must refuse with OutsideTenancy, not {seam:?}"
        );
    }
}

#[test]
fn a_tenant_admin_cannot_run_the_pools_topology() {
    // Row P7. The topology statements name nothing, so their subject is the
    // whole store and they need an owner with no tenancy of their own — a
    // tenant admin reaching four kinds INSIDE its database must not be able to
    // admit a peer, move a placement, or stand for elections, because each of
    // those reaches every tenancy's data at once. The refusals are pinned to
    // name the missing reach rather than merely failing.
    let store = store();
    pooled(&store);

    for statement in [
        "DEFINE REPLICA rogue AT '127.0.0.1:9' ROLES serving",
        "DEFINE NODE ROLES serving, writable, coordinating",
        "DEFINE FAILOVER AWARENESS 2s COLLECTION 3s ROUND 1s CAMPAIGN 2s LEASE 10s",
    ] {
        let mut a_admin = signed_in(&store, "a_admin");
        a_admin
            .run_now("USE NAMESPACE tenant_a; USE DATABASE app;")
            .unwrap();
        let refusal = refused(&mut a_admin, statement);
        assert!(
            refusal.contains("operate") || refusal.contains("owner"),
            "a tenant admin must be refused the pool's topology for missing reach, \
             not for some other reason: {statement} -> {refusal}"
        );
    }
}

#[test]
fn the_audit_names_exactly_who_reaches_a_pooled_table() {
    // The audit row of the proof set. `INFO FOR ACCESS` answers from the same
    // authorization path the statements pass through, so over the pool it must
    // name tenant_a's reader as reaching and never hand tenant_b's reader a
    // reach — one side pinned to expected content, the absence the claim.
    let store = store();
    pooled(&store);

    let mut operator = signed_in(&store, "operator");
    let outcomes = operator
        .run_now("USE NAMESPACE tenant_a; USE DATABASE app; INFO FOR ACCESS TO TABLE orders;")
        .unwrap();
    let Some(Outcome::Value(Value::Object(report))) = outcomes.last() else {
        panic!("expected a report, got {outcomes:?}");
    };
    // The report names every user with a verdict, so the confinement claim is
    // tenant_b's verdict itself — pinned to `false`, not to an absence from
    // the list, which a filtered report would also satisfy.
    let Some(Value::Array(access)) = report.get("access") else {
        panic!("expected a per-user verdict list, got {report:?}");
    };
    let verdict_of = |name: &str| {
        access.iter().find_map(|entry| {
            let Value::Object(row) = entry else {
                return None;
            };
            (row.get("user") == Some(&Value::String(name.to_owned())))
                .then(|| (row.get("read").cloned(), row.get("write").cloned()))
        })
    };
    assert_eq!(
        verdict_of("a_app"),
        Some((Some(Value::Bool(true)), Some(Value::Bool(true)))),
        "tenant_a's reader is not reported as reaching: {access:?}"
    );
    assert_eq!(
        verdict_of("b_app"),
        Some((Some(Value::Bool(false)), Some(Value::Bool(false)))),
        "tenant_b's reader was handed a reach across the seam: {access:?}"
    );
    assert_eq!(
        verdict_of("operator"),
        Some((Some(Value::Bool(true)), Some(Value::Bool(true)))),
        "the operator is not reported as reaching: {access:?}"
    );
}
