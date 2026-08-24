//! What a script does, and whether this node may take it.
//!
//! The classifier's **completeness** is not asserted here, because the compiler
//! already proves it: `Effect::of` is an exhaustive `match` with no wildcard
//! arm, so a statement kind that reaches no classification does not build. What
//! this file checks is the part a compiler cannot — that each classification is
//! the *right* one, and that the answer is taken over the whole script.

#![allow(clippy::panic, clippy::unwrap_used)]

use bgv_db_encoding::Roles;
use bgv_db_ql::parse;
use bgv_db_session::{Effect, admits};

/// Every statement kind, with the effect it must have.
///
/// A table rather than a sequence of asserts so that each claim is individually
/// visible: the interesting entries are the ones where the intuition and the
/// answer differ, and those are easy to lose inside prose.
///
/// The syntax is harvested from the conformance corpus rather than invented, so
/// a grammar change breaks this file loudly instead of leaving it asserting
/// something about a language nobody speaks.
const CLASSIFIED: &[(&str, Effect)] = &[
    // --- reads -------------------------------------------------------------
    ("SELECT * FROM users;", Effect::Read),
    ("EXPLAIN SELECT * FROM users;", Effect::Read),
    ("INFO FOR TABLE users;", Effect::Read),
    ("GET sessions:'abc';", Effect::Read),
    ("KEYS FROM sessions;", Effect::Read),
    ("READ media:'/logo.png';", Effect::Read),
    // The largest read in the language: it streams the whole log and changes
    // nothing. Sending it to the leader would be a needless hop for the single
    // heaviest statement there is.
    ("BACKUP;", Effect::Read),
    // Session state, not store state. A read-only node must still be able to
    // say which database it is reading, and to group its reads.
    ("USE NAMESPACE prod;", Effect::Read),
    ("BEGIN;", Effect::Read),
    ("COMMIT;", Effect::Read),
    ("CANCEL;", Effect::Read),
    // --- writes: structure -------------------------------------------------
    ("DEFINE NAMESPACE prod;", Effect::Write),
    ("DEFINE DATABASE shop;", Effect::Write),
    ("DEFINE TABLE users;", Effect::Write),
    ("DEFINE SPACE sessions;", Effect::Write),
    ("DEFINE BUCKET media;", Effect::Write),
    ("DEFINE FIELD name ON users TYPE string;", Effect::Write),
    ("DEFINE INDEX by_name ON users FIELDS name;", Effect::Write),
    (
        "DEFINE ANALYZER simple FILTERS lowercase, ascii;",
        Effect::Write,
    ),
    ("DROP TABLE users;", Effect::Write),
    ("DROP FIELD seen_at ON users;", Effect::Write),
    ("DROP INDEX by_name ON users;", Effect::Write),
    // Reads every record to rebuild the entries, and writes all of them. The
    // reading half is why a "does it look like a query" test gets this wrong.
    ("REBUILD INDEX by_name ON users;", Effect::Write),
    // --- writes: who may reach it ------------------------------------------
    (
        "DEFINE USER ada ON prod.shop ROLE editor PASSWORD 'correct horse';",
        Effect::Write,
    ),
    ("DROP USER ada;", Effect::Write),
    ("GRANT read ON users TO nobody;", Effect::Write),
    ("REVOKE read ON users FROM nobody;", Effect::Write),
    // --- topology: the two halves answer differently -------------------------
    // `DEFINE NODE` changes this store and is still a `Read` for routing: it
    // writes the *local* half (ADR-0020 §3), so forwarding it would reconfigure
    // the leader rather than the node the operator addressed. It is also the
    // only statement that can give `WRITABLE` back to a node that dropped it.
    (
        "DEFINE NODE ROLES serving, writable ENDPOINTS 'here:9000';",
        Effect::Read,
    ),
    // The replicated half, and therefore a write: a peer is a catalog record
    // that commits in its transaction and travels through the apply path.
    ("DEFINE REPLICA second AT 'there:9001';", Effect::Write),
    // --- writes: records and files -----------------------------------------
    ("CREATE users:1 = { name: 'ada' };", Effect::Write),
    // Reads to find its targets, then changes them — the shape the criterion
    // names as where a keyword test fails.
    ("UPDATE users:1 SET name = 'ada';", Effect::Write),
    ("DELETE users:1;", Effect::Write),
    ("DELETE FROM users WHERE name = 'ada';", Effect::Write),
    (
        "RELATE users:1->attached->media:'/logo.png';",
        Effect::Write,
    ),
    ("SET sessions:'abc' = 42;", Effect::Write),
    ("DEL sessions:'abc';", Effect::Write),
    ("PUT media:'/logo.png' = 0x89504e47;", Effect::Write),
];

fn effect_of(source: &str) -> Effect {
    Effect::of_script(&parse(source).unwrap())
}

#[test]
fn every_statement_kind_is_classified_the_way_it_behaves() {
    let mut wrong = Vec::new();
    for (source, expected) in CLASSIFIED {
        let got = effect_of(source);
        if got != *expected {
            wrong.push(format!("{source} => {got:?}, expected {expected:?}"));
        }
    }
    assert!(wrong.is_empty(), "misclassified:\n{}", wrong.join("\n"));
}

#[test]
fn a_transaction_that_reads_and_writes_is_a_write() {
    // The criterion's stated trap. Every statement before the `UPDATE` reads,
    // so a first-statement or keyword test answers `Read` and sends this where
    // its write cannot commit.
    let mixed = "BEGIN; SELECT * FROM users; UPDATE users:1 SET name = 'ada'; COMMIT;";
    assert_eq!(effect_of(mixed), Effect::Write);

    // And the same block without the write really is a read, so the rule above
    // is not simply "anything with BEGIN is a write".
    let read_only = "BEGIN; SELECT * FROM users; COMMIT;";
    assert_eq!(effect_of(read_only), Effect::Read);
}

#[test]
fn a_script_is_a_write_wherever_the_write_sits_in_it() {
    // Not just the first statement, and not just the last.
    assert_eq!(
        effect_of("SELECT * FROM users; CREATE users:1 = { name: 'ada' }; SELECT * FROM users;"),
        Effect::Write
    );
}

#[test]
fn a_node_without_the_writable_role_refuses_a_write_and_still_answers_reads() {
    let read_only = Roles::SERVING;

    let write = parse("CREATE users:1 = { name: 'ada' };").unwrap();
    let refused = admits(read_only, &write).unwrap_err();
    assert!(
        matches!(refused, bgv_db_session::Error::NotWritable { .. }),
        "refused for the wrong reason: {refused}"
    );

    // A read-only node that cannot read is not a read-only node.
    let read = parse("SELECT * FROM users;").unwrap();
    assert_eq!(admits(read_only, &read).unwrap(), Effect::Read);
}

#[test]
fn a_node_that_stands_alone_still_takes_writes() {
    // Kill criterion 3 in miniature: the single-node store must not quietly
    // lose the ability to write because routing was added around it.
    let alone = Roles::ALONE;
    let write = parse("CREATE users:1 = { name: 'ada' };").unwrap();
    assert_eq!(admits(alone, &write).unwrap(), Effect::Write);
}
