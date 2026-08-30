//! `CREATE`, with and without a name for the record.
//!
//! # What this file is actually pinning
//!
//! The identity's **absence** is the whole signal. `CREATE users = { … }` says
//! the caller has a record and no name for it; `CREATE users:1 = { … }` says
//! they have both. Nothing else in the statement moves, and no second verb was
//! added — so the only thing standing between the two readings is where the
//! parser decides to stop reading the target.
//!
//! # Why the other verbs are tested here too
//!
//! The relaxation is `CREATE`-only, and that is a claim about `UPDATE`,
//! `UPSERT`, `DELETE` and `SET` rather than about `CREATE`. Each of those
//! changes a record that already exists, where an address is the honest shape —
//! and a grammar that quietly accepted `UPDATE users = { … }` would be offering
//! to replace a table with an object. The refusals are asserted here because
//! this is the file that would have caused them.

#![allow(clippy::panic, clippy::unwrap_used)]

use tessari_ql::{CreateTarget, Identity, StatementKind, parse};
use tessari_types::RecordId;

fn one(source: &str) -> StatementKind {
    let parsed = match parse(source) {
        Ok(parsed) => parsed,
        Err(error) => panic!("{source}\n  failed: {error}"),
    };
    assert_eq!(parsed.statements.len(), 1, "{source}");
    parsed.statements.into_iter().next().unwrap().kind
}

fn refusal(source: &str) -> String {
    parse(source)
        .err()
        .unwrap_or_else(|| panic!("{source} was accepted"))
        .to_string()
}

#[test]
fn a_create_without_an_identity_leaves_the_naming_to_the_store() {
    let StatementKind::Create { target, .. } = one("CREATE users = { name: 'ada' };") else {
        panic!("expected a create");
    };
    let CreateTarget::Generated(table) = target else {
        panic!("expected the generated form, got {target:?}");
    };
    assert_eq!(table.name.text, "users");
    assert!(table.database.is_none());
}

#[test]
fn a_create_with_an_identity_still_names_the_record_itself() {
    let StatementKind::Create { target, .. } = one("CREATE users:1 = { name: 'ada' };") else {
        panic!("expected a create");
    };
    let CreateTarget::Named(named) = target else {
        panic!("expected the addressed form, got {target:?}");
    };
    assert_eq!(named.table.name.text, "users");
    assert_eq!(named.id, Identity::Fixed(RecordId::Int(1)));
}

/// A qualified table is still a table, and still stops before the `=`.
///
/// `orders.users` holds the same `.` a route holds, so the one shape that could
/// have gone wrong is the parser reading the database half as the table and the
/// table half as something else entirely.
#[test]
fn the_generated_form_takes_a_qualified_table() {
    let StatementKind::Create { target, .. } = one("CREATE orders.users = { name: 'ada' };") else {
        panic!("expected a create");
    };
    let CreateTarget::Generated(table) = target else {
        panic!("expected the generated form, got {target:?}");
    };
    assert_eq!(
        table.database.map(|name| name.text),
        Some("orders".to_owned())
    );
    assert_eq!(table.name.text, "users");
}

/// The identity may still be supplied, and it still binds.
///
/// Worth an assertion because the parser now reaches the `:id` half through a
/// branch rather than unconditionally, and a parameter is the id shape that
/// would break most quietly: it parses, and then nothing binds it.
#[test]
fn a_supplied_identity_is_still_a_parameter_in_the_addressed_form() {
    let StatementKind::Create { target, .. } = one("CREATE users:$who = { name: 'ada' };") else {
        panic!("expected a create");
    };
    let CreateTarget::Named(named) = target else {
        panic!("expected the addressed form, got {target:?}");
    };
    assert_eq!(named.id, Identity::Parameter("who".to_owned()));
}

/// The value is still required, and the refusal still says which one is missing.
#[test]
fn a_create_with_neither_an_identity_nor_a_value_is_refused() {
    assert!(
        refusal("CREATE users;").contains('='),
        "the refusal should ask for the value: {}",
        refusal("CREATE users;")
    );
}

/// The relaxation is `CREATE`'s alone.
///
/// Each of these changes a record that already exists. Accepting a bare table
/// would turn `UPDATE users = { … }` into an offer to replace a whole table
/// with one object — the kind of statement whose damage is done before anybody
/// reads the refusal that never came.
#[test]
fn the_verbs_that_change_an_existing_record_still_demand_an_address() {
    for source in [
        "UPDATE users = { name: 'ada' };",
        "UPSERT users = { name: 'ada' };",
        "SET users = { name: 'ada' };",
    ] {
        let said = refusal(source);
        assert!(
            said.contains(':'),
            "{source} should ask for the record's identity, said: {said}"
        );
    }
}
