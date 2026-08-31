//! `CREATE users = { … }` — the write that does not make the caller invent a name.
//!
//! # What the answer has to carry, and why `ok` would be a defect
//!
//! The caller did not choose the identity, cannot derive it, and has no second
//! statement that would find the record again. A write that reported only that
//! it happened would therefore be a write nothing can reach — so the identity
//! is the answer, and that is asserted here by **reading the record back at the
//! identity that came out**, never by checking that some identity came out.
//!
//! # Why the scheme is read from the table and not from the statement
//!
//! Two producers for one question is how a table ends up holding records named
//! under two schemes, after which nothing can say which scheme a missing record
//! was written under. `INSERT` and this statement share one helper, and the
//! test that pins it asks both verbs the same question about the same table.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
DEFINE COLLECTION users;
DEFINE COLLECTION orders;
DEFINE COLLECTION sessions IDENTITY uuid;
",
        )
        .unwrap();
    session
}

fn run(session: &mut Session<'_>, script: &str) -> Outcome {
    session
        .run(script)
        .unwrap_or_else(|error| panic!("{script}: {error}"))
        .pop()
        .expect("one outcome")
}

/// The identities a write answered with.
fn keys(outcome: Outcome) -> Vec<RecordId> {
    match outcome {
        Outcome::Keys(keys) => keys,
        other => panic!("expected the produced identities, got {other:?}"),
    }
}

/// The one identity a single write answered with.
fn key(session: &mut Session<'_>, script: &str) -> RecordId {
    let mut produced = keys(run(session, script));
    assert_eq!(produced.len(), 1, "{script}");
    produced.pop().unwrap()
}

fn field(record: &Value, name: &str) -> Value {
    let Value::Object(fields) = record else {
        panic!("not an object")
    };
    fields.get(name).expect("the field").clone()
}

/// An identity written the way the grammar spells it.
///
/// `to_literal` and not `Display`, and the difference is the point: the store
/// answers with this form, so a test that addressed the record any other way
/// would be checking a path no caller travels. It carried a hand-written
/// `match` on the kind for one wave — written because `Display`'s bare hex
/// reads as a **duration** rather than as an identity — and that `match` was a
/// second spelling authority living in a test file, which is exactly what
/// Q-291 turned out to be about.
fn addressed(table: &str, id: &RecordId) -> String {
    format!("{table}:{}", id.to_literal())
}

/// Read one record back by its identity, and refuse to guess if it is absent.
fn read_back(session: &mut Session<'_>, table: &str, id: &RecordId) -> Value {
    let script = format!("SELECT * FROM {};", addressed(table, id));
    match run(session, &script) {
        Outcome::Records { records, .. } => {
            assert_eq!(records.len(), 1, "{script} found nothing to read");
            records.into_iter().next().unwrap().1
        }
        other => panic!("expected records, got {other:?}"),
    }
}

#[test]
fn the_answer_is_the_identity_and_it_addresses_the_record_that_was_written() {
    let store = store();
    let mut session = ready(&store);

    let id = key(&mut session, "CREATE users = { name: 'ada' };");
    assert_eq!(id, RecordId::Int(1));

    // The assertion that matters: the identity is not merely present, it works.
    let held = read_back(&mut session, "users", &id);
    assert_eq!(field(&held, "name"), Value::from("ada"));
}

#[test]
fn a_table_counts_its_own_records_from_one_and_upwards() {
    let store = store();
    let mut session = ready(&store);

    for expected in 1..=3_i64 {
        let id = key(&mut session, "CREATE users = { name: 'ada' };");
        assert_eq!(id, RecordId::Int(expected));
    }
}

/// Two tables counting independently, which is why the counter is per table.
///
/// A store-wide counter would pass every assertion above and leave both tables
/// full of gaps — and a gap in a sequence reads as a record somebody deleted.
#[test]
fn two_tables_do_not_share_a_counter() {
    let store = store();
    let mut session = ready(&store);

    assert_eq!(
        key(&mut session, "CREATE users = { name: 'ada' };"),
        RecordId::Int(1)
    );
    assert_eq!(
        key(&mut session, "CREATE orders = { total: 5 };"),
        RecordId::Int(1)
    );
    assert_eq!(
        key(&mut session, "CREATE users = { name: 'grace' };"),
        RecordId::Int(2)
    );
}

/// A declared scheme is honoured, and the default is the counter.
///
/// The owner's rule stated as a test: a `u64` unless the table said `uuid`.
#[test]
fn a_table_that_declared_uuid_is_named_with_one_and_an_undeclared_table_is_not() {
    let store = store();
    let mut session = ready(&store);

    let declared = key(&mut session, "CREATE sessions = { token: 'abc' };");
    assert!(
        matches!(declared, RecordId::Uuid(_)),
        "a table declared IDENTITY uuid should mint one, got {declared:?}"
    );
    // And it still addresses the record, which a UUID rendered wrongly would not.
    let held = read_back(&mut session, "sessions", &declared);
    assert_eq!(field(&held, "token"), Value::from("abc"));

    let undeclared = key(&mut session, "CREATE users = { name: 'ada' };");
    assert_eq!(undeclared, RecordId::Int(1));
}

/// `INSERT` reads the same declaration, through the same helper.
///
/// This is the assertion the change was made for: before it, `INSERT` minted a
/// UUID into every table, including one whose declaration says `int`. Two verbs
/// naming records into one table under two schemes is not a cosmetic
/// disagreement — afterwards nothing can say which scheme a missing record was
/// written under.
#[test]
fn insert_and_create_name_records_in_one_table_the_same_way() {
    let store = store();
    let mut session = ready(&store);

    let created = key(&mut session, "CREATE users = { name: 'ada' };");
    let inserted = keys(run(
        &mut session,
        "INSERT INTO users (name) VALUES ('grace');",
    ));
    assert_eq!(created, RecordId::Int(1));
    assert_eq!(inserted, vec![RecordId::Int(2)]);

    let declared = keys(run(
        &mut session,
        "INSERT INTO sessions (token) VALUES ('abc');",
    ));
    assert!(
        matches!(declared.first(), Some(RecordId::Uuid(_))),
        "INSERT into a table declared IDENTITY uuid should mint one, got {declared:?}"
    );
}

/// `RETURN AFTER` answers the record, as it does for the addressed form.
#[test]
fn return_after_answers_the_record_it_wrote() {
    let store = store();
    let mut session = ready(&store);

    match run(&mut session, "CREATE users = { name: 'ada' } RETURN AFTER;") {
        Outcome::Value(held) => assert_eq!(field(&held, "name"), Value::from("ada")),
        other => panic!("expected the record, got {other:?}"),
    }
}

/// The addressed form is untouched, and the two forms share one counter.
///
/// A caller who names `users:9` has not advanced anything the store owns, so the
/// next generated identity is still the counter's — which is exactly the
/// collision this asserts does **not** happen quietly: a store that seeded the
/// counter from the highest identity it had seen would answer `10` here, and a
/// store that ignored the write entirely would answer `1`. Only one of those is
/// this design, and it is the one where a named identity and a generated one
/// live in the same table without either knowing about the other.
#[test]
fn a_named_identity_neither_advances_nor_blocks_the_counter() {
    let store = store();
    let mut session = ready(&store);

    run(&mut session, "CREATE users:9 = { name: 'named' };");
    assert_eq!(
        key(&mut session, "CREATE users = { name: 'ada' };"),
        RecordId::Int(1)
    );

    // Both records are there, and neither replaced the other.
    assert_eq!(
        field(&read_back(&mut session, "users", &RecordId::Int(9)), "name"),
        Value::from("named")
    );
    assert_eq!(
        field(&read_back(&mut session, "users", &RecordId::Int(1)), "name"),
        Value::from("ada")
    );
}

/// The counter walks past identities the caller named, rather than refusing.
///
/// This is the assertion that keeps a table writable. A refusal discards the
/// counter's advance along with the rest of the transaction, so a store that
/// refused here would refuse **again** on the next attempt and on every attempt
/// after it: a table whose low identities were imported could never again take a
/// generated write. The failure is not a bad error message, it is a dead table,
/// and nothing in the type system or in a happy-path test would have said so.
#[test]
fn the_counter_walks_past_a_named_identity_instead_of_getting_stuck_on_it() {
    let store = store();
    let mut session = ready(&store);

    session
        .run("CREATE users:1 = { name: 'one' }; CREATE users:2 = { name: 'two' };")
        .unwrap();

    // Not a refusal, and not `1`.
    assert_eq!(
        key(&mut session, "CREATE users = { name: 'ada' };"),
        RecordId::Int(3)
    );
    // And the walk is not repeated: the counter kept what it spent.
    assert_eq!(
        key(&mut session, "CREATE users = { name: 'grace' };"),
        RecordId::Int(4)
    );
    // Both named records are untouched — walking past is not writing over.
    assert_eq!(
        field(&read_back(&mut session, "users", &RecordId::Int(1)), "name"),
        Value::from("one")
    );
    assert_eq!(
        field(&read_back(&mut session, "users", &RecordId::Int(2)), "name"),
        Value::from("two")
    );
}

/// Naming an identity the store already produced is still refused.
///
/// The walk goes one way only. It keeps the *store* from colliding with the
/// caller; it does not license the caller to collide with the store, because
/// there the caller made a claim — "no record holds this" — and it was false.
#[test]
fn naming_an_identity_the_store_produced_is_still_refused() {
    let store = store();
    let mut session = ready(&store);

    assert_eq!(
        key(&mut session, "CREATE users = { name: 'ada' };"),
        RecordId::Int(1)
    );
    let refused = session
        .run("CREATE users:1 = { name: 'someone else' };")
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("already exists"),
        "expected a duplicate refusal, got: {refused}"
    );
}

/// A generated write into a bucket is refused, as an addressed one already was.
///
/// A bucket's records describe bytes the store holds, so one written by hand can
/// lie about them. The refusal lives on `Session::writable`, which takes a
/// record target — and this statement names no record, so the guard had to be
/// reached a second way. A guard reached one way out of two is not a guard.
#[test]
fn a_bucket_still_refuses_a_record_written_by_hand() {
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE BUCKET media;").unwrap();

    let refused = session
        .run("CREATE media = { size: 1 };")
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("media"),
        "the refusal should name the bucket: {refused}"
    );
}

/// The counter survives the session that advanced it.
///
/// Not a restart — `catalog.rs` owns that, and proves it by reopening the store
/// — but the cheap end of the same property, and the one that would break first
/// if the counter were ever cached in a session rather than read from the
/// catalog.
#[test]
fn a_second_session_continues_the_count_rather_than_restarting_it() {
    let store = store();
    {
        let mut session = ready(&store);
        assert_eq!(
            key(&mut session, "CREATE users = { name: 'ada' };"),
            RecordId::Int(1)
        );
    }
    let mut session = Session::new(&store);
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    assert_eq!(
        key(&mut session, "CREATE users = { name: 'grace' };"),
        RecordId::Int(2)
    );
}
