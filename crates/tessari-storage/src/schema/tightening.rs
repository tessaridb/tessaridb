//! What a declaration that tightens a schema finds already stored.

use super::{Declared, TableAddress, TableSchema, Violation, check};
use crate::catalog::{Catalog, CatalogChange, catalog_change};
use crate::error::Result;
use crate::transaction::Transaction;
use std::collections::{BTreeMap, BTreeSet};
use tessari_encoding::{LogRecord, RecordValue, decode_payload};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

/// Every stored record of one table that disagrees with what it declares now.
///
/// The question a tightening statement answers by refusing, asked on its own.
/// A table that was strict from the start cannot hold a record that contradicts
/// it — the apply path saw every one of them. A table that **became** strict is
/// checked at the moment it became so and never again, and between those two
/// there is a table that has a declaration nobody has ever held its rows to: one
/// restored from a backup taken before the declaration, or one whose rows were
/// written while a field was optional and whose operator wants to know what
/// stands in the way of requiring it.
///
/// At most one violation per record, because [`check`] stops at the first: a
/// record that is wrong in two ways is one record to go and look at.
///
/// It reads the whole table, which is the only honest way to answer, and it is
/// therefore a statement an operator runs rather than something the store does
/// on its own.
pub fn violations(
    view: &mut Transaction<'_>,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
) -> Result<Vec<Violation>> {
    let schema = declared_schema(view, table)?;
    if schema.constrains_nothing() {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for (id, payload) in view.sweep_table(namespace, database, table)? {
        let Some(refusal) = check(&schema, &decode_payload(&payload)?, &id) else {
            continue;
        };
        found.extend(Violation::of(&refusal));
    }
    Ok(found)
}

/// The schema a table has as the catalog stands.
///
/// The half of [`build_schema`] that reads nothing but the catalog, so that a
/// caller with no log record in hand — [`violations`] — asks the same question
/// the apply path asks and cannot drift from it by asking a second way.
pub(crate) fn declared_schema(view: &mut Transaction<'_>, table: TableId) -> Result<TableSchema> {
    let defined = Catalog::new(view).table(table)?;
    let name = defined
        .as_ref()
        .map(|found| found.name.clone())
        .unwrap_or_default();
    let vault = defined.as_ref().is_some_and(|found| found.is_vault());
    let queue = defined.as_ref().is_some_and(|found| found.is_queue());
    let schemafull = defined.is_some_and(|found| found.schemafull);
    let fields: BTreeMap<String, Declared> = Catalog::new(view)
        .fields_on(table)?
        .into_iter()
        .map(|declared| {
            (
                declared.name,
                Declared {
                    kind: declared.kind,
                    required: declared.required,
                    secret: declared.secret,
                    assert: declared.assert.clone(),
                },
            )
        })
        .collect();
    Ok(TableSchema {
        name,
        fields,
        schemafull,
        vault,
        queue,
    })
}

/// The schema a table will have once this record is applied.
pub(crate) fn build_schema(
    view: &mut Transaction<'_>,
    record: &LogRecord,
    table: TableId,
) -> Result<TableSchema> {
    let TableSchema {
        mut name,
        mut fields,
        mut schemafull,
        mut vault,
        queue,
    } = declared_schema(view, table)?;

    for mutation in record.mutations() {
        match catalog_change(mutation)? {
            Some(CatalogChange::TableDefined(declared)) if declared.id == table => {
                name = declared.name.clone();
                schemafull = declared.schemafull;
                vault = declared.is_vault();
            }
            Some(CatalogChange::FieldDefined(declared)) if declared.table == table => {
                fields.insert(
                    declared.name,
                    Declared {
                        kind: declared.kind,
                        required: declared.required,
                        secret: declared.secret,
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
        vault,
        queue,
    })
}

/// Tables whose schema this record **tightens**, whose existing rows therefore
/// have to be re-checked.
///
/// Loosening never needs a re-check: a dropped declaration only widens what is
/// allowed, and a table redefined schemaless refuses less than it did.
pub(crate) fn tightened_tables(
    view: &mut Transaction<'_>,
    record: &LogRecord,
) -> Result<BTreeSet<TableAddress>> {
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
            // **Dropping a declaration is not always a loosening.** On a strict
            // table the declaration is what made the field legal, so removing it
            // leaves every stored record carrying that field contradicting the
            // table's own catalog — with nothing raised, because the statement
            // reads as a widening and was classified as one. The store's first
            // rule is that it never holds data disagreeing with its catalog, and
            // this was the way past it.
            //
            // The table is added whether or not it is strict, because that is
            // decided by the schema built below and a second reading here could
            // disagree with it. On a table that is not strict the re-check finds
            // nothing, and the cost is one scan on a statement `DEFINE FIELD`
            // already pays a scan for.
            Some(CatalogChange::FieldDropped(address)) => {
                if let Some(payload) = view.get(&address)? {
                    let dropped =
                        crate::catalog::FieldDefinition::from_value(&decode_payload(&payload)?)?;
                    tables.insert((dropped.namespace, dropped.database, dropped.table));
                }
            }
            _ => {}
        }
    }
    Ok(tables)
}

/// Every row a table holds once this record is applied.
pub(crate) fn rows_after(
    view: &Transaction<'_>,
    record: &LogRecord,
    address: &TableAddress,
) -> Result<Vec<(RecordId, Vec<u8>)>> {
    let mut rows: BTreeMap<RecordId, Vec<u8>> = view
        .sweep_table(address.0, address.1, address.2)?
        .into_iter()
        .collect();
    for mutation in record.mutations() {
        if (mutation.namespace, mutation.database, mutation.table) != *address {
            continue;
        }
        match mutation.value.value() {
            RecordValue::Present(payload) => rows.insert(mutation.id.clone(), payload.clone()),
            RecordValue::Tombstone => rows.remove(&mutation.id),
        };
    }
    Ok(rows.into_iter().collect())
}
