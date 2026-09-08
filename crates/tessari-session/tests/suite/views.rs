//! A name for a read, and the four things that decide whether it is a good one.
//!
//! The properties asserted here are the ones the design document commits to, in
//! its own words, because a design's claim that nobody ran is a sentence rather
//! than a property:
//!
//! - a view answers through **its own** projection, and a condition written
//!   outside it sees what it answered rather than the base record;
//! - a chain of views is **bounded**, and a cycle is the same bound seen from
//!   the other side;
//! - a view resolves against the **caller's** permissions, so a caller granted
//!   the view and nothing else is refused **naming the base table**;
//! - a view is a name for a read and not a store, so everything that writes,
//!   addresses a key or builds an index over one is refused.
//!
//! # The permissions test asserts the table the refusal names, and that is not
//! decoration
//!
//! A caller granted neither the view nor the table is refused either way, so a
//! test asserting only *refused* passes whatever the code does. This one grants
//! the caller `read` **on the view** and nothing else: under the ordering the
//! design's first draft described — expanding while the read runs, after the
//! grant check — that statement would have **succeeded**, because the check
//! would have seen one table, the view's, and found it granted. Asserting the
//! name in the refusal is what tells the two apart.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

const PASSWORD: &str = "correct horse battery";

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// A database holding four people, two of them in engineering.
fn peopled(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE staff SCHEMALESS;\n\
             CREATE staff:1 = { name: 'ada', team: 'eng', salary: 100 };\n\
             CREATE staff:2 = { name: 'bo', team: 'sales', salary: 90 };\n\
             CREATE staff:3 = { name: 'cy', team: 'eng', salary: 120 };\n\
             CREATE staff:4 = { name: 'di', team: 'sales', salary: 80 };",
        )
        .unwrap();
    session
}

/// One statement's outcome.
fn run(session: &mut Session<'_>, script: &str) -> Outcome {
    session.run(script).unwrap().pop().unwrap()
}

/// The refusal one statement raises.
fn refused(session: &mut Session<'_>, script: &str) -> String {
    match session.run(script) {
        Err(why) => why.to_string(),
        Ok(outcome) => panic!("expected a refusal, got {outcome:?}"),
    }
}

/// The records an outcome answered with, as `(id, record)` pairs.
fn records(outcome: &Outcome) -> Vec<(String, Value)> {
    match outcome {
        Outcome::Records { records, .. } => records
            .iter()
            .map(|(id, record)| (id.to_string(), record.clone()))
            .collect(),
        other => panic!("expected records, got {other:?}"),
    }
}

/// The identities an outcome answered with, in order.
fn identities(outcome: &Outcome) -> Vec<String> {
    records(outcome).into_iter().map(|(id, _)| id).collect()
}

#[test]
fn a_view_answers_what_its_read_answers() {
    let store = store();
    let mut session = peopled(&store);
    session
        .run("DEFINE VIEW engineers AS SELECT * FROM staff WHERE team = 'eng';")
        .unwrap();
    // Compared against the read itself rather than against a list written out
    // here, because that is literally the claim: a view answers what its read
    // answers. A hand-written expectation would pass a view that answered
    // correctly for the wrong reason and fail one that was right about a
    // question this test had not thought of.
    assert_eq!(
        records(&run(&mut session, "SELECT * FROM engineers;")),
        records(&run(
            &mut session,
            "SELECT * FROM staff WHERE team = 'eng';"
        )),
        "a view answered something other than its read"
    );
}

#[test]
fn a_view_is_read_through_its_own_projection() {
    let store = store();
    let mut session = peopled(&store);
    session
        .run("DEFINE VIEW roster AS SELECT name, team FROM staff;")
        .unwrap();
    let answered = records(&run(&mut session, "SELECT * FROM roster;"));
    assert_eq!(answered.len(), 4, "the view lost records: {answered:?}");
    for (id, record) in &answered {
        let Value::Object(fields) = record else {
            panic!("expected an object for {id}, got {record:?}");
        };
        // The star belongs to the *outer* read and expands over what the view
        // answered, so a field the view projected away is gone rather than
        // restored from the base record.
        assert_eq!(
            fields.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["name", "team"],
            "the outer star reached past the view's projection for {id}"
        );
    }
}

#[test]
fn a_condition_over_a_view_sees_the_view_and_not_the_base_record() {
    let store = store();
    let mut session = peopled(&store);
    session
        .run("DEFINE VIEW roster AS SELECT name, team FROM staff;")
        .unwrap();
    // `salary` is a real field of `staff` and is not one of `roster`. Merging
    // this condition into the view's own source would test it against the base
    // record and answer three people; over the view it can only see `none`,
    // which no number is greater than.
    assert_eq!(
        identities(&run(
            &mut session,
            "SELECT * FROM roster WHERE salary > 85;"
        )),
        Vec::<String>::new(),
        "a condition outside the view was tested against the base record"
    );
    // The control, so the test cannot pass because the condition never ran: a
    // field the view *does* answer with filters exactly as it should.
    assert_eq!(
        identities(&run(
            &mut session,
            "SELECT * FROM roster WHERE team = 'eng';"
        )),
        vec!["1".to_owned(), "3".to_owned()],
        "a condition over a projected field did not filter"
    );
}

#[test]
fn a_view_naming_a_view_is_expanded() {
    let store = store();
    let mut session = peopled(&store);
    session
        .run(
            "DEFINE VIEW engineers AS SELECT * FROM staff WHERE team = 'eng';\n\
             DEFINE VIEW senior_engineers AS SELECT * FROM engineers WHERE salary > 110;",
        )
        .unwrap();
    assert_eq!(
        identities(&run(&mut session, "SELECT * FROM senior_engineers;")),
        vec!["3".to_owned()],
        "a view over a view did not answer its own read"
    );
}

#[test]
fn a_view_chain_past_the_depth_is_refused_naming_the_chain() {
    let store = store();
    let mut session = peopled(&store);
    session
        .run("DEFINE VIEW v0 AS SELECT * FROM staff;")
        .unwrap();
    // `v0` is one view; `v8` is nine, which is one past the bound. Built by name
    // so the chain is legible in the refusal rather than only in the count.
    for step in 1..=8 {
        session
            .run(&format!(
                "DEFINE VIEW v{step} AS SELECT * FROM v{};",
                step - 1
            ))
            .unwrap();
    }
    let refusal = refused(&mut session, "SELECT * FROM v8;");
    assert!(
        refusal.contains("nested more than 8 deep"),
        "the refusal did not name the bound: {refusal}"
    );
    assert!(
        refusal.contains("v8 -> v7"),
        "the refusal did not print the chain it followed: {refusal}"
    );
    // The bound is a real ceiling and not an off-by-one that refuses everything:
    // the chain one shorter answers.
    assert_eq!(
        identities(&run(&mut session, "SELECT * FROM v7;")).len(),
        4,
        "a chain inside the bound was refused"
    );
}

#[test]
fn a_view_that_names_itself_is_refused_by_the_same_bound() {
    let store = store();
    let mut session = peopled(&store);
    // Accepted at definition — nothing is resolved there, the same rule a
    // field's `DEFAULT` follows — and refused on the first read.
    session
        .run("DEFINE VIEW loop_ AS SELECT * FROM loop_;")
        .unwrap();
    let refusal = refused(&mut session, "SELECT * FROM loop_;");
    assert!(
        refusal.contains("nested more than 8 deep"),
        "a self-naming view was refused by something other than the depth bound: {refusal}"
    );
}

#[test]
fn a_caller_granted_the_view_and_not_the_table_is_refused_naming_the_table() {
    let store = store();
    let mut session = peopled(&store);
    session
        .run(
            "DEFINE VIEW engineers AS SELECT * FROM staff WHERE team = 'eng';\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();
    let mut root = Session::new(&store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run(
        "USE NAMESPACE prod; USE DATABASE shop;\n\
         DEFINE TABLE audit SCHEMALESS;\n\
         DEFINE USER caller ON prod.shop ROLE editor PASSWORD 'correct horse battery';\n\
         DEFINE USER definer ON prod.shop ROLE editor PASSWORD 'correct horse battery';\n\
         GRANT read ON audit TO caller;\n\
         GRANT read ON staff TO definer;",
    )
    .unwrap();

    // **A grant on a view is refused, and that follows from the answer rather
    // than limiting it.** A view is replaced by its read before anything is
    // authorized, so no grant on a view is ever consulted; accepting one would
    // store a permission that does nothing and read, to whoever wrote it, like
    // a permission that does something.
    let refusal = refused(&mut root, "GRANT read ON engineers TO caller;");
    assert!(
        refusal.contains("is a view"),
        "a grant was stored against a view: {refusal}"
    );

    let mut caller = Session::new(&store);
    caller.sign_in("caller", PASSWORD).unwrap();
    caller
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    let refusal = refused(&mut caller, "SELECT * FROM engineers;");
    assert!(
        refusal.contains("staff"),
        "the refusal did not name the table the view reads: {refusal}"
    );

    // The other half, and it is what makes the first half mean something: the
    // same statement, the same view, a caller granted the base table instead —
    // and it answers.
    let mut definer = Session::new(&store);
    definer.sign_in("definer", PASSWORD).unwrap();
    definer
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    assert_eq!(
        identities(&run(&mut definer, "SELECT * FROM engineers;")),
        vec!["1".to_owned(), "3".to_owned()],
        "a caller granted the base table could not read through the view"
    );
}

#[test]
fn a_view_with_no_limit_past_the_ceiling_is_refused_rather_than_shortened() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE wide SCHEMALESS;",
        )
        .unwrap();
    // One past the ceiling a held read runs under. Written as one script so the
    // cost is a single commit rather than ten thousand.
    let mut script = String::new();
    for n in 1..=10_001 {
        script.push_str(&format!("CREATE wide:{n} = {{ n: {n} }};\n"));
    }
    session.run(&script).unwrap();
    session
        .run("DEFINE VIEW everything AS SELECT * FROM wide;")
        .unwrap();
    let refusal = refused(&mut session, "SELECT * FROM everything LIMIT 1;");
    assert!(
        refusal.contains("10000"),
        "the refusal did not name the ceiling it reached: {refusal}"
    );
    // A view that states its own bound is not touched by the ceiling, which is
    // the escape the refusal is pointing at.
    session
        .run("DEFINE VIEW first_ten AS SELECT * FROM wide LIMIT 10;")
        .unwrap();
    assert_eq!(
        identities(&run(&mut session, "SELECT * FROM first_ten;")).len(),
        10,
        "a view naming its own bound was refused"
    );
}

#[test]
fn a_view_is_not_a_store_and_refuses_everything_that_treats_it_as_one() {
    let store = store();
    let mut session = peopled(&store);
    session
        .run("DEFINE VIEW engineers AS SELECT * FROM staff WHERE team = 'eng';")
        .unwrap();
    for statement in [
        "CREATE engineers:9 = { name: 'ev' };",
        "UPDATE engineers:1 = { name: 'ev' };",
        "DELETE engineers:1;",
        "INSERT INTO engineers (name) VALUES ('ev');",
        // Each of these is a **keyspace** address, and a view has no keyspace.
        // Left unexpanded on purpose so the refusal comes from the resolution
        // and says what a view is, rather than answering nothing from an empty
        // prefix — which is the one failure that would look like a correct
        // empty result.
        "SELECT * FROM engineers:1;",
        "SELECT * FROM engineers:1..3;",
        "DEFINE INDEX by_name ON engineers FIELDS name;",
        "DEFINE FIELD name ON engineers TYPE string;",
    ] {
        let refusal = refused(&mut session, statement);
        assert!(
            refusal.contains("is a view"),
            "`{statement}` was refused for the wrong reason: {refusal}"
        );
    }
}

#[test]
fn a_view_round_trips_through_info() {
    let store = store();
    let mut session = peopled(&store);
    let written = "SELECT name, team FROM staff WHERE team = 'eng'";
    session
        .run(&format!("DEFINE VIEW engineers AS {written};"))
        .unwrap();
    let Outcome::Value(Value::Object(report)) = run(&mut session, "INFO FOR TABLE engineers;")
    else {
        panic!("expected a report");
    };
    // The read comes back **character for character**, which is the whole reason
    // it is stored as text: a re-rendered statement that happens to mean the
    // same thing is a different statement, and a reader comparing what they
    // wrote to what the store reports should find them equal.
    assert_eq!(
        report.get("view"),
        Some(&Value::from(written)),
        "the stored read was not reported as written: {report:?}"
    );
    assert_eq!(
        report.get("definition"),
        Some(&Value::from(
            format!("DEFINE VIEW engineers AS {written};\n").as_str()
        )),
        "the declaration did not re-execute to the same view: {report:?}"
    );
}

#[test]
fn a_view_cannot_take_the_name_of_a_table_and_a_table_cannot_take_a_view_s() {
    let store = store();
    let mut session = peopled(&store);
    // One namespace, so a name means one thing. This is what makes the whole
    // feature safe to add to a store that already has tables: no name anybody
    // has written can start resolving to something else.
    let refusal = refused(&mut session, "DEFINE VIEW staff AS SELECT * FROM staff;");
    assert!(
        refusal.contains("staff"),
        "a view took a table's name: {refusal}"
    );
    session
        .run("DEFINE VIEW engineers AS SELECT * FROM staff WHERE team = 'eng';")
        .unwrap();
    let refusal = refused(&mut session, "DEFINE TABLE engineers SCHEMALESS;");
    assert!(
        refusal.contains("engineers"),
        "a table took a view's name: {refusal}"
    );
    // And a repeat definition is refused rather than replacing, which is what
    // makes a view immutable while it exists — the property the expansion leans
    // on when it authorizes the tree it is about to run.
    let refusal = refused(
        &mut session,
        "DEFINE VIEW engineers AS SELECT * FROM staff WHERE team = 'sales';",
    );
    assert!(
        refusal.contains("engineers"),
        "a repeat definition replaced instead of refusing: {refusal}"
    );
}

#[test]
fn dropping_a_view_removes_the_definition_and_nothing_else() {
    let store = store();
    let mut session = peopled(&store);
    session
        .run("DEFINE VIEW engineers AS SELECT * FROM staff WHERE team = 'eng';")
        .unwrap();
    session.run("DROP VIEW engineers;").unwrap();
    let refusal = refused(&mut session, "SELECT * FROM engineers;");
    assert!(
        refusal.contains("engineers"),
        "a dropped view was still readable: {refusal}"
    );
    // The records the view read are untouched, which is the whole of what
    // dropping one costs.
    assert_eq!(
        identities(&run(&mut session, "SELECT * FROM staff;")).len(),
        4,
        "dropping a view took records with it"
    );
    // And the word refuses a name of another kind, so the statement's own word
    // stays the most reliable thing about it.
    let refusal = refused(&mut session, "DROP VIEW staff;");
    assert!(
        refusal.contains("staff"),
        "`DROP VIEW` removed a table: {refusal}"
    );
}
