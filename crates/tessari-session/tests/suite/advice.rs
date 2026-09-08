//! Refusals that say what to write next.
//!
//! # Why nothing here asserts the wording
//!
//! A refusal that carries a statement is making a promise no message has to
//! make: that the text is *runnable*. Asserting the text only proves the
//! formatter did what the formatter does, and it would have passed just as
//! happily on `DEFINE FIELD nickname ON 7 TYPE string` — the message this node
//! exists to replace, which names a table by an id the language cannot spell.
//!
//! So the suggestion is taken out of the refusal, **run**, and then the write
//! that was refused is repeated. Nothing but a correct statement passes that,
//! and a wrong one fails at the place it is wrong instead of at an assertion
//! somebody would have to re-derive.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_ql::Parameters;
use tessari_session::{Error, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// A store holding one strict table with one declared field.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE people (name string);",
        )
        .unwrap();
    session
}

/// The write every test here is refused for, and repeats afterwards.
const UNDECLARED: &str = "CREATE people:1 = { name: 'ada', nickname: 'the countess' };";

#[test]
fn the_refusal_names_its_table_the_way_a_declaration_would() {
    let store = store();
    let mut session = ready(&store);

    let refusal = session.run(UNDECLARED).unwrap_err().to_string();

    assert!(
        refusal.contains("people"),
        "the refusal does not name its table: {refusal}"
    );
    // The id this refusal used to carry. A caller cannot write it anywhere, and
    // its presence would mean the name was added beside it rather than instead
    // of it.
    assert!(
        !refusal.contains("table 1 ") && !refusal.contains("table 2 "),
        "the refusal still names its table by id: {refusal}"
    );
}

#[test]
fn every_declaration_refusal_names_its_table_the_way_a_declaration_would() {
    // The three siblings of the refusal above, which carried the id for as long
    // as a doc comment beside them explained that the layer had nothing else —
    // while the arm two lines away read the name off the same `TableSchema`.
    //
    // Both halves of the assertion were checked by reading the messages rather
    // than by inferring them — mutate the count below to 4 and the panic prints
    // all three:
    //
    //   table ledgers declares balance as int, but record 1 holds string there
    //   record 2 of table ledgers holds a balance its declaration refuses
    //   record 3 in table ledgers leaves required field holder holding none
    //
    // The record half renders as a bare id, so `contains("ledgers")` can only be
    // satisfied by the table half — the one that was wrong. The second assertion
    // is the direction that matters more: the id could have been added *beside*
    // the name rather than replaced by it, and only its absence rules that out.
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE ledgers (balance int ASSERT $value >= 0, \
             holder string REQUIRED);",
        )
        .unwrap();

    let refusals = [
        // SchemaViolation — a declared field holding the wrong type.
        "CREATE ledgers:1 = { holder: 'ada', balance: 'plenty' };",
        // AssertionViolation — the right type, refused by the declaration.
        "CREATE ledgers:2 = { holder: 'ada', balance: -1 };",
        // MissingRequiredField — a required field holding nothing.
        "CREATE ledgers:3 = { balance: 1 };",
    ]
    .map(|statement| {
        session
            .run(statement)
            .expect_err("a write the declaration refuses was accepted")
            .to_string()
    });

    for refusal in &refusals {
        assert!(
            refusal.contains("ledgers"),
            "the refusal does not name its table: {refusal}"
        );
        assert!(
            !refusal.contains("table 1")
                && !refusal.contains("table 2")
                && !refusal.contains("table 3"),
            "the refusal still names its table by id: {refusal}"
        );
    }

    // And the three are three different refusals rather than the same one
    // reached three times — without this the loop above passes on a store that
    // refuses every write for one reason none of these tests is about.
    let distinct: std::collections::BTreeSet<&String> = refusals.iter().collect();
    assert_eq!(distinct.len(), 3, "{refusals:?}");
}

#[test]
fn the_suggested_declaration_makes_the_refused_write_succeed() {
    let store = store();
    let mut session = ready(&store);

    let Error::UndeclaredField { suggestion, .. } = session.run(UNDECLARED).unwrap_err() else {
        panic!("the refusal carried no statement to run");
    };

    // Verbatim. A suggestion that needs editing before it runs is a suggestion
    // that did not answer the question the caller asked.
    session
        .run(&format!("{suggestion};"))
        .unwrap_or_else(|error| {
            panic!("the suggested statement did not run: {error} — {suggestion}")
        });
    session
        .run(UNDECLARED)
        .unwrap_or_else(|error| panic!("the write is still refused after its own remedy: {error}"));
}

#[test]
fn the_suggestion_declares_a_kind_that_accepts_what_was_sent() {
    // The kind is read off the value, so a second value of a second type has to
    // produce a second declaration. One test with one string would pass against
    // an implementation that always writes `TYPE string`.
    let store = store();
    let mut session = ready(&store);

    let Error::UndeclaredField { suggestion, .. } = session
        .run("CREATE people:2 = { name: 'grace', born: 1906 };")
        .unwrap_err()
    else {
        panic!("the refusal carried no statement to run");
    };

    session.run(&format!("{suggestion};")).unwrap();
    session
        .run("CREATE people:2 = { name: 'grace', born: 1906 };")
        .unwrap_or_else(|error| panic!("a number was declared as something else: {error}"));
}

#[test]
fn a_field_the_language_cannot_spell_is_refused_without_a_suggestion() {
    // A record's fields do not have to come from a script: this one arrives
    // through a bound parameter, so its name is whatever the caller's own data
    // holds. `select` is a word the parser reads as a keyword and refuses in a
    // name position, so the statement naming it would not read back — and a
    // suggestion that does not run is worse than none, because it looks like
    // something to paste.
    let store = store();
    let mut session = ready(&store);

    let mut row = BTreeMap::new();
    row.insert("name".to_owned(), Value::from("ada"));
    row.insert("select".to_owned(), Value::from("everything"));
    let mut parameters = Parameters::new();
    parameters.insert("row".to_owned(), Value::Object(row));

    let refusal = session
        .run_with("CREATE people:3 = $row;", &parameters)
        .unwrap_err();

    assert!(
        !matches!(refusal, Error::UndeclaredField { .. }),
        "a statement was suggested that the parser would refuse: {refusal}"
    );
    let text = refusal.to_string();
    assert!(
        text.contains("select") && text.contains("people"),
        "the refusal stopped saying what was wrong: {text}"
    );
}

#[test]
fn a_refused_batch_names_every_row_that_was_wrong() {
    // The natural implementation stops at the first bad row, and it is not
    // wrong to: the commit is all-or-nothing, so the first refusal already
    // decides the outcome. It is the shape that makes a caller fix a batch one
    // round trip per mistake — which is why three bad rows have to produce
    // three refusals in one answer.
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE readings (n int);",
        )
        .unwrap();

    let refusal = session
        .run("INSERT INTO readings (n) VALUES (1), ('two'), (3), ('four'), ('five');")
        .unwrap_err()
        .to_string();

    // Three refusals, and three *different* records — the count alone would be
    // satisfied by naming one row three times, which is what the first attempt
    // at this actually did (both validation passes saw the same row).
    assert!(
        refusal.contains("3 records"),
        "the refusal does not count what it refused: {refusal}"
    );
    let named: std::collections::BTreeSet<&str> = refusal
        .split("but record ")
        .skip(1)
        .filter_map(|tail| tail.split(' ').next())
        .collect();
    assert_eq!(
        named.len(),
        3,
        "the refusal does not name three distinct records: {refusal}"
    );
}

#[test]
fn one_bad_row_is_refused_the_way_it_always_was() {
    // The counterpart, and the reason the plural form is not simply always
    // used: a batch of one is not a batch, and every corpus row and every test
    // reading the singular refusal is reading something still true.
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE readings (n int);",
        )
        .unwrap();

    let refusal = session
        .run("INSERT INTO readings (n) VALUES (1), ('two'), (3);")
        .unwrap_err()
        .to_string();

    assert!(
        !refusal.contains("records were refused"),
        "a single refusal was wrapped as a batch: {refusal}"
    );
    assert!(
        refusal.contains("int") && refusal.contains("string"),
        "the refusal stopped saying what disagreed with what: {refusal}"
    );
}

#[test]
fn a_rehearsed_write_is_refused_in_exactly_the_words_the_real_one_is() {
    // The whole value of a rehearsal is that it agrees with the performance, and
    // the way to lose that is a second validation path — which would agree until
    // it did not, silently, in the direction somebody trusted. So the two
    // messages are compared rather than each being asserted against a pattern
    // both could satisfy while differing.
    let store = store();
    let mut session = ready(&store);

    let rehearsed = session
        .run(&format!("BEGIN; {UNDECLARED} VERIFY;"))
        .unwrap_err()
        .to_string();
    let performed = session.run(UNDECLARED).unwrap_err().to_string();

    assert_eq!(
        rehearsed, performed,
        "the rehearsal and the write disagree about the same record"
    );
}

#[test]
fn a_rehearsal_that_passes_writes_nothing() {
    let store = store();
    let mut session = ready(&store);

    session
        .run("BEGIN; CREATE people:1 = { name: 'ada' }; VERIFY;")
        .unwrap_or_else(|error| panic!("a write that should be accepted was refused: {error}"));

    let after = format!("{:?}", session.run("SELECT * FROM people;").unwrap());
    assert!(
        !after.contains("ada"),
        "the rehearsal committed the record it was rehearsing: {after}"
    );
}

#[test]
fn a_rehearsal_reports_a_refusal_that_only_the_index_would_raise() {
    // `UniqueViolation` comes out of index maintenance, not schema validation,
    // and it is the refusal a caller most wants to rehearse. A dry run that
    // stopped after the schema check would report success here — which is the
    // exact failure a rehearsal exists to prevent, arriving through the
    // rehearsal itself.
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE people (name string) SCHEMALESS;\n\
             DEFINE INDEX one_name ON people FIELDS name UNIQUE;\n\
             CREATE people:1 = { name: 'ada' };",
        )
        .unwrap();

    let refusal = session
        .run("BEGIN; CREATE people:2 = { name: 'ada' }; VERIFY;")
        .unwrap_err()
        .to_string();

    assert!(
        refusal.contains("ada") || refusal.to_lowercase().contains("unique"),
        "the rehearsal did not reach the index: {refusal}"
    );
}

#[test]
fn a_rehearsal_without_a_transaction_is_refused_like_its_siblings() {
    let store = store();
    let mut session = ready(&store);

    let refusal = session.run("VERIFY;").unwrap_err();

    assert!(
        matches!(refusal, Error::NoOpenTransaction { .. }),
        "a rehearsal outside a transaction was not refused as one: {refusal}"
    );
}

#[test]
fn the_refusal_names_no_field_the_caller_did_not_send() {
    // ADR-0044. The useful-looking suggestion is the nearest declared field —
    // and a declared field is one this caller's grants may hide, which is why
    // the remedy is built only out of what they just wrote.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE FIELD salary ON people TYPE int;")
        .unwrap();

    let refusal = session
        .run("CREATE people:4 = { name: 'ada', slary: 1 };")
        .unwrap_err()
        .to_string();

    assert!(
        !refusal.contains("salary"),
        "the refusal named a field of the schema: {refusal}"
    );
}
