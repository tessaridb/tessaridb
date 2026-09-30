//! A store written as TessariQL and rebuilt from it (ADR-0091).
//!
//! The claims: a script restores into an empty store with no refusal; the
//! rebuilt store answers every read the source answers, on every engine and for
//! every value kind; its users sign in with the passwords they always had; and
//! whatever the script does not carry is named in its header — never dropped in
//! silence.

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;

const PASSWORD: &str = "a long one";

/// Every kind a script writes, with data in each, and two users.
const EVERYTHING: &str = r#"
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE NAMESPACE empty;
DEFINE DATABASE orders; USE DATABASE orders;
DEFINE ANALYZER simple FILTERS lowercase, ascii;
DEFINE TABLE people SCHEMALESS;
DEFINE FIELD bio ON people TYPE string ANALYZER simple;
DEFINE INDEX by_email ON people FIELDS email UNIQUE;
DEFINE INDEX by_city ON people FIELDS address.city;
DEFINE INDEX by_bio ON people FIELDS bio SEARCH;
DEFINE INDEX by_at ON people FIELDS at VECTOR euclidean;
CREATE people:1 = { name: 'ada', email: 'a@x', bio: 'lock contention on the write path', address: { city: 'london' }, at: [0.0, 0.0] };
CREATE people:2 = { name: 'grace', email: 'b@x', bio: 'a compiler and a lock', address: { city: 'york' }, at: [1.0, 0.0] };
CREATE people:'three' = { name: 'edith', email: 'c@x', bio: 'nothing about locks', address: { city: 'london' }, at: [5.0, 0.0] };
CREATE people:4 = { name: 'gone', email: 'd@x' };
DELETE people:4;
DEFINE COLLECTION kinds;
CREATE kinds:1 = { absent: NONE, empty: NULL, yes: true, whole: 42, negative: -7, real: 1.5, exact: dec 12.34, single: 'text', tricky: 'it\'s a \\ and a ; and a newline', raw: 0x0a1b, span: 1h30m, at: datetime '1970-01-01T00:00:00Z', who: uuid '00112233-4455-6677-8899-aabbccddeeff', list: [1, 'two', [3]], nested: { inner: { deeper: 1 } }, keyed: { 'with space': 1 }, unique: set [1, 2, 3], place: geometry { type: 'Point', coordinates: [2.35, 48.85] }, who_ref: people:1 };
DEFINE TABLE follows EDGE;
RELATE people:1->follows->people:2;
DEFINE SPACE cache;
SET cache:'forever' = { v: 1 };
SET cache:'soon' = 'x' EXPIRE 1h;
DEFINE BUCKET media;
PUT media:'/logo.png' = 0x89504e470d0a1a0a;
DEFINE QUEUE jobs TIMEOUT 30s;
CREATE jobs:1 = { job: 'send' };
DEFINE TOPIC events;
CREATE events:'e1' = { kind: 'paid' };
DEFINE VECTOR embeddings DIMENSION 2 DISTANCE cosine;
CREATE embeddings:1 = { vector: [0.6, 0.8] };
DEFINE GEO places;
CREATE places:1 = { geometry: geometry { type: 'Point', coordinates: [2.35, 48.85] } };
BEGIN;
DEFINE USER root ROLE owner PASSWORD 'a long one';
DEFINE USER bea ON prod.orders ROLE viewer PASSWORD 'a long one';
GRANT read ON people FIELDS name, email TO bea;
COMMIT;"#;

const INTERROGATION: &[&str] = &[
    "SELECT * FROM people;",
    "SELECT * FROM people WHERE email = 'b@x';",
    "SELECT * FROM people WHERE address.city = 'london';",
    "SELECT * FROM people WHERE bio MATCHES 'lock';",
    "SELECT name, search::score(bio, 'lock compiler') AS relevance FROM people WHERE bio MATCHES 'lock' ORDER BY relevance DESC;",
    "SELECT * FROM people ORDER BY vector::euclidean(at, [0.5, 0.0]) LIMIT 2;",
    "SELECT * FROM kinds;",
    "SELECT * FROM people:1->follows->people;",
    "GET cache:'forever';",
    "KEYS FROM cache;",
    // `updated` becomes the time the file is written again, as the header says.
    "SELECT size, chunks FROM media;",
    "READ media:'/logo.png';",
    "SELECT * FROM jobs;",
    "SELECT * FROM events;",
    "SELECT * FROM embeddings;",
    "SELECT * FROM places;",
];

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

fn signed_in<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, PASSWORD).unwrap();
    session
        .run("USE NAMESPACE prod; USE DATABASE orders;")
        .unwrap();
    session
}

/// A store with the fixture applied, and the script `BACKUP SCRIPT` answers.
fn original() -> (Store, String) {
    let source = store();
    Session::new(&source).run(EVERYTHING).unwrap();
    let answered = signed_in(&source, "root").run("BACKUP SCRIPT;").unwrap();
    let Some(Outcome::Value(tessari_types::Value::String(script))) = answered.last() else {
        panic!("BACKUP SCRIPT answered {answered:?}");
    };
    (source, script.clone())
}

#[test]
fn a_script_rebuilds_a_store_that_answers_what_the_source_answers() {
    let (source, script) = original();
    let target = store();
    Session::new(&target)
        .run(&script)
        .unwrap_or_else(|refusal| panic!("the script did not restore: {refusal}\n{script}"));
    let mut here = signed_in(&source, "root");
    let mut there = signed_in(&target, "root");
    for read in INTERROGATION {
        assert_eq!(
            format!("{:?}", here.run(read).unwrap()),
            format!("{:?}", there.run(read).unwrap()),
            "the two stores disagree about {read}"
        );
    }
    // An expiry is carried as the time it had left, and a key without one stays
    // without one.
    let mut ttl = |key: &str| {
        format!(
            "{:?}",
            there.run(&format!("RETURN TTL cache:'{key}';")).unwrap()
        )
    };
    let soon = ttl("soon");
    let forever = ttl("forever");
    assert!(soon.contains("Duration"), "the expiry was lost: {soon}");
    assert!(
        forever.contains("Null"),
        "an expiry was invented: {forever}"
    );
    // The viewer is back with her grant: two fields of one table, nothing else.
    let mut bea = signed_in(&target, "bea");
    let seen = format!("{:?}", bea.run("SELECT * FROM people:1;").unwrap());
    assert!(
        seen.contains("ada") && !seen.contains("london"),
        "bea's grant came back wrong: {seen}"
    );
    assert!(
        bea.run("SELECT * FROM kinds;").is_err(),
        "bea reads a table she was never granted"
    );
}

#[test]
fn what_a_script_does_not_carry_is_named_in_its_header() {
    let (_, script) = original();
    let header: Vec<&str> = script
        .lines()
        .take_while(|line| line.starts_with("--"))
        .collect();
    for named in [
        "prod.orders.jobs: holds",
        "prod.orders.events: messages",
        "prod.orders.cache: an expiry",
        "prod.orders.media: a file",
    ] {
        assert!(
            header.iter().any(|line| line.contains(named)),
            "the header does not name `{named}`:\n{}",
            header.join("\n")
        );
    }
}

#[test]
fn a_part_with_no_faithful_spelling_is_refused_by_name_and_not_written() {
    let source = store();
    Session::new(&source)
        .run(
            "DEFINE NAMESPACE n; USE NAMESPACE n; DEFINE DATABASE d; USE DATABASE d; \
             DEFINE TABLE a SCHEMALESS; DEFINE TABLE b SCHEMALESS; \
             DEFINE TABLE likes EDGE FROM a TO b; CREATE a:1 = {}; CREATE b:1 = {}; \
             RELATE a:1->likes->b:1;",
        )
        .unwrap();
    let taken = tessari_session::write_script(&source).unwrap();
    assert!(
        taken
            .refused
            .iter()
            .any(|part| part.starts_with("n.d.likes")),
        "an edge table with declared endpoints was not named: {:?}",
        taken.refused
    );
    assert!(
        !taken.text.contains("CREATE likes:"),
        "records of a table that was not declared were written"
    );
    // And the rest restores.
    Session::new(&store()).run(&taken.text).unwrap();
}
