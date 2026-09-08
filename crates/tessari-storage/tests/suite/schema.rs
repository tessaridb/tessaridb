//! What a table declares, the store enforces.
//!
//! The enforcement is on the apply path rather than in a layer above it, so
//! these tests write through the store directly — the same path a replica takes.
//! That is the point: a check the session owns is a check any other writer
//! bypasses.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_encoding::encode_payload;
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_storage::{Catalog, Error, FieldShape, RecordAddress, Store, TableShape};
use tessari_types::{
    Assertion, BinaryOp, DatabaseId, FieldKind, NamespaceId, Number, Operand, Path, RecordId,
    Sequence, TableId, Value,
};

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
        Catalog::new(&mut transaction).create_field(
            self.table,
            name,
            kind,
            FieldShape::default(),
        )?;
        transaction.commit()
    }

    fn require(&self, name: &str, kind: FieldKind) -> Result<Sequence, Error> {
        let mut transaction = self.store.begin().unwrap();
        Catalog::new(&mut transaction).create_field(
            self.table,
            name,
            kind,
            FieldShape {
                required: true,
                secret: false,
                default: None,
                analyzer: None,
                assert: None,
            },
        )?;
        transaction.commit()
    }

    fn constrain(&self, name: &str, kind: FieldKind, assert: Assertion) -> Result<Sequence, Error> {
        let mut transaction = self.store.begin().unwrap();
        Catalog::new(&mut transaction).create_field(
            self.table,
            name,
            kind,
            FieldShape {
                assert: Some(assert),
                ..FieldShape::default()
            },
        )?;
        transaction.commit()
    }

    fn declared(&self, name: &str) -> Option<tessari_storage::FieldDefinition> {
        let mut transaction = self.store.begin().unwrap();
        Catalog::new(&mut transaction)
            .fields_on(self.table)
            .unwrap()
            .into_iter()
            .find(|field| field.name == name)
    }

    fn remove(&self, id: &str) {
        let mut transaction = self.store.begin().unwrap();
        transaction.delete(self.at(id));
        transaction.commit().unwrap();
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
                &[("count", Value::Number(tessari_types::Number::float(3.0)))]
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
        .create_field(
            fixture.table,
            "email",
            FieldKind::String,
            FieldShape::default(),
        )
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
        .create_field(
            fixture.table,
            "email",
            FieldKind::String,
            FieldShape::default(),
        )
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
        .create_field(
            fixture.table,
            "email",
            FieldKind::String,
            FieldShape::default(),
        )
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
        .create_field(
            fixture.table,
            "email",
            FieldKind::String,
            FieldShape::default(),
        )
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

#[test]
fn a_required_field_must_hold_a_value_and_null_is_not_one() {
    let fixture = Fixture::new(false);
    fixture
        .write("u1", &[("name", Value::from("ada"))])
        .unwrap();
    fixture
        .write("u2", &[("email", Value::from("a@b"))])
        .unwrap();

    // Declared over rows that already violate it: refused, writing nothing.
    assert!(fixture.require("email", FieldKind::String).is_err());
    assert!(fixture.declared("email").is_none());

    // Once the offending row is gone, the declaration lands and then binds.
    fixture.remove("u1");
    fixture.require("email", FieldKind::String).unwrap();
    assert!(
        fixture
            .write("u3", &[("name", Value::from("grace"))])
            .is_err()
    );
    assert!(fixture.write("u4", &[("email", Value::Null)]).is_err());
    fixture
        .write("u5", &[("email", Value::from("c@d"))])
        .unwrap();
}

#[test]
fn a_replica_reaches_the_same_verdict_about_a_required_field() {
    // The requirement is enforced on the store's apply path, so a replica
    // computes it from the log rather than being told.
    let fixture = Fixture::new(false);
    fixture.require("email", FieldKind::String).unwrap();
    fixture
        .write("u1", &[("email", Value::from("a@b"))])
        .unwrap();
    assert!(
        fixture
            .write("u2", &[("name", Value::from("ada"))])
            .is_err()
    );

    let replica_backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let replica = Store::open(Arc::clone(&replica_backend)).unwrap();
    for (sequence, record) in fixture.store.log_records(Sequence::ZERO, 1024).unwrap() {
        replica.apply_record(sequence, &record).unwrap();
    }
    // Everything the leader committed applies; nothing it refused is in the log.
    assert_eq!(
        replica.log_records(Sequence::ZERO, 1024).unwrap().len(),
        fixture
            .store
            .log_records(Sequence::ZERO, 1024)
            .unwrap()
            .len()
    );
}

/// `$value >= 0`, the constraint every assertion test below uses.
fn not_negative() -> Assertion {
    Assertion::Compare {
        op: BinaryOp::GreaterOrEqual,
        against: Operand::Literal(Value::Number(Number::Integer(0))),
    }
}

/// `$value > starts_at`, the constraint that reads a second field of the record.
fn after(other: &str) -> Assertion {
    Assertion::Compare {
        op: BinaryOp::Greater,
        against: Operand::Field(Path::parse(other).unwrap()),
    }
}

#[test]
fn a_value_the_declaration_refuses_never_lands() {
    let fixture = Fixture::new(false);
    fixture
        .constrain("balance", FieldKind::Int, not_negative())
        .unwrap();
    fixture
        .write("a", &[("balance", Value::Number(Number::Integer(1)))])
        .unwrap();
    let refused = fixture.write("b", &[("balance", Value::Number(Number::Integer(-1)))]);
    assert!(
        matches!(refused, Err(Error::AssertionViolation { .. })),
        "{refused:?}"
    );
}

#[test]
fn a_declaration_may_compare_one_field_of_a_record_with_another() {
    // The verdict is still a pure function of the record and the catalog: the
    // second value comes out of the record being written, so no read of any
    // other record happens and a replica reaches the same answer.
    let fixture = Fixture::new(false);
    fixture
        .constrain("ends_at", FieldKind::Int, after("starts_at"))
        .unwrap();
    fixture
        .write(
            "ok",
            &[
                ("starts_at", Value::Number(Number::Integer(10))),
                ("ends_at", Value::Number(Number::Integer(20))),
            ],
        )
        .unwrap();

    let refused = fixture.write(
        "backwards",
        &[
            ("starts_at", Value::Number(Number::Integer(30))),
            ("ends_at", Value::Number(Number::Integer(20))),
        ],
    );
    let Err(error @ Error::AssertionViolation { .. }) = refused else {
        panic!("{refused:?}");
    };
    // The message names both fields, because "ends_at is refused" alone leaves
    // the writer guessing which constraint they broke — and both values came
    // from the statement they just sent, so neither is a disclosure.
    let said = error.to_string();
    assert!(said.contains("ends_at"), "{said}");
    assert!(said.contains("starts_at"), "{said}");
    assert!(!fixture.holds("backwards"), "the refused record landed");
}

#[test]
fn the_compared_field_is_read_from_the_record_and_need_not_be_declared() {
    // A lenient table keeps accepting what it accepted: `starts_at` is never
    // declared here, and the constraint still reads it, because the route
    // resolves through the record rather than through the catalog.
    let fixture = Fixture::new(false);
    fixture
        .constrain("ends_at", FieldKind::Int, after("starts_at"))
        .unwrap();
    assert!(!fixture.declares("starts_at"));

    fixture
        .write(
            "ok",
            &[
                ("starts_at", Value::Number(Number::Integer(1))),
                ("ends_at", Value::Number(Number::Integer(2))),
                ("note", Value::from("undeclared, and still accepted")),
            ],
        )
        .unwrap();
    // No subject, no assertion — the rule an assertion has always followed.
    fixture
        .write("empty", &[("note", Value::from("x"))])
        .unwrap();
    assert!(fixture.holds("ok") && fixture.holds("empty"));
}

#[test]
fn an_assertion_constrains_a_present_non_null_value_and_nothing_else() {
    // The same rule a kind follows. `REQUIRED` is the one constraint about
    // absence, and an assertion that also implied presence would make `REQUIRED`
    // mean two things depending on what stood beside it.
    let fixture = Fixture::new(false);
    fixture
        .constrain("balance", FieldKind::Int, not_negative())
        .unwrap();
    fixture.write("a", &[("other", Value::from("x"))]).unwrap();
    fixture.write("b", &[("balance", Value::Null)]).unwrap();
}

#[test]
fn declaring_one_over_rows_that_break_it_is_refused_and_writes_nothing() {
    let fixture = Fixture::new(false);
    fixture
        .write("a", &[("balance", Value::Number(Number::Integer(-5)))])
        .unwrap();
    let refused = fixture.constrain("balance", FieldKind::Int, not_negative());
    assert!(
        matches!(refused, Err(Error::AssertionViolation { .. })),
        "{refused:?}"
    );
    assert!(
        fixture.declared("balance").is_none(),
        "a refused declaration left itself behind"
    );
}

#[test]
fn a_cross_field_declaration_binds_the_rows_that_predate_it_too() {
    // Nothing new: the declare-time walk calls the same `check` per row, so a
    // constraint that reads a second field is checked against the rows already
    // there for the same reason a constraint over one value is. The walk stays
    // inside this table — it is N point reads bounded by the table itself, not
    // a read of any other table.
    let fixture = Fixture::new(false);
    fixture
        .write(
            "ok",
            &[
                ("starts_at", Value::Number(Number::Integer(1))),
                ("ends_at", Value::Number(Number::Integer(2))),
            ],
        )
        .unwrap();
    fixture
        .write(
            "backwards",
            &[
                ("starts_at", Value::Number(Number::Integer(9))),
                ("ends_at", Value::Number(Number::Integer(2))),
            ],
        )
        .unwrap();

    let refused = fixture.constrain("ends_at", FieldKind::Int, after("starts_at"));
    let Err(error @ Error::AssertionViolation { .. }) = refused else {
        panic!("{refused:?}");
    };
    // The offending record is named, and no other record is — the refusal says
    // which row to look at without reporting anything about the rest of them.
    let said = error.to_string();
    assert!(said.contains("backwards"), "{said}");
    assert!(!said.contains("\"ok\""), "{said}");
    assert!(
        fixture.declared("ends_at").is_none(),
        "a refused declaration left itself behind"
    );
    // And the rows are untouched: a refused declaration writes nothing at all.
    assert!(fixture.holds("ok") && fixture.holds("backwards"));
}

#[test]
fn a_replica_reaches_the_same_verdict_because_the_constraint_is_in_the_log() {
    // The reason the check lives here rather than in the session: the catalog is
    // itself in the log, so a replica that has applied the log holds the
    // constraint and refuses the same write — with nothing sent to tell it to.
    let fixture = Fixture::new(false);
    fixture
        .constrain("balance", FieldKind::Int, not_negative())
        .unwrap();
    fixture
        .write("a", &[("balance", Value::Number(Number::Integer(7)))])
        .unwrap();

    let replica_backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let replica = Store::open(Arc::clone(&replica_backend)).unwrap();
    for (sequence, record) in fixture.store.log_records(Sequence::ZERO, 1_000).unwrap() {
        replica.apply_record(sequence, &record).unwrap();
    }

    let mut transaction = replica.begin().unwrap();
    transaction.put(
        fixture.at("b"),
        encode_payload(&record(&[("balance", Value::Number(Number::Integer(-1)))])).into_bytes(),
    );
    let refused = transaction.commit();
    assert!(
        matches!(refused, Err(Error::AssertionViolation { .. })),
        "the replica did not learn the constraint: {refused:?}"
    );
}

/// A check over a table that satisfies its own declarations answers nothing.
///
/// The negative half, and it is the half that matters most: a check that found
/// something on a clean table would be worse than no check at all, because an
/// operator would learn to ignore it.
#[test]
fn a_check_answers_nothing_while_every_record_satisfies_the_declarations() {
    let fixture = Fixture::new(false);
    fixture.require("name", FieldKind::String).unwrap();
    fixture
        .write("1", &[("name", Value::String("ada".to_owned()))])
        .unwrap();
    fixture
        .write("2", &[("name", Value::String("grace".to_owned()))])
        .unwrap();

    let mut transaction = fixture.store.begin().unwrap();
    let found = tessari_storage::violations(
        &mut transaction,
        fixture.namespace,
        fixture.database,
        fixture.table,
    )
    .unwrap();
    assert!(found.is_empty(), "{found:?}");
}

/// A table that declares nothing answers nothing, without reading its rows.
#[test]
fn a_check_over_a_table_that_constrains_nothing_answers_nothing() {
    let fixture = Fixture::new(false);
    fixture
        .write("1", &[("anything", Value::Bool(true))])
        .unwrap();

    let mut transaction = fixture.store.begin().unwrap();
    let found = tessari_storage::violations(
        &mut transaction,
        fixture.namespace,
        fixture.database,
        fixture.table,
    )
    .unwrap();
    assert!(found.is_empty(), "{found:?}");
}

/// Every offending record is named, with the rule it broke — never only the first.
///
/// The three classes are asserted together because they are reached by three
/// different arms and a check that walked only one of them would look correct on
/// any table that violates a single rule. The declarations are held in an
/// **uncommitted** transaction: committing them is precisely what the apply path
/// refuses, and refusing is how the store stays consistent — so the state this
/// check exists to describe is one the language cannot commit its way into, and
/// an honest test has to build it the way a restore or a hand-edited backend
/// would.
#[test]
fn a_check_names_every_offending_record_and_the_rule_it_broke() {
    let fixture = Fixture::new(false);
    fixture
        .write(
            "1",
            &[
                ("name", Value::String("ada".to_owned())),
                ("n", Value::Number(Number::Integer(3))),
            ],
        )
        .unwrap();
    fixture
        .write("2", &[("n", Value::Number(Number::Integer(3)))])
        .unwrap();
    fixture
        .write(
            "3",
            &[
                ("name", Value::String("bo".to_owned())),
                ("n", Value::Number(Number::Integer(3))),
                ("extra", Value::Bool(true)),
            ],
        )
        .unwrap();
    fixture
        .write(
            "4",
            &[
                ("name", Value::String("cy".to_owned())),
                ("n", Value::Number(Number::Integer(-5))),
            ],
        )
        .unwrap();

    let mut transaction = fixture.store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    catalog
        .create_field(
            fixture.table,
            "name",
            FieldKind::String,
            FieldShape {
                required: true,
                ..FieldShape::default()
            },
        )
        .unwrap();
    catalog
        .create_field(
            fixture.table,
            "n",
            FieldKind::Int,
            FieldShape {
                assert: Some(not_negative()),
                ..FieldShape::default()
            },
        )
        .unwrap();
    catalog.set_schemafull(fixture.table, true).unwrap();

    let found = tessari_storage::violations(
        &mut transaction,
        fixture.namespace,
        fixture.database,
        fixture.table,
    )
    .unwrap();

    let named: Vec<(&str, &str)> = found
        .iter()
        .map(|violation| (violation.record.as_str(), violation.rule))
        .collect();
    assert_eq!(
        named,
        vec![("2", "required"), ("3", "undeclared"), ("4", "assert")],
        "{found:?}"
    );
    // The words are the store's own, so a check run before a tightening and the
    // tightening's own refusal cannot describe one record two ways.
    assert!(
        found[0].detail.contains("required field name"),
        "{:?}",
        found[0].detail
    );
    assert!(found[1].field == "extra", "{:?}", found[1]);
}
