//! `DEFINE NODE`, `DEFINE REPLICA`, `INFO FOR NODE` — the node configured in
//! the language rather than in a file beside it.
//!
//! The claim these hold is not "the statements work". It is that the **line**
//! between the two halves is where ADR-0018 draws it, and that the answer shows
//! the line rather than hiding it.
//!
//! - What describes **this machine** — its roles, its address — goes to the
//!   local `META` keyspace, which the log does not carry.
//! - What describes the **topology** — which peers exist — is a catalog record,
//!   which the log does carry and every node therefore learns.
//!
//! `INFO FOR NODE` answers both as two named groups, because a reader has to be
//! able to tell which fields would follow a backup. That the peer list is
//! *under* `cluster` rather than beside `roles` is asserted here as a property
//! and not as formatting: it is the only thing in the answer that says which
//! half a field belongs to, and the bad day it is guessed wrong on is the one
//! where a restore produces a second claimant to one identity.
//!
//! The half these tests do **not** hold is the one that needs a backup to show:
//! that the peer list actually survives a restore while the identity does not.
//! That is the pair, and it lives in `tessari-backup/tests/restore.rs` against a
//! fixture that exercises every engine — neither half of it is the criterion on
//! its own.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::Duration;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

const PASSWORD: &str = "correct horse battery";

fn backend() -> Arc<dyn KvBackend> {
    Arc::new(MemoryBackend::new())
}

/// A store closed by an owner, so that permission is actually being tested.
fn closed(backend: &Arc<dyn KvBackend>) -> Store {
    let store = Store::open(Arc::clone(backend)).unwrap();
    {
        let mut session = Session::new(&store);
        session
            .run(&format!(
                "DEFINE USER root ROLE owner PASSWORD '{PASSWORD}';"
            ))
            .unwrap();
    }
    store
}

/// An owner session, retrying the node's admission bound.
///
/// The node verifies only so many passwords at once and **refuses** the rest
/// rather than queueing them, so that an unauthenticated caller cannot spend the
/// node's memory by asking. A refusal is therefore an ordinary answer on a busy
/// node and a real client retries it — this harness, running its cases in
/// parallel, is exactly such a burst. Retrying here exercises that contract
/// instead of assuming the burst never happens; unwrapping asserted a promise
/// the node does not make.
///
/// The count is bounded so a broken guard fails this helper rather than hanging
/// it.
fn signed_in<'store>(store: &'store Store, name: &str) -> Session<'store> {
    let mut session = Session::new(store);
    for _ in 0..1_000 {
        match session.sign_in(name, PASSWORD) {
            Ok(()) => return session,
            // A place is held for as long as a hash takes, so yielding would
            // spin a thousand times inside one of them and learn nothing.
            Err(Error::SignInThrottled) => std::thread::sleep(Duration::from_millis(2)),
            Err(refused) => panic!("sign-in failed for a reason other than load: {refused}"),
        }
    }
    panic!("the node never admitted a sign-in for {name}");
}

fn owner(store: &Store) -> Session<'_> {
    signed_in(store, "root")
}

/// What `INFO FOR NODE` reports, as an owner.
fn reported(store: &Store) -> std::collections::BTreeMap<String, Value> {
    let outcomes = owner(store).run("INFO FOR NODE;").unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.first() else {
        panic!("not a report: {outcomes:?}");
    };
    fields.clone()
}

/// The peers a report names, as `(name, endpoint)` pairs.
fn peers(report: &std::collections::BTreeMap<String, Value>) -> Vec<(String, String)> {
    let Some(Value::Object(cluster)) = report.get("cluster") else {
        panic!("no cluster group: {report:?}");
    };
    let Some(Value::Array(found)) = cluster.get("peers") else {
        panic!("no peer list: {cluster:?}");
    };
    found
        .iter()
        .map(|peer| {
            let Value::Object(fields) = peer else {
                panic!("not a peer: {peer:?}");
            };
            let text = |key: &str| match fields.get(key) {
                Some(Value::String(value)) => value.clone(),
                other => panic!("{key} is {other:?}"),
            };
            (text("name"), text("endpoint"))
        })
        .collect()
}

/// The id this node prints for itself.
fn own_id(report: &std::collections::BTreeMap<String, Value>) -> String {
    match report.get("id") {
        Some(Value::String(text)) => text.clone(),
        other => panic!("id is {other:?}"),
    }
}

/// One role, as the text it is. `Display` would quote it.
fn named(role: &Value) -> String {
    match role {
        Value::String(text) => text.clone(),
        other => panic!("not a role name: {other:?}"),
    }
}

/// The roles this node currently holds — the local half, from `META`.
fn effective(report: &std::collections::BTreeMap<String, Value>) -> Vec<String> {
    match report.get("roles") {
        Some(Value::Array(found)) => found.iter().map(named).collect(),
        other => panic!("roles is {other:?}"),
    }
}

/// The roles the cluster says this node should hold — the replicated half.
///
/// `None` when no membership row names it, which is a different answer from an
/// empty list and is asserted as such below.
fn desired(report: &std::collections::BTreeMap<String, Value>) -> Option<Vec<String>> {
    let Some(Value::Object(cluster)) = report.get("cluster") else {
        panic!("no cluster group: {report:?}");
    };
    match cluster.get("desired") {
        Some(Value::Null) => None,
        Some(Value::Array(found)) => Some(found.iter().map(named).collect()),
        other => panic!("desired is {other:?}"),
    }
}

/// How long this node says it may still write, as `INFO FOR NODE` answers it.
///
/// `None` when the field is `null` — a node nobody made a leader — which is a
/// different answer from a duration of zero and is asserted as such below.
fn lease(report: &std::collections::BTreeMap<String, Value>) -> Option<Value> {
    let Some(Value::Object(cluster)) = report.get("cluster") else {
        panic!("no cluster group: {report:?}");
    };
    match cluster.get("lease") {
        Some(Value::Null) => None,
        Some(found) => Some(found.clone()),
        None => panic!("no lease field: {cluster:?}"),
    }
}

#[test]
fn a_node_nobody_made_a_leader_reports_no_lease_rather_than_none_left() {
    // The field is present and `null`, not absent. An operator reading this has
    // to be able to tell "no leadership here" from "leadership about to lapse",
    // and a missing key answers neither.
    let store = closed(&backend());
    assert_eq!(lease(&reported(&store)), None);
}

#[test]
fn a_node_holding_a_lease_reports_the_time_it_has_left() {
    let store = closed(&backend());
    store.hold_lease(Duration::from_secs(120));
    let Some(Value::Duration(left)) = lease(&reported(&store)) else {
        panic!("a granted lease is reported as a duration");
    };
    // Positive, and inside the fence rather than inside the grant: the δ the
    // cluster waits before reassigning is not time this node may write in.
    assert!(left.seconds() > 0, "the lease reports {left:?} remaining");
    assert!(
        left.seconds() <= 118,
        "the lease reports {left:?}, which reaches past its own fence"
    );
}

#[test]
fn a_fresh_node_reports_both_halves_and_an_empty_topology() {
    // The shape before anything is configured, because that is what every later
    // assertion is a change *from*. An empty peer list is an answer here, not an
    // absence: a node standing alone has a topology and it has one member.
    let report = reported(&closed(&backend()));

    for named in [
        "id",
        "roles",
        "membership",
        "version",
        "endpoints",
        "cluster",
    ] {
        assert!(report.contains_key(named), "no {named}: {report:?}");
    }
    assert_eq!(report.get("membership"), Some(&Value::from("alone")));
    assert_eq!(report.get("endpoints"), Some(&Value::Array(Vec::new())));
    assert!(peers(&report).is_empty(), "{report:?}");
}

#[test]
fn the_local_half_and_the_replicated_half_are_named_apart() {
    // The grouping is the point of the statement, and this is the assertion that
    // holds it. A flat object carrying the same values would be the defect
    // ADR-0020 §3 refuses: the reader could no longer tell which fields would
    // follow a backup, and remembering that is what fails on the bad day.
    let store = closed(&backend());
    owner(&store)
        .run("DEFINE NODE ROLES serving ENDPOINTS 'here:9000'; DEFINE REPLICA second AT 'there:9001';")
        .unwrap();
    let report = reported(&store);

    // Local, flat.
    assert_eq!(
        report.get("endpoints"),
        Some(&Value::Array(vec![Value::from("here:9000")]))
    );
    // Replicated, nested — and *not* also present at the top level, which is the
    // half of "two groups" that a merged answer would still satisfy.
    assert_eq!(
        peers(&report),
        vec![("second".to_owned(), "there:9001".to_owned())]
    );
    assert!(!report.contains_key("peers"), "{report:?}");
}

#[test]
fn what_the_statement_sets_survives_a_restart() {
    // Configuration that lasted only as long as the process would be a runtime
    // flag wearing a statement's clothes. Re-**opened** rather than re-read, for
    // the reason wave 58's restore test had to be: the question is what is on
    // disk, and a handle can answer from something it read earlier.
    let held = backend();
    let store = closed(&held);
    owner(&store)
        .run("DEFINE NODE ROLES serving, coordinating ENDPOINTS 'here:9000', 'here:9443';")
        .unwrap();
    drop(store);

    let reopened = Store::open(Arc::clone(&held)).unwrap();
    let report = reported(&reopened);
    assert_eq!(
        report.get("roles"),
        Some(&Value::Array(vec![
            Value::from("serving"),
            Value::from("coordinating"),
        ])),
        "{report:?}"
    );
    assert_eq!(
        report.get("endpoints"),
        Some(&Value::Array(vec![
            Value::from("here:9000"),
            Value::from("here:9443"),
        ])),
        "{report:?}"
    );
}

#[test]
fn a_clause_left_out_leaves_its_field_alone() {
    // The alternative — an absent clause clearing its field — makes
    // `DEFINE NODE ENDPOINTS …` a silent way to strip a node of its roles, and
    // an operator would find out from the routing rather than from the
    // statement.
    let store = closed(&backend());
    let mut session = owner(&store);
    session
        .run("DEFINE NODE ROLES coordinating ENDPOINTS 'here:9000';")
        .unwrap();
    session.run("DEFINE NODE ENDPOINTS 'moved:9000';").unwrap();
    let report = reported(&store);

    assert_eq!(
        report.get("roles"),
        Some(&Value::Array(vec![Value::from("coordinating")])),
        "the endpoints clause cleared the roles: {report:?}"
    );
    assert_eq!(
        report.get("endpoints"),
        Some(&Value::Array(vec![Value::from("moved:9000")]))
    );
}

#[test]
fn what_a_clause_names_replaces_what_was_there() {
    // A list is the whole story, which is the rule a grant's field list already
    // follows. Written as its own test because the previous one proves the
    // opposite property, and a reader who saw only that one could reasonably
    // conclude that clauses accumulate.
    let store = closed(&backend());
    let mut session = owner(&store);
    session
        .run("DEFINE NODE ROLES serving, writable, coordinating;")
        .unwrap();
    session.run("DEFINE NODE ROLES serving;").unwrap();

    assert_eq!(
        reported(&store).get("roles"),
        Some(&Value::Array(vec![Value::from("serving")]))
    );
}

#[test]
fn a_role_the_store_does_not_know_is_refused_and_named() {
    // Refused by the store rather than by the grammar, for the reason a vector
    // distance is: which roles exist is the store's question, and this is where
    // it knows what it knows. The refusal names the word so an operator does not
    // have to guess which of three they misspelled.
    let store = closed(&backend());
    let refused = owner(&store)
        .run("DEFINE NODE ROLES serving, leader;")
        .unwrap_err()
        .to_string();
    assert!(refused.contains("leader"), "{refused}");
}

#[test]
fn a_statement_that_sets_nothing_is_refused_rather_than_accepted_as_a_no_op() {
    // A half-written statement that succeeds is how an operator comes to believe
    // a node was configured.
    assert!(owner(&closed(&backend())).run("DEFINE NODE;").is_err());
}

#[test]
fn a_peer_declared_twice_is_refused_unless_the_statement_says_otherwise() {
    // The name is what makes a peer one peer. Without this, a re-run of a
    // provisioning script would double the topology.
    let store = closed(&backend());
    let mut session = owner(&store);
    session
        .run("DEFINE REPLICA second AT 'there:9001';")
        .unwrap();
    assert!(
        session
            .run("DEFINE REPLICA second AT 'there:9001';")
            .is_err()
    );
    session
        .run("DEFINE REPLICA IF NOT EXISTS second AT 'elsewhere:9001';")
        .unwrap();

    // And the accepted re-run left the first declaration standing rather than
    // quietly moving the address — `IF NOT EXISTS` is not an upsert.
    assert_eq!(
        peers(&reported(&store)),
        vec![("second".to_owned(), "there:9001".to_owned())]
    );
}

#[test]
fn peers_are_reported_in_name_order_whatever_order_they_were_declared_in() {
    // Two nodes comparing peer lists is the entire reason to have one, and a
    // list whose order depends on the order somebody wrote a script in cannot be
    // compared.
    let store = closed(&backend());
    owner(&store)
        .run(
            "DEFINE REPLICA charlie AT 'c:9001'; \
             DEFINE REPLICA alpha AT 'a:9001'; \
             DEFINE REPLICA bravo AT 'b:9001';",
        )
        .unwrap();

    let named: Vec<String> = peers(&reported(&store))
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(named, vec!["alpha", "bravo", "charlie"]);
}

#[test]
fn a_viewer_is_refused_all_three_rather_than_passed_over_by_an_empty_grant_check() {
    // The vacuity guard, and it is the same one `$node` needed: none of these
    // statements names a table, so a rule shaped "every table it names is
    // granted" is true of them for a reason that has nothing to do with
    // permission. A viewer reading `INFO FOR NODE` would otherwise be handed the
    // address of every machine holding this store's data.
    let store = closed(&backend());
    owner(&store)
        .run(&format!(
            "DEFINE USER ada ROLE viewer PASSWORD '{PASSWORD}';"
        ))
        .unwrap();

    let mut viewer = signed_in(&store, "ada");
    for statement in [
        "INFO FOR NODE;",
        "DEFINE NODE ROLES serving;",
        "DEFINE REPLICA second AT 'there:9001';",
    ] {
        let refused = viewer.run(statement).unwrap_err().to_string();
        // Refused for what the statement *needs*, not for a table it failed to
        // name — which is the difference between a rule and an emptiness. The
        // authority is named rather than a rank, so the reader is sent to
        // `operate` and not to a role that would not have helped.
        assert!(refused.contains("operate"), "{statement}: {refused}");
    }
}

#[test]
fn an_editor_may_shape_data_and_still_not_configure_the_node() {
    // The line `Needs::of` draws, tested where it actually bites: the other
    // `DEFINE`s are `Write` because an editor is expected to shape the data they
    // own, and these two are not that.
    let store = closed(&backend());
    owner(&store)
        .run(&format!("DEFINE USER e ROLE editor PASSWORD '{PASSWORD}';"))
        .unwrap();

    let mut editor = signed_in(&store, "e");
    editor.run("DEFINE NAMESPACE prod;").unwrap();
    assert!(editor.run("DEFINE NODE ROLES serving;").is_err());
    assert!(
        editor
            .run("DEFINE REPLICA second AT 'there:9001';")
            .is_err()
    );
}

#[test]
fn a_table_called_node_is_still_an_ordinary_table() {
    // `NODE` is a contextual word rather than a reserved one, so the price of
    // this statement family is not a table name that data may already be using.
    // The same reasoning `INFO FOR STORE` was built on, held as a test rather
    // than as a comment in the parser.
    let store = closed(&backend());
    let mut session = owner(&store);
    session
        .run("DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE d; USE DATABASE d;")
        .unwrap();
    session
        .run("DEFINE COLLECTION node; CREATE node:1 = { name: 'a' };")
        .unwrap();

    let outcomes = session.run("SELECT * FROM node;").unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.first() else {
        panic!("not a read: {outcomes:?}");
    };
    assert_eq!(records.len(), 1);
    // And the statement family still works beside it, which is what makes the
    // two genuinely unambiguous rather than merely both accepted.
    assert!(session.run("INFO FOR NODE;").is_ok());
}

// ---------------------------------------------------------------------------
// The role the panel writes, beside the role the node holds (G024 S5.2).
//
// `roles` is the **effective** role: what this process is running as, held in
// `META`, which a backup does not carry. `cluster.desired` is the **desired**
// role: the roles on the membership row bound to this node's id, which is an
// ordinary catalog record and which every node therefore holds. They are two
// values on purpose, they are allowed to differ, and the window in which they
// do is what these tests read.
// ---------------------------------------------------------------------------

#[test]
fn a_node_no_membership_row_names_has_no_desired_role_at_all() {
    // The baseline, and the reason this whole feature changes nothing for a
    // store standing on its own. `null` rather than an empty list: nothing has
    // an opinion about this node, which is a different statement from something
    // having the opinion that it should do nothing.
    let store = closed(&backend());
    let report = reported(&store);
    assert_eq!(desired(&report), None, "{report:?}");

    // A peer that names no node leaves it that way — the row every existing
    // declaration writes.
    owner(&store)
        .run("DEFINE REPLICA second AT 'there:9001' ROLES serving, writable;")
        .unwrap();
    let after = reported(&store);
    assert_eq!(desired(&after), None, "{after:?}");
}

#[test]
fn the_id_the_report_prints_is_the_id_the_clause_reads_back() {
    // The property that makes binding a node something an operator can actually
    // do: copy `id` out of the answer, paste it into the statement. A clause
    // that would not take what the report gives has a conversion step in it, and
    // an undocumented conversion step is where the first wrong binding comes
    // from.
    let store = closed(&backend());
    let id = own_id(&reported(&store));
    owner(&store)
        .run(&format!(
            "DEFINE REPLICA here AT 'here:9000' NODE '{id}' ROLES serving;"
        ))
        .unwrap();

    let report = reported(&store);
    assert_eq!(
        desired(&report),
        Some(vec!["serving".to_owned()]),
        "{report:?}"
    );
}

#[test]
fn declaring_a_desired_role_does_not_move_the_effective_one() {
    // **The window S5.2 asks about.** The declaration commits, the catalog says
    // what this node is supposed to be, and what it is actually running as has
    // not moved — both readable, at the same instant, disagreeing.
    let store = closed(&backend());
    let id = own_id(&reported(&store));
    let before = effective(&reported(&store));
    assert_eq!(before, vec!["serving".to_owned(), "writable".to_owned()]);

    owner(&store)
        .run(&format!(
            "DEFINE REPLICA here AT 'here:9000' NODE '{id}' ROLES serving, coordinating;"
        ))
        .unwrap();

    let report = reported(&store);
    assert_eq!(
        effective(&report),
        before,
        "the effective role moved without the node reconciling: {report:?}"
    );
    assert_eq!(
        desired(&report),
        Some(vec!["serving".to_owned(), "coordinating".to_owned()]),
        "{report:?}"
    );
    assert_ne!(
        desired(&report).unwrap(),
        effective(&report),
        "the two halves cannot be observed differing: {report:?}"
    );
}

#[test]
fn reopening_the_store_converges_the_effective_role_on_the_desired_one() {
    // The other half of the same window: the node reconciles at open, which is
    // the moment it has a catalog to read and has answered nobody yet. Re-opened
    // rather than re-read, for `what_the_statement_sets_survives_a_restart`'s
    // reason — a live handle can answer from something it read earlier.
    let held = backend();
    let store = closed(&held);
    let id = own_id(&reported(&store));
    owner(&store)
        .run(&format!(
            "DEFINE REPLICA here AT 'here:9000' NODE '{id}' ROLES serving, coordinating;"
        ))
        .unwrap();
    drop(store);

    let reopened = Store::open(Arc::clone(&held)).unwrap();
    let report = reported(&reopened);
    assert_eq!(
        effective(&report),
        vec!["serving".to_owned(), "coordinating".to_owned()],
        "{report:?}"
    );
    assert_eq!(
        desired(&report).as_deref(),
        Some(effective(&report).as_slice()),
        "converged and then disagreed with itself: {report:?}"
    );
    // The id is what it was. Adopting a role is not becoming a different node.
    assert_eq!(own_id(&report), id, "{report:?}");
}

#[test]
fn a_row_bound_to_another_node_moves_nothing_here() {
    // The property that lets a desired role replicate to every follower and
    // still mean one machine. Without the id in the comparison this row would be
    // *a* desired role and every node holding it would adopt it — which is the
    // broadcast role that binding by id exists to make impossible.
    let held = backend();
    let store = closed(&held);
    let before = effective(&reported(&store));
    owner(&store)
        .run(&format!(
            "DEFINE REPLICA elsewhere AT 'there:9001' NODE '{}' ROLES coordinating;",
            "ab".repeat(16)
        ))
        .unwrap();
    drop(store);

    let reopened = Store::open(Arc::clone(&held)).unwrap();
    let report = reported(&reopened);
    assert_eq!(
        desired(&report),
        None,
        "somebody else's row was read as ours: {report:?}"
    );
    assert_eq!(effective(&report), before, "{report:?}");
}

#[test]
fn a_bound_row_that_names_no_roles_drains_the_node() {
    // The sharp edge, pinned so that it is a decision rather than a discovery.
    // An absent `ROLES` already means `Roles::NONE`, and `Roles::NONE` is
    // already documented as how an operator drains a node without stopping it.
    // Binding a row and saying nothing therefore drains this node at its next
    // open — one value keeping one meaning, at the price of an edge. The
    // alternative is a second spelling for absent, and two spellings for absent
    // come to disagree.
    let held = backend();
    let store = closed(&held);
    let id = own_id(&reported(&store));
    owner(&store)
        .run(&format!("DEFINE REPLICA here AT 'here:9000' NODE '{id}';"))
        .unwrap();
    drop(store);

    let reopened = Store::open(Arc::clone(&held)).unwrap();
    let report = reported(&reopened);
    assert!(
        effective(&report).is_empty(),
        "a bound row with no roles left the node serving: {report:?}"
    );
    assert_eq!(desired(&report), Some(Vec::new()), "{report:?}");
}

#[test]
fn the_binding_is_reported_beside_the_peer_it_binds() {
    // A binding an operator can write and cannot read back is one they cannot
    // check, and the mistake it hides is silent: a row bound to an id nobody
    // has converges nothing and complains about nothing.
    let store = closed(&backend());
    let id = own_id(&reported(&store));
    owner(&store)
        .run(&format!(
            "DEFINE REPLICA here AT 'here:9000' NODE '{id}';             DEFINE REPLICA second AT 'there:9001';"
        ))
        .unwrap();

    let report = reported(&store);
    let Some(Value::Object(cluster)) = report.get("cluster") else {
        panic!("no cluster group: {report:?}");
    };
    let Some(Value::Array(found)) = cluster.get("peers") else {
        panic!("no peer list: {cluster:?}");
    };
    let bound: Vec<(String, Value)> = found
        .iter()
        .map(|peer| {
            let Value::Object(fields) = peer else {
                panic!("not a peer: {peer:?}");
            };
            let Some(Value::String(name)) = fields.get("name") else {
                panic!("no name: {fields:?}");
            };
            (
                name.clone(),
                fields.get("node").cloned().unwrap_or(Value::Null),
            )
        })
        .collect();
    assert_eq!(bound[0].0, "here");
    assert_eq!(bound[0].1.to_string(), format!("uuid:{id}"), "{bound:?}");
    assert_eq!(bound[1].0, "second");
    assert_eq!(
        bound[1].1,
        Value::Null,
        "an unbound row named a node: {bound:?}"
    );
}

#[test]
fn a_node_id_that_is_not_one_is_refused_where_it_was_written() {
    // Refused at the statement rather than stored: afterwards, a row naming a
    // node nobody will ever be is indistinguishable from a row nobody bound.
    let store = closed(&backend());
    let failure = owner(&store)
        .run("DEFINE REPLICA here AT 'here:9000' NODE 'not-an-id' ROLES serving;")
        .unwrap_err();
    assert!(
        format!("{failure:?}").contains("Uuid"),
        "refused for the wrong reason: {failure:?}"
    );
    assert!(
        peers(&reported(&store)).is_empty(),
        "a refused declaration left a row behind"
    );
}
