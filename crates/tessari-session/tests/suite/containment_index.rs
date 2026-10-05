//! A containment index changes what `CONTAINS` costs and never what it answers
//! (ADR-0116 D4).
//!
//! The comparison is against the **scan's own answer** over the same documents in
//! a table with no index, for the reason `index_kinds_do_not_leak.rs` gives: the
//! scan is the definition. The documents and the questions are generated — nested
//! documents, arrays of values and of documents, every leaf kind the rule treats
//! differently — because the cases a person writes down are the ones the rule was
//! designed around, and the index is wrong, if it is, somewhere else.
//!
//! After the reads agree, the entries are swept against the records both ways: an
//! orphan entry costs every later read a candidate, and a missing one is a row the
//! index never offers — the one failure the re-test above it cannot catch.

#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use tessari_encoding::{ContainmentKey, IndexValues, KeyKind, StoreKey};
use tessari_kv::{KeyRange, KvBackend, MemoryBackend, ScanDirection, ScanRequest};
use tessari_ql::literal;
use tessari_session::{AccessPath, Outcome, Session};
use tessari_storage::Store;
use tessari_types::{Number, RecordId, Value};

/// A small deterministic generator: the fixture is the same on every run, so a
/// failure names a case that can be run again.
struct Draw(u64);

impl Draw {
    fn next(&mut self) -> u64 {
        // Knuth's MMIX constants.
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next().checked_rem(bound).unwrap_or(0)
    }

    fn leaf(&mut self) -> Value {
        match self.below(7) {
            0 => Value::Number(Number::Integer(i64::try_from(self.below(4)).unwrap())),
            // A float equal to an integer: one value to `=`, and to the index.
            1 => Value::Number(Number::Float(1.0)),
            2 => Value::from(["x", "y", "z"][usize::try_from(self.below(3)).unwrap()]),
            3 => Value::Bool(self.below(2) == 0),
            4 => Value::Null,
            5 => Value::from("1"),
            _ => Value::Number(Number::Integer(2)),
        }
    }

    fn document(&mut self, depth: u32) -> Value {
        let mut fields = BTreeMap::new();
        for _ in 0..self.below(4).saturating_add(1) {
            let name = ["a", "b", "c", "tags", "items"][usize::try_from(self.below(5)).unwrap()];
            fields.insert(name.to_owned(), self.member(depth));
        }
        Value::Object(fields)
    }

    fn member(&mut self, depth: u32) -> Value {
        match (self.below(5), depth) {
            (0, depth) if depth < 3 => self.document(depth.saturating_add(1)),
            (1, depth) if depth < 3 => {
                let mut items = Vec::new();
                for _ in 0..self.below(4) {
                    items.push(if self.below(2) == 0 {
                        self.leaf()
                    } else {
                        self.document(depth.saturating_add(1))
                    });
                }
                Value::Array(items)
            }
            _ => self.leaf(),
        }
    }

    /// A question a held document answers yes to: some of its fields, some of
    /// its array elements, recursively.
    fn part_of(&mut self, held: &Value) -> Value {
        match held {
            Value::Object(fields) => {
                let mut kept = BTreeMap::new();
                for (name, value) in fields {
                    if self.below(3) != 0 {
                        kept.insert(name.clone(), self.part_of(value));
                    }
                }
                Value::Object(kept)
            }
            Value::Array(items) => {
                let mut kept = Vec::new();
                for item in items {
                    if self.below(2) == 0 {
                        kept.push(self.part_of(item));
                    }
                }
                Value::Array(kept)
            }
            leaf => leaf.clone(),
        }
    }

    /// A field's value for one record: mostly a document, sometimes an array of
    /// them (membership), a single value, or nothing at all.
    fn field(&mut self) -> Option<Value> {
        match self.below(10) {
            0 => Some(Value::Array(vec![self.document(1), self.document(1)])),
            1 => Some(self.leaf()),
            2 => None,
            _ => Some(self.document(0)),
        }
    }
}

const RECORDS: u64 = 160;
const QUESTIONS: usize = 160;

fn memory() -> (Arc<dyn KvBackend>, Store) {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(Arc::clone(&backend)).unwrap();
    (backend, store)
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE n; USE NAMESPACE n; DEFINE DATABASE d; USE DATABASE d;\n\
             DEFINE COLLECTION plain; DEFINE COLLECTION indexed;\n\
             DEFINE INDEX by_doc ON indexed FIELDS doc CONTAINS;",
        )
        .unwrap();
    session
}

fn written(value: &Value) -> String {
    literal::value(value, &literal::Names::new())
}

fn put(session: &mut Session<'_>, id: u64, field: Option<&Value>) {
    let body = field.map_or_else(
        || "{ other: 1 }".to_owned(),
        |held| format!("{{ doc: {} }}", written(held)),
    );
    session
        .run(&format!(
            "UPSERT plain:{id} = {body}; UPSERT indexed:{id} = {body};"
        ))
        .unwrap();
}

/// The ids one read answered with, and the path it took.
fn answered(
    session: &mut Session<'_>,
    table: &str,
    asked: &Value,
) -> (BTreeSet<RecordId>, AccessPath) {
    let statement = format!(
        "SELECT * FROM {table} WHERE doc CONTAINS {};",
        written(asked)
    );
    let outcome = session.run(&statement).unwrap().pop().unwrap();
    let path = outcome.path().unwrap();
    let Outcome::Records { records, .. } = outcome else {
        panic!("{statement}");
    };
    (records.into_iter().map(|(id, _)| id).collect(), path)
}

/// Every question asked of both tables; the two answers must agree.
fn agree(session: &mut Session<'_>, questions: &[Value]) -> (usize, usize) {
    let (mut answered_some, mut answered_none) = (0_usize, 0_usize);
    for asked in questions {
        let (scanned, path) = answered(session, "plain", asked);
        assert_eq!(path, AccessPath::Scan);
        let (served, path) = answered(session, "indexed", asked);
        let narrows = !tessari_types::containment::asked_pairs(asked).is_empty();
        assert_eq!(
            path,
            if narrows {
                AccessPath::Index
            } else {
                AccessPath::Scan
            },
            "{}",
            written(asked)
        );
        assert_eq!(served, scanned, "CONTAINS {}", written(asked));
        if scanned.is_empty() {
            answered_none = answered_none.saturating_add(1);
        } else {
            answered_some = answered_some.saturating_add(1);
        }
    }
    (answered_some, answered_none)
}

/// The entries the store holds, against the pairs its records hold.
fn swept(backend: &Arc<dyn KvBackend>, session: &mut Session<'_>) {
    let request = ScanRequest {
        keyspace: KeyKind::Containment.keyspace(),
        range: KeyRange::prefix(&[KeyKind::Containment.tag()]),
        direction: ScanDirection::Forward,
        limit: None,
    };
    let held: BTreeSet<(Vec<u8>, RecordId)> = backend
        .scan(&request)
        .unwrap()
        .into_iter()
        .map(|(key, _)| {
            let entry = ContainmentKey::decode(key.as_slice()).unwrap();
            (entry.values.as_slice().to_vec(), entry.id)
        })
        .collect();
    let Outcome::Records { records, .. } = session
        .run("SELECT * FROM indexed;")
        .unwrap()
        .pop()
        .unwrap()
    else {
        panic!();
    };
    let mut expected = BTreeSet::new();
    for (id, record) in records {
        let Value::Object(fields) = record else {
            panic!()
        };
        if let Some(field) = fields.get("doc") {
            for pair in tessari_types::containment::held_pairs(field) {
                expected.insert((IndexValues::of(&pair).as_slice().to_vec(), id.clone()));
            }
        }
    }
    assert_eq!(held.len(), expected.len(), "entry count");
    assert!(held == expected, "an orphan or a missing entry");
}

#[test]
fn a_containment_index_answers_what_the_scan_answers_over_generated_documents() {
    let (backend, store) = memory();
    let mut session = ready(&store);
    let mut draw = Draw(0x0116);
    let mut fields = BTreeMap::new();
    for id in 0..RECORDS {
        let field = draw.field();
        put(&mut session, id, field.as_ref());
        fields.insert(id, field);
    }
    let documents: Vec<&Value> = fields.values().flatten().collect();
    let mut questions = Vec::new();
    for at in 0..QUESTIONS {
        let held = documents[at % documents.len()];
        questions.push(match draw.below(4) {
            // Something a record holds, and something nobody may.
            0 => draw.document(0),
            1 => match held {
                Value::Array(items) => items.first().cloned().unwrap_or(Value::Null),
                other => draw.part_of(other),
            },
            _ => draw.part_of(held),
        });
    }
    let (some, none) = agree(&mut session, &questions);
    // A fixture where every answer is empty, or every one is everything, would
    // agree trivially.
    assert!(some > 40 && none > 10, "{some} answered, {none} empty");
    swept(&backend, &mut session);

    // Rewrite a third of them, remove some, and ask again.
    for id in 0..RECORDS {
        match draw.below(6) {
            0 | 1 => put(&mut session, id, draw.field().as_ref()),
            2 => {
                session
                    .run(&format!("DELETE plain:{id}; DELETE indexed:{id};"))
                    .unwrap();
            }
            _ => {}
        }
    }
    agree(&mut session, &questions);
    swept(&backend, &mut session);
}

#[test]
fn a_record_this_transaction_wrote_is_offered_before_it_commits() {
    let (_, store) = memory();
    let mut session = ready(&store);
    session
        .run("CREATE indexed:1 = { doc: { city: 'Paris' } };")
        .unwrap();
    let asked = "SELECT * FROM indexed WHERE doc CONTAINS { city: 'Paris' };";
    let mut outcomes = session
        .run(&format!(
            "BEGIN; CREATE indexed:2 = {{ doc: {{ city: 'Paris' }} }}; DELETE indexed:1; {asked} CANCEL;"
        ))
        .unwrap();
    let outcome = outcomes.remove(3);
    assert_eq!(outcome.path(), Some(AccessPath::Index));
    let ids: Vec<RecordId> = outcome
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    assert_eq!(ids, vec![RecordId::Int(2)]);
    let ids: Vec<RecordId> = session.run(asked).unwrap()[0]
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    assert_eq!(ids, vec![RecordId::Int(1)]);
}

#[test]
fn the_index_reads_back_after_the_store_is_closed_and_opened() {
    let directory = tempfile::tempdir().unwrap();
    let open = || {
        let backend = tessari_lsm::LsmBackend::open(
            directory.path(),
            tessari_lsm::StoreConfig::new(tessari_lsm::Durability::ProcessCrashSafe),
        )
        .unwrap();
        Store::open(Arc::new(backend) as Arc<dyn KvBackend>).unwrap()
    };
    let asked = "SELECT * FROM indexed WHERE doc CONTAINS { items: [{ sku: 'b' }] };";
    {
        let disk = open();
        let mut session = ready(&disk);
        session
            .run(
                "CREATE indexed:1 = { doc: { items: [{ sku: 'a' }, { sku: 'b' }] } };\n\
                 CREATE indexed:2 = { doc: { items: [{ sku: 'c' }] } };",
            )
            .unwrap();
        assert_eq!(session.run(asked).unwrap()[0].records().unwrap().len(), 1);
    }
    let disk = open();
    let mut session = Session::new(&disk);
    session.run("USE NAMESPACE n; USE DATABASE d;").unwrap();
    let outcome = session.run(asked).unwrap().pop().unwrap();
    assert_eq!(outcome.path(), Some(AccessPath::Index));
    assert_eq!(outcome.records().unwrap().len(), 1);
}

#[test]
fn a_store_moves_to_format_six_only_when_its_first_containment_index_is_built() {
    use tessari_encoding::{FormatVersion, FormatVersionKey, StoreValue};
    use tessari_kv::WriteBatch;

    let stamp = |backend: &Arc<dyn KvBackend>| {
        backend
            .get(FormatVersionKey::keyspace(), &FormatVersionKey.encode())
            .unwrap()
            .map(|held| FormatVersion::decode(held.as_slice()).unwrap().get())
    };
    let (backend, store) = memory();
    drop(store);
    // A store an older build wrote: format 5, which this build opens as is.
    backend
        .apply(WriteBatch::new().put(
            FormatVersionKey::keyspace(),
            FormatVersionKey.encode(),
            FormatVersion::new(5).encode(),
        ))
        .unwrap();
    let store = Store::open(Arc::clone(&backend)).unwrap();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE n; USE NAMESPACE n; DEFINE DATABASE d; USE DATABASE d;\n\
             DEFINE COLLECTION notes; CREATE notes:1 = { doc: { a: 1 }, n: 1 };\n\
             DEFINE INDEX by_n ON notes FIELDS n;",
        )
        .unwrap();
    assert_eq!(
        stamp(&backend),
        Some(5),
        "an ordinary index moved the format"
    );
    session
        .run("DEFINE INDEX by_doc ON notes FIELDS doc CONTAINS;")
        .unwrap();
    assert_eq!(
        stamp(&backend),
        Some(6),
        "a containment index left the format at 5"
    );
}
