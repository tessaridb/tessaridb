//! What a table declares, and what it does about the records already in it.
//!
//! # Why this file exists beside `strictness.rs` and `management.rs`
//!
//! Those two establish that a declaration constrains a **write**. This one is
//! about the other direction — the moment a declaration arrives over records
//! that predate it — and about the statement that asks the same question without
//! changing anything.
//!
//! The behaviour was already there and nothing pinned it. That is the worse of
//! the two ways to be wrong about a constraint: the module's own header claimed
//! the store did not have it, and a reader who trusted the header would have
//! built the check a second time in their application.
//!
//! # The property that makes `CHECK TABLE` worth its scan
//!
//! Every tightening statement here **refuses**, and a refusal fails the whole
//! commit. So a store this release wrote cannot hold a record contradicting its
//! own catalog, and a check over it must come back empty. That is not a reason
//! to drop the statement — it is the reason to have one, because the claim is
//! only worth something if somebody can ask. What the check is for is the store
//! that arrived by another road: restored, replicated from a node running
//! different rules, or repaired underneath the language.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

const SCHEMA: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
";

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

fn opened(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session.run(SCHEMA).unwrap();
    session
}

fn ok(session: &mut Session<'_>, script: &str) {
    session
        .run(script)
        .unwrap_or_else(|error| panic!("{script}: {error}"));
}

fn refusal(session: &mut Session<'_>, script: &str) -> String {
    session
        .run(script)
        .err()
        .map(|error| error.to_string())
        .unwrap_or_else(|| panic!("{script}: answered instead of refusing"))
}

/// The records a `CHECK TABLE` named, as `(record, rule)` pairs.
fn checked(session: &mut Session<'_>, script: &str) -> Vec<(String, String)> {
    let outcome = session
        .run(script)
        .unwrap_or_else(|error| panic!("{script}: {error}"))
        .pop()
        .expect("one outcome");
    let Outcome::Value(Value::Array(rows)) = outcome else {
        panic!("{script} did not answer with a list: {outcome:?}");
    };
    rows.into_iter()
        .map(|row| {
            let Value::Object(fields) = row else {
                panic!("a row that is not an object: {row:?}");
            };
            let text = |name: &str| match fields.get(name) {
                Some(Value::String(held)) => held.clone(),
                other => panic!("{name} is {other:?}"),
            };
            (text("record"), text("rule"))
        })
        .collect()
}

/// A field cannot become required while a stored record leaves it empty.
///
/// The refusal names the record, which is what makes it actionable: the operator
/// goes to that record rather than running the query this statement already ran.
#[test]
fn a_field_cannot_become_required_while_a_stored_record_leaves_it_empty() {
    let store = store();
    let mut session = opened(&store);
    ok(
        &mut session,
        "DEFINE COLLECTION people;
         ALTER TABLE people ADD FIELD name TYPE string;
         CREATE people:1 = { name: 'ada' };
         CREATE people:2 = { };",
    );
    let message = refusal(
        &mut session,
        "ALTER TABLE people ALTER FIELD name TYPE string REQUIRED;",
    );
    assert!(message.contains('2'), "does not name the record: {message}");
    assert!(
        message.contains("name"),
        "does not name the field: {message}"
    );
    // Nothing was written, so the declaration is still the one it was: a record
    // without the field still lands, which it would not if the half that
    // required it had been applied on its own.
    ok(&mut session, "CREATE people:3 = { };");
}

/// The same holds for a declaration that arrives for the first time.
#[test]
fn a_first_declaration_is_held_to_the_records_that_predate_it() {
    let store = store();
    let mut session = opened(&store);
    ok(
        &mut session,
        "DEFINE COLLECTION things; CREATE things:1 = { a: 1 };",
    );
    let message = refusal(
        &mut session,
        "DEFINE FIELD b ON things TYPE string REQUIRED;",
    );
    assert!(message.contains('b'), "does not name the field: {message}");
}

/// A tightening names every offending record, not the first one it meets.
///
/// The count is the thing an operator needs before deciding whether to fix the
/// data or the declaration, and one refusal at a time is how a table gets fixed
/// over an afternoon instead of in one edit.
#[test]
fn a_tightening_names_every_offending_record_and_not_only_the_first() {
    let store = store();
    let mut session = opened(&store);
    ok(
        &mut session,
        "DEFINE COLLECTION rows;
         ALTER TABLE rows ADD FIELD name TYPE string;
         CREATE rows:1 = { };
         CREATE rows:2 = { };
         CREATE rows:3 = { name: 'x' };",
    );
    let message = refusal(
        &mut session,
        "ALTER TABLE rows ALTER FIELD name TYPE string REQUIRED;",
    );
    assert!(message.contains('1'), "does not name record 1: {message}");
    assert!(message.contains('2'), "does not name record 2: {message}");
}

/// The three classes are reported together, and each names its own record.
///
/// Written as one transaction because that is how the three arrive at the pass
/// at once. Each class is reached by a different arm, so a check that walked one
/// of them would still look right on any table that breaks a single rule.
#[test]
fn three_classes_of_disagreement_are_reported_together() {
    let store = store();
    let mut session = opened(&store);
    ok(
        &mut session,
        "DEFINE TABLE mix SCHEMALESS;
         DEFINE FIELD name ON mix TYPE string;
         DEFINE FIELD n ON mix TYPE int;
         CREATE mix:1 = { name: 'ada', n: 3 };
         CREATE mix:2 = { n: 3 };
         CREATE mix:3 = { name: 'bo', n: 3, extra: true };
         CREATE mix:4 = { name: 'cy', n: -5 };",
    );
    let message = refusal(
        &mut session,
        "BEGIN;
         ALTER TABLE mix ALTER FIELD name TYPE string REQUIRED;
         ALTER TABLE mix ALTER FIELD n TYPE int ASSERT $value > 0;
         ALTER TABLE mix SET SCHEMAFULL;
         COMMIT;",
    );
    assert!(
        message.contains("required field name"),
        "the required class is missing: {message}"
    );
    assert!(
        message.contains("no field extra"),
        "the undeclared class is missing: {message}"
    );
    assert!(
        message.contains("declaration refuses"),
        "the assertion class is missing: {message}"
    );
    assert!(
        !message.contains("mix:1") && !message.contains("record 1 "),
        "the record that satisfies everything was reported: {message}"
    );
}

/// A default satisfies a requirement; an explicit null does not.
///
/// Decided rather than discovered — ADR-0055. `DEFAULT` fills a field that is
/// **absent**, and it runs before the requirement is checked, so a write that
/// omits the field and one that sends `none` both come out holding the default.
/// `null` is a value meaning nothing, not an absence, so nothing fills it and
/// the requirement refuses it — which is also SQL's answer for an explicit null
/// into a `NOT NULL` column with a default.
#[test]
fn a_default_satisfies_a_requirement_and_an_explicit_null_does_not() {
    let store = store();
    let mut session = opened(&store);
    ok(
        &mut session,
        "DEFINE COLLECTION d;
         ALTER TABLE d ADD FIELD rank TYPE string REQUIRED DEFAULT 'viewer';",
    );
    ok(&mut session, "CREATE d:1 = { };");
    ok(&mut session, "CREATE d:2 = { rank: NONE };");
    for id in ["d:1", "d:2"] {
        let outcome = session
            .run(&format!("RETURN (SELECT rank FROM {id});"))
            .unwrap()
            .pop()
            .expect("one outcome");
        assert!(
            format!("{outcome:?}").contains("viewer"),
            "{id} did not take the default: {outcome:?}"
        );
    }
    let message = refusal(&mut session, "CREATE d:3 = { rank: NULL };");
    assert!(
        message.contains("null"),
        "the refusal does not say what it held: {message}"
    );
}

/// Clearing a required field is refused, and the old value survives.
#[test]
fn clearing_a_required_field_is_refused_and_the_old_value_survives() {
    let store = store();
    let mut session = opened(&store);
    ok(
        &mut session,
        "DEFINE COLLECTION p;
         ALTER TABLE p ADD FIELD name TYPE string REQUIRED;
         CREATE p:1 = { name: 'ada' };",
    );
    refusal(&mut session, "UPDATE p:1 SET name = NONE;");
    let outcome = session
        .run("RETURN (SELECT name FROM p:1);")
        .unwrap()
        .pop()
        .expect("one outcome");
    assert!(
        format!("{outcome:?}").contains("ada"),
        "a refused clear took the value anyway: {outcome:?}"
    );
}

/// Dropping a declaration from a **strict** table is not a loosening.
///
/// On a schemaless table the drop widens what is admissible and no stored record
/// can contradict it. On a strict one the declaration is what made the field
/// legal, so removing it leaves every record carrying that field disagreeing
/// with the table's own catalog — silently, because the statement reads as a
/// widening. This is the one route into an inconsistent store that the language
/// still had.
#[test]
fn dropping_a_declaration_from_a_strict_table_is_refused_while_records_carry_it() {
    let store = store();
    let mut session = opened(&store);
    ok(
        &mut session,
        "DEFINE TABLE st SCHEMALESS;
         DEFINE FIELD a ON st TYPE string;
         ALTER TABLE st SET SCHEMAFULL;
         CREATE st:1 = { a: 'x' };",
    );
    let message = refusal(&mut session, "ALTER TABLE st DROP FIELD a;");
    assert!(message.contains('a'), "does not name the field: {message}");
    // Still declared, so the refusal wrote nothing: a record carrying it lands.
    ok(&mut session, "CREATE st:2 = { a: 'y' };");
    // And the drop goes through once nothing carries the field.
    ok(&mut session, "DELETE st:1; DELETE st:2;");
    ok(&mut session, "ALTER TABLE st DROP FIELD a;");
}

/// The same drop on a table that is not strict stays a loosening.
#[test]
fn dropping_a_declaration_from_a_lenient_table_is_still_allowed() {
    let store = store();
    let mut session = opened(&store);
    ok(
        &mut session,
        "DEFINE COLLECTION lenient;
         ALTER TABLE lenient ADD FIELD a TYPE string;
         CREATE lenient:1 = { a: 'x' };",
    );
    ok(&mut session, "ALTER TABLE lenient DROP FIELD a;");
}

/// `CHECK TABLE` answers an empty list for a table that holds to its own rules.
#[test]
fn check_table_answers_nothing_for_a_table_that_holds_to_its_declarations() {
    let store = store();
    let mut session = opened(&store);
    ok(
        &mut session,
        "DEFINE COLLECTION c;
         ALTER TABLE c ADD FIELD name TYPE string REQUIRED;
         CREATE c:1 = { name: 'ada' };
         CREATE c:2 = { name: 'grace' };",
    );
    assert!(checked(&mut session, "CHECK TABLE c;").is_empty());
    // And for a table that constrains nothing, without reading its rows.
    ok(
        &mut session,
        "DEFINE COLLECTION bare; CREATE bare:1 = { x: 1 };",
    );
    assert!(checked(&mut session, "CHECK TABLE bare;").is_empty());
}

/// It refuses a table nobody declared, rather than answering an empty list.
///
/// An empty answer would be indistinguishable from a clean table, so a typo in
/// the name would read as a clean bill of health.
#[test]
fn check_table_refuses_a_table_that_does_not_exist() {
    let store = store();
    let mut session = opened(&store);
    let message = refusal(&mut session, "CHECK TABLE nope;");
    assert!(message.contains("nope"), "does not name it: {message}");
}
