//! What a table declares, the store enforces.
//!
//! The enforcement is on the apply path rather than in a layer above it, so
//! these tests write through the store directly — the same path a replica takes.
//! That is the point: a check the session owns is a check any other writer
//! bypasses.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use bgv_db_encoding::encode_payload;
use bgv_db_kv::{KvBackend, MemoryBackend};
use bgv_db_storage::{Catalog, Error, RecordAddress, Store, TableShape};
use bgv_db_types::{DatabaseId, FieldKind, NamespaceId, RecordId, Sequence, TableId, Value};

struct Fixture {
    store: Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
}

impl Fixture {
    /// A table, schemaless or not, with nothing declared on it yet.
    fn new(schemafull: bool) -> Self {
        let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
        let store = Store::open(backend).unwrap();

        let mut transaction = store.begin().unwrap();
        let mut catalog = Catalog::new(&mut transaction);
        let namespace = catalog.create_namespace("prod").unwrap();
        let database = catalog.create_database(namespace.id, "orders").unwrap();
        let table = catalog
            .create_table(
                namespace.id,
                database.id,
                "users",
                TableShape {
                    schemafull,
                    ..TableShape::default()
                },
            )
            .unwrap();
        transaction.commit().unwrap();

        Self {
            store,
            namespace: namespace.id,
            database: database.id,
            table: table.id,
        }
    }

    fn at(&self, id: &str) -> RecordAddress {
        RecordAddress::new(
            self.namespace,
            self.database,
            self.table,
            RecordId::from(id),
        )
    }

    fn declare(&self, name: &str, kind: FieldKind) -> Result<Sequence, Error> {
        let mut transaction = self.store.begin().unwrap();
        Catalog::new(&mut transaction).create_field(self.table, name, kind)?;
        transaction.commit()
    }

    fn write(&self, id: &str, fields: &[(&str, Value)]) -> Result<Sequence, Error> {
        let mut transaction = self.store.begin().unwrap();
        transaction.put(self.at(id), encode_payload(&record(fields)).into_bytes());
        transaction.commit()
    }

    fn holds(&self, id: &str) -> bool {
        self.store
            .begin()
            .unwrap()
            .get(&self.at(id))
            .unwrap()
            .is_some()
    }

    fn declares(&self, name: &str) -> bool {
        let mut transaction = self.store.begin().unwrap();
        Catalog::new(&mut transaction)
            .fields_on(self.table)
            .unwrap()
            .iter()
            .any(|declared| declared.name == name)
    }
}

fn record(fields: &[(&str, Value)]) -> Value {
    Value::Object(
        fields
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.clone()))
            .collect::<BTreeMap<String, Value>>(),
    )
}

#[test]
fn a_table_that_declares_nothing_accepts_anything() {
    let fixture = Fixture::new(false);
    fixture
        .write("u1", &[("email", Value::from("ada@example.com"))])
        .unwrap();
    fixture
        .write("u2", &[("email", Value::Bool(true))])
        .unwrap();
    assert!(fixture.holds("u1") && fixture.holds("u2"));
}

#[test]
fn a_declared_field_holding_the_wrong_type_is_refused() {
    let fixture = Fixture::new(false);
    fixture.declare("email", FieldKind::String).unwrap();

    let error = fixture
        .write("u1", &[("email", Value::Bool(true))])
        .unwrap_err();
    assert_eq!(error.code(), "validation");
    let text = error.to_string();
    // All three of the field, what was declared and what was found: a message
    // carrying only the first is one the reader has to go and look two things up
    // to act on.
    assert!(text.contains("email"), "{text}");
    assert!(text.contains("string"), "{text}");
    assert!(text.contains("bool"), "{text}");
    assert!(!fixture.holds("u1"));
}

#[test]
fn an_undeclared_field_passes_on_a_schemaless_table_and_is_refused_on_a_schemafull_one() {
    let lenient = Fixture::new(false);
    lenient.declare("status", FieldKind::String).unwrap();
    lenient
        .write("t1", &[("stauts", Value::from("open"))])
        .unwrap();
    assert!(lenient.holds("t1"));

    let strict = Fixture::new(true);
    strict.declare("status", FieldKind::String).unwrap();
    let error = strict
        .write("t1", &[("stauts", Value::from("open"))])
        .unwrap_err();
    assert_eq!(error.code(), "validation");
    assert!(error.to_string().contains("stauts"), "{error}");
    assert!(!strict.holds("t1"));
}

#[test]
fn absent_and_null_satisfy_a_declaration_but_a_wrong_type_does_not() {
    let fixture = Fixture::new(false);
    fixture.declare("email", FieldKind::String).unwrap();

    // Absent: the field is not there, so there is nothing to check — the same
    // rule an index applies to a record missing an indexed field.
    fixture
        .write("u1", &[("name", Value::from("ada"))])
        .unwrap();
    // Null: present and holding nothing, which SQL lets a typed column hold.
    fixture.write("u2", &[("email", Value::Null)]).unwrap();
    assert!(fixture.holds("u1") && fixture.holds("u2"));

    assert!(
        fixture
            .write("u3", &[("email", Value::Number(7.into()))])
            .is_err()
    );
}

#[test]
fn the_three_numeric_forms_are_separate_declarations() {
    let fixture = Fixture::new(false);
    fixture.declare("count", FieldKind::Int).unwrap();
    fixture.declare("price", FieldKind::Decimal).unwrap();

    fixture
        .write("i1", &[("count", Value::Number(3.into()))])
        .unwrap();
    // A float is a number and is not an int, which is the distinction that keeps
    // money out of binary floating point.
    assert!(
        fixture
            .write(
                "i2",
                &[("count", Value::Number(bgv_db_types::Number::float(3.0)))]
            )
            .is_err()
    );
    assert!(
        fixture
            .write("p1", &[("price", Value::Number(3.into()))])
            .is_err()
    );
}

#[test]
fn declaring_a_field_over_rows_that_already_violate_it_is_refused_and_writes_nothing() {
    let fixture = Fixture::new(false);
    fixture
        .write("u1", &[("email", Value::Bool(true))])
        .unwrap();

    let error = fixture.declare("email", FieldKind::String).unwrap_err();
    assert_eq!(error.code(), "validation");
    // Not even the definition: a constraint that can be declared over data
    // violating it is a constraint the store does not have, and every reader
    // afterwards would believe it did.
    assert!(!fixture.declares("email"));
    assert!(fixture.holds("u1"));
}

#[test]
fn a_declaration_constrains_the_rows_that_predate_it() {
    let fixture = Fixture::new(false);
    fixture
        .write("u1", &[("email", Value::from("ada@example.com"))])
        .unwrap();
    fixture.declare("email", FieldKind::String).unwrap();

    assert!(fixture.declares("email"));
    assert!(
        fixture
            .write("u2", &[("email", Value::Bool(false))])
            .is_err()
    );
}

#[test]
fn a_row_written_in_the_transaction_that_declares_the_field_is_checked_against_it() {
    let fixture = Fixture::new(false);

    let mut transaction = fixture.store.begin().unwrap();
    Catalog::new(&mut transaction)
        .create_field(fixture.table, "email", FieldKind::String)
        .unwrap();
    transaction.put(
        fixture.at("u1"),
        encode_payload(&record(&[("email", Value::Bool(true))])).into_bytes(),
    );
    // The per-mutation path reads the catalog *below* this commit, where the
    // declaration does not exist yet. It is the second pass that catches this.
    let error = transaction.commit().unwrap_err();
    assert_eq!(error.code(), "validation");
    assert!(!fixture.holds("u1"));
    assert!(!fixture.declares("email"));
}

#[test]
fn a_row_and_its_declaration_may_arrive_in_either_order_within_one_transaction() {
    let fixture = Fixture::new(false);

    let mut transaction = fixture.store.begin().unwrap();
    transaction.put(
        fixture.at("u1"),
        encode_payload(&record(&[("email", Value::from("ada@example.com"))])).into_bytes(),
    );
    Catalog::new(&mut transaction)
        .create_field(fixture.table, "email", FieldKind::String)
        .unwrap();
    transaction.commit().unwrap();

    assert!(fixture.holds("u1") && fixture.declares("email"));
}

#[test]
fn a_schemafull_table_refuses_every_field_until_one_is_declared() {
    // `SCHEMAFULL` is fixed at creation, so there is no "make this populated
    // table strict" case to test: the table is empty when it becomes strict, and
    // rows written alongside the creation are caught by the ordinary path.
    let strict = Fixture::new(true);
    assert!(
        strict
            .write("u1", &[("email", Value::from("ada@example.com"))])
            .is_err()
    );

    strict.declare("email", FieldKind::String).unwrap();
    strict
        .write("u1", &[("email", Value::from("ada@example.com"))])
        .unwrap();
    assert!(strict.holds("u1"));
}

#[test]
fn a_schemafull_table_declared_and_written_in_one_transaction_checks_those_rows() {
    let fixture = Fixture::new(true);
    fixture.declare("email", FieldKind::String).unwrap();

    let mut transaction = fixture.store.begin().unwrap();
    transaction.put(
        fixture.at("u1"),
        encode_payload(&record(&[("stauts", Value::from("open"))])).into_bytes(),
    );
    let error = transaction.commit().unwrap_err();
    assert_eq!(error.code(), "validation");
    assert!(!fixture.holds("u1"));
}

#[test]
fn dropping_a_declaration_loosens_what_the_table_accepts() {
    let fixture = Fixture::new(false);
    let mut transaction = fixture.store.begin().unwrap();
    let declared = Catalog::new(&mut transaction)
        .create_field(fixture.table, "email", FieldKind::String)
        .unwrap();
    transaction.commit().unwrap();
    assert!(
        fixture
            .write("u1", &[("email", Value::Bool(true))])
            .is_err()
    );

    let mut transaction = fixture.store.begin().unwrap();
    Catalog::new(&mut transaction)
        .drop_field(declared.id)
        .unwrap();
    transaction.commit().unwrap();

    fixture
        .write("u1", &[("email", Value::Bool(true))])
        .unwrap();
    assert!(fixture.holds("u1"));
}

#[test]
fn a_declaration_dropped_in_the_same_transaction_no_longer_constrains_the_write() {
    let fixture = Fixture::new(false);
    let mut transaction = fixture.store.begin().unwrap();
    let declared = Catalog::new(&mut transaction)
        .create_field(fixture.table, "email", FieldKind::String)
        .unwrap();
    transaction.commit().unwrap();

    let mut transaction = fixture.store.begin().unwrap();
    Catalog::new(&mut transaction)
        .drop_field(declared.id)
        .unwrap();
    transaction.put(
        fixture.at("u1"),
        encode_payload(&record(&[("email", Value::Bool(true))])).into_bytes(),
    );
    transaction.commit().unwrap();
    assert!(fixture.holds("u1"));
}

#[test]
fn a_replica_reaches_the_same_verdict_from_the_same_log() {
    // Validation is a pure function of the log record and the catalog, and the
    // catalog is in the log — so a replica needs nothing sent to agree. The
    // define-time pass is where it could diverge, because it reads rows the
    // record does not carry.
    let source = Fixture::new(false);
    source
        .write("u1", &[("email", Value::from("ada@example.com"))])
        .unwrap();
    source.declare("email", FieldKind::String).unwrap();
    source
        .write("u2", &[("email", Value::from("grace@example.com"))])
        .unwrap();
    assert!(source.write("u3", &[("email", Value::Bool(true))]).is_err());

    let replica_backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let replica = Store::open(Arc::clone(&replica_backend)).unwrap();
    for (sequence, log_record) in source.store.log_records(Sequence::ZERO, 1024).unwrap() {
        replica.apply_record(sequence, &log_record).unwrap();
    }

    let mirrored = Fixture {
        store: replica,
        namespace: source.namespace,
        database: source.database,
        table: source.table,
    };
    assert!(mirrored.holds("u1") && mirrored.holds("u2") && !mirrored.holds("u3"));
    assert!(mirrored.declares("email"));
    // And the constraint is live on the replica too, not merely replayed.
    assert!(
        mirrored
            .write("u4", &[("email", Value::Bool(false))])
            .is_err()
    );
}

#[test]
fn a_user_table_sharing_an_id_with_a_system_table_is_still_its_own_table() {
    // Definitions are the schema, so catalog records are not schema-checked —
    // and what separates them from user records is the *tenancy*, not the table
    // id. User table ids start at one, which is also the id of the system table
    // holding namespace definitions, so a check comparing ids alone would take
    // every first-created table for a system one and stop enforcing on it.
    let fixture = Fixture::new(true);
    assert_eq!(fixture.table.get(), 1, "the premise of this test moved");
    fixture.declare("email", FieldKind::String).unwrap();
    assert!(fixture.declares("email"));
    assert!(
        fixture
            .write("u1", &[("stauts", Value::from("open"))])
            .is_err()
    );
}
