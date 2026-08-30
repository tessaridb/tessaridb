//! Refusing records that disagree with what their table declares.
//!
//! # Why the store and not the layer above it
//!
//! A schema the session checks is a schema every other writer bypasses, and the
//! store is the thing that must never hold data contradicting its own catalog.
//! So the check lives on the apply path, beside index maintenance, and for the
//! same three reasons that put index entries there:
//!
//! - it is a pure function of the log record and the catalog, so a replica
//!   reaches the same verdict as the leader without anything being sent;
//! - the catalog is itself in the log, so a declaration and the rows it
//!   constrains are decided against one another at one position;
//! - a refusal fails the whole commit, so a transaction never lands half
//!   constrained.
//!
//! # What a declaration constrains
//!
//! A [`FieldKind`] constrains a **present, non-null** value. `none` passes,
//! because the field is not there — the rule an index already applies to a
//! record missing an indexed field — and `null` passes, because that is SQL's
//! rule for a typed column. So `TYPE string` does not make a field mandatory;
//! requiring a value is a separate constraint this milestone does not have.
//!
//! A `SCHEMAFULL` table additionally refuses a field it has no declaration for.
//! That is what turns a set of declarations into a schema, because the mistake
//! worth catching writes a field nobody declared: `stauts` for `status` creates
//! a field, raises nothing, and quietly drops the record out of every query
//! filtering on the name that was meant.
//!
//! # Two passes, for the reason index maintenance needs two
//!
//! The first checks each record this log record writes. The second exists
//! because a declaration made *by this record* is invisible to a catalog read
//! taken below it: a `DEFINE FIELD`, or a `DEFINE TABLE … SCHEMAFULL`, must
//! constrain the rows already there — otherwise a constraint could be declared
//! over data that violates it, and every reader afterwards would believe it
//! held. So the second pass re-checks every row of any table whose schema this
//! record tightens: the committed rows overlaid with this record's own writes.
//!
//! Both passes check against the schema **as it will stand after the commit**,
//! which is what lets a row and the declaration constraining it arrive in either
//! order within one transaction.
//!
//! # What it costs
//!
//! Declaring a field reads the whole table inside the commit, exactly as
//! defining an index does, and fails with [`Error::CommitContention`] rather
//! than half-applying when the pass outlasts the gap between concurrent writes.
//!
//! Writing an ordinary record costs a snapshot plus, per table the record
//! touches, one point read of the table definition and one scan of the field
//! catalog — built once per commit and cached across the record's mutations.
//! **A table that declares nothing pays this too**, because finding out that it
//! declares nothing is the scan. That is the same cost [`crate::index`] already
//! pays through `indexes_on`, and it is honest at catalog scale rather than at
//! table scale; when it stops being honest the answer is a cache keyed by the
//! catalog's own version, shared by both, not a cleverer scan in each.

use std::collections::{BTreeMap, BTreeSet};

use tessari_encoding::{LogRecord, RecordValue, decode_payload};
use tessari_types::{Assertion, DatabaseId, FieldKind, NamespaceId, RecordId, TableId, Value};

use crate::catalog::{Catalog, CatalogChange, catalog_change};
use crate::error::{Error, Result};
use crate::store::Store;
use crate::transaction::Transaction;

/// Where a table lives, and what it declares, once this record is applied.
#[derive(Debug, Default)]
struct TableSchema {
    /// The table's name, as a declaration writes it.
    ///
    /// Kept from the definition the schema is built out of, because the one
    /// refusal a caller fixes with a declaration has to be able to name what the
    /// declaration would be `ON`. Empty for a table with no definition at all,
    /// which is a table that also refuses nothing.
    name: String,
    /// Declared fields, by name: what each may hold, and whether it must.
    fields: BTreeMap<String, Declared>,
    /// Whether an undeclared field is refused.
    schemafull: bool,
}

impl TableSchema {
    /// Whether this schema can refuse anything at all.
    ///
    /// A schemaless table with no declarations constrains nothing, and skipping
    /// it is what keeps the check off the path of every table that has no
    /// schema — which, until someone declares one, is every table.
    fn constrains_nothing(&self) -> bool {
        self.fields.is_empty() && !self.schemafull
    }
}

/// What one declaration constrains.
#[derive(Debug, Clone)]
struct Declared {
    /// What the field may hold when it holds anything.
    kind: FieldKind,
    /// Whether it must hold something: present, and not `null`.
    required: bool,
    /// What it must satisfy beyond its type, when it holds anything.
    ///
    /// Already lowered by the language, so checking it here is a comparison and
    /// not an evaluation — which is what lets validation stay a pure function of
    /// the record and the catalog.
    assert: Option<Assertion>,
}

/// A table addressed the way a scan needs it.
type TableAddress = (NamespaceId, DatabaseId, TableId);

/// Refuse the record if any of its writes disagrees with its table's schema.
///
/// # Errors
///
/// Returns [`Error::SchemaViolation`] when a declared field holds the wrong
/// type, [`Error::UndeclaredField`] when a `SCHEMAFULL` table is written a field
/// it does not declare, and a substrate or decoding failure otherwise.
///
/// When **more than one** record in the same commit disagrees, the refusals are
/// carried together in [`Error::RecordsRefused`]. Every record is checked before
/// any of them is raised, so a caller writing a batch is told about all of it at
/// once rather than one commit at a time.
pub(crate) fn validate(store: &Store, record: &LogRecord) -> Result<()> {
    let tightened = tightened_tables(record)?;
    let touched: BTreeSet<TableAddress> = record
        .mutations()
        .iter()
        .filter(|mutation| matches!(mutation.value, RecordValue::Present(_)))
        .map(|mutation| (mutation.namespace, mutation.database, mutation.table))
        .filter(|address| !is_system(address))
        .collect();
    if tightened.is_empty() && touched.is_empty() {
        return Ok(());
    }

    let mut view = store.begin()?;
    let mut schemas: BTreeMap<TableId, TableSchema> = BTreeMap::new();
    for address in touched.iter().chain(tightened.iter()) {
        let schema = build_schema(&mut view, record, address.2)?;
        schemas.insert(address.2, schema);
    }

    // Both passes report into this rather than raising, because a caller who
    // sent a batch is going to fix all of it and a refusal naming one row makes
    // them find the rest one commit at a time. The whole record is walked even
    // once something is known to be wrong, which costs a scan the commit was
    // going to abandon anyway.
    let mut refusals: Vec<Error> = Vec::new();

    // Rows the first pass has checked, for tables the second pass will walk.
    //
    // A row this record writes to a table it also **tightens** is seen by both
    // passes, against the same schema and the same value, so the second pass
    // would refuse it a second time. Nothing noticed while the first refusal
    // ended the walk; now it would tell a caller two records were refused when
    // one was — a count that is wrong in the direction of looking thorough.
    //
    // Only populated for tables that are being tightened, so an ordinary write
    // allocates nothing here.
    let mut checked: BTreeSet<(TableId, RecordId)> = BTreeSet::new();

    for mutation in record.mutations() {
        let address = (mutation.namespace, mutation.database, mutation.table);
        if is_system(&address) {
            continue;
        }
        let RecordValue::Present(payload) = &mutation.value else {
            continue;
        };
        let Some(schema) = schemas.get(&mutation.table) else {
            continue;
        };
        if schema.constrains_nothing() {
            continue;
        }
        refusals.extend(check(
            schema,
            &decode_payload(payload)?,
            mutation.table,
            &mutation.id,
        ));
        if tightened.iter().any(|address| address.2 == mutation.table) {
            checked.insert((mutation.table, mutation.id.clone()));
        }
    }

    for address in &tightened {
        let Some(schema) = schemas.get(&address.2) else {
            continue;
        };
        if schema.constrains_nothing() {
            continue;
        }
        for (id, payload) in rows_after(&view, record, address)? {
            if checked.contains(&(address.2, id.clone())) {
                continue;
            }
            refusals.extend(check(schema, &decode_payload(&payload)?, address.2, &id));
        }
    }

    // One refusal keeps the shape it has always had. A batch of one is not a
    // batch, and every corpus row and every test asserting the singular form is
    // asserting something that is still true.
    let mut found = refusals.into_iter();
    match (found.next(), found.next()) {
        (None, _) => Ok(()),
        (Some(only), None) => Err(only),
        (Some(first), Some(second)) => Err(Error::RecordsRefused {
            refusals: std::iter::once(first)
                .chain(std::iter::once(second))
                .chain(found)
                .collect(),
        }),
    }
}

/// The other fields a constraint compares against, as one phrase for a refusal.
///
/// `None` for a constraint that compares against literals alone, so the message
/// keeps the shape it has always had for the assertions that existed before a
/// declaration could name a second field.
fn compared_with(assertion: &Assertion) -> Option<String> {
    let named = assertion.compared_fields();
    (!named.is_empty()).then(|| named.join(", "))
}

/// One record's fields, against the table's declarations.
///
/// Reports the **first** disagreement this record has rather than raising it, so
/// that a caller who sent several bad records hears about all of them. One per
/// record and not one per field: the caller's unit of work is the row, and a row
/// with two mistakes in it is still one row to go back and fix.
fn check(schema: &TableSchema, value: &Value, table: TableId, id: &RecordId) -> Option<Error> {
    // A record that is not an object has no named fields to constrain. The
    // key-value model stores single values that way (ADR-0010), and a field
    // declaration on such a table describes something that is not there.
    let Value::Object(fields) = value else {
        return None;
    };
    for (name, held) in fields {
        match schema.fields.get(name.as_str()) {
            Some(declared) if !declared.kind.accepts(held) => {
                return Some(Error::SchemaViolation {
                    table: table.get(),
                    record: id.to_string(),
                    field: name.clone(),
                    declared: declared.kind.name().into_owned(),
                    found: held.type_name(),
                });
            }
            // An assertion constrains a **present, non-null** value, exactly as
            // a kind does. `REQUIRED` is the one constraint about absence, and an
            // assertion that also implied presence would make `REQUIRED` mean
            // two things depending on what stood beside it.
            Some(declared)
                if held.is_present()
                    && *held != Value::Null
                    && declared
                        .assert
                        .as_ref()
                        .is_some_and(|assertion| !assertion.holds(held, value)) =>
            {
                return Some(Error::AssertionViolation {
                    table: table.get(),
                    record: id.to_string(),
                    field: name.clone(),
                    compared_with: declared.assert.as_ref().and_then(compared_with),
                });
            }
            Some(_) => {}
            None if schema.schemafull => {
                return Some(Error::UndeclaredField {
                    table: schema.name.clone(),
                    record: id.to_string(),
                    field: name.clone(),
                    // The kind of the value the caller just sent, so a
                    // declaration built from it accepts this very write. `none`
                    // and `null` are the two type names that are not kinds —
                    // they are what a field holds when it holds nothing — and
                    // `any` is the kind that accepts them.
                    kind: Box::new(FieldKind::parse(held.type_name()).unwrap_or(FieldKind::Any)),
                });
            }
            None => {}
        }
    }

    // A requirement is checked over the **declarations**, not over what the
    // record holds — a field that is absent is absent from the loop above, so
    // the one constraint about absence is the one that cannot be expressed
    // there.
    for (name, declared) in &schema.fields {
        if !declared.required {
            continue;
        }
        let held = fields.get(name.as_str()).unwrap_or(&Value::None);
        if !held.is_present() || *held == Value::Null {
            return Some(Error::MissingRequiredField {
                table: table.get(),
                record: id.to_string(),
                field: name.clone(),
                found: held.type_name(),
            });
        }
    }
    None
}

/// The schema a table will have once this record is applied.
fn build_schema(
    view: &mut Transaction<'_>,
    record: &LogRecord,
    table: TableId,
) -> Result<TableSchema> {
    let defined = Catalog::new(view).table(table)?;
    let mut name = defined
        .as_ref()
        .map(|found| found.name.clone())
        .unwrap_or_default();
    let mut schemafull = defined.is_some_and(|found| found.schemafull);
    let mut fields: BTreeMap<String, Declared> = Catalog::new(view)
        .fields_on(table)?
        .into_iter()
        .map(|declared| {
            (
                declared.name,
                Declared {
                    kind: declared.kind,
                    required: declared.required,
                    assert: declared.assert.clone(),
                },
            )
        })
        .collect();

    for mutation in record.mutations() {
        match catalog_change(mutation)? {
            Some(CatalogChange::TableDefined(declared)) if declared.id == table => {
                name = declared.name.clone();
                schemafull = declared.schemafull;
            }
            Some(CatalogChange::FieldDefined(declared)) if declared.table == table => {
                fields.insert(
                    declared.name,
                    Declared {
                        kind: declared.kind,
                        required: declared.required,
                        assert: declared.assert.clone(),
                    },
                );
            }
            // A tombstone carries only the field's id, so what it removed has to
            // be read back from the state this record is applied on top of.
            Some(CatalogChange::FieldDropped(address)) => {
                if let Some(payload) = view.get(&address)? {
                    let dropped =
                        crate::catalog::FieldDefinition::from_value(&decode_payload(&payload)?)?;
                    if dropped.table == table {
                        fields.remove(&dropped.name);
                    }
                }
            }
            _ => {}
        }
    }

    Ok(TableSchema {
        name,
        fields,
        schemafull,
    })
}

/// Tables whose schema this record **tightens**, whose existing rows therefore
/// have to be re-checked.
///
/// Loosening never needs a re-check: a dropped declaration only widens what is
/// allowed, and a table redefined schemaless refuses less than it did.
fn tightened_tables(record: &LogRecord) -> Result<BTreeSet<TableAddress>> {
    let mut tables = BTreeSet::new();
    for mutation in record.mutations() {
        // Two things tighten a table, and the second one arrived when
        // `SCHEMAFULL` stopped being fixed at creation — this is the place the
        // comment that used to stand here pointed at.
        match catalog_change(mutation)? {
            Some(CatalogChange::FieldDefined(declared)) => {
                tables.insert((declared.namespace, declared.database, declared.table));
            }
            // `ALTER TABLE … SET SCHEMAFULL` rewrites the definition, so the
            // rows already stored are made to answer for it here — the same
            // stance `DEFINE FIELD` takes, and the reason it is not enough to
            // check the writes this transaction happens to carry.
            //
            // Creation writes a definition too and reaches this arm; that costs
            // nothing, because a table being created has no rows to re-check and
            // the ones the transaction writes alongside it are caught by the
            // first pass anyway.
            Some(CatalogChange::TableDefined(defined)) if defined.schemafull => {
                tables.insert((defined.namespace, defined.database, defined.id));
            }
            _ => {}
        }
    }
    Ok(tables)
}

/// Every row a table holds once this record is applied.
fn rows_after(
    view: &Transaction<'_>,
    record: &LogRecord,
    address: &TableAddress,
) -> Result<Vec<(RecordId, Vec<u8>)>> {
    let mut rows: BTreeMap<RecordId, Vec<u8>> = view
        .scan_table(address.0, address.1, address.2)?
        .into_iter()
        .collect();
    for mutation in record.mutations() {
        if (mutation.namespace, mutation.database, mutation.table) != *address {
            continue;
        }
        match &mutation.value {
            RecordValue::Present(payload) => rows.insert(mutation.id.clone(), payload.clone()),
            RecordValue::Tombstone => rows.remove(&mutation.id),
        };
    }
    Ok(rows.into_iter().collect())
}

/// Whether an address is in the reserved tenancy the catalog lives in.
///
/// Definitions are the schema; constraining them by one would be circular.
fn is_system(address: &TableAddress) -> bool {
    address.0 == crate::catalog::SYSTEM_NAMESPACE && address.1 == crate::catalog::SYSTEM_DATABASE
}
