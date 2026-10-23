//! `REFERENCES t [ON DELETE …]` (ADR-0134 D10): a record value must name a
//! record the referenced table holds, and deleting one there rejects, cascades
//! to, or unsets the records naming it.
//!
//! Both halves read in the writer's transaction, so a target and a reference to
//! it written together are found together. A write guards the record it names,
//! so a delete of it committing first sends the write round again; a reference
//! written and its target deleted by two transactions whose commits do not see
//! each other can both land — the snapshot-isolation limit `docs/tessariql.md`
//! states — and the guard closes the order in which the delete commits first.

use tessari_encoding::decode_payload;
use tessari_ql::{Name, Span, TableRef};
use tessari_storage::{
    Catalog, FieldDefinition, FieldReference, OnDelete, RecordAddress, TableKind, Transaction,
};
use tessari_types::{FieldKind, RecordRef, Value};

use crate::error::{Error, Result};
use crate::session::Session;

impl Session<'_> {
    /// The reference a `DEFINE FIELD` states, resolved and checked: a `TYPE
    /// record` field, into a table or collection of the same database that is
    /// not split and does not expire.
    pub(super) async fn resolved_reference(
        &self,
        transaction: &mut Transaction<'_>,
        on: &TableRef,
        kind: &FieldKind,
        reference: &tessari_ql::Reference,
    ) -> Result<FieldReference> {
        let refused = |why: &str| Error::ReferenceRefused {
            why: why.to_owned(),
            span: reference.span,
        };
        if *kind != FieldKind::Record {
            return Err(refused("a reference is held by a `TYPE record` field"));
        }
        let target = TableRef {
            database: on.database.clone(),
            name: reference.table.clone(),
            span: reference.table.span,
        };
        let (_, table) = self.resolve_table(transaction, &target).await?;
        let definition = Catalog::new(transaction)
            .table(table)
            .await?
            .ok_or_else(|| refused("a table that exists"))?;
        if !matches!(definition.kind, TableKind::Table | TableKind::Collection) {
            return Err(refused("a reference names a table or a collection"));
        }
        if definition.shards.is_some() {
            return Err(refused(
                "a table that is not split — a split table's records live on several leaders",
            ));
        }
        if definition.expire.is_some() {
            return Err(refused(
                "a table whose records do not expire — an expiring record leaves with no delete \
                 for the reference to answer",
            ));
        }
        Ok(FieldReference {
            table,
            on_delete: match reference.on_delete {
                tessari_ql::OnDelete::Reject => OnDelete::Reject,
                tessari_ql::OnDelete::Cascade => OnDelete::Cascade,
                tessari_ql::OnDelete::Unset => OnDelete::Unset,
            },
        })
    }

    /// Every record value of a write that a `REFERENCES` field holds names a
    /// record its table holds, in this transaction.
    ///
    /// A record value is looked for before the catalog is read, so a write
    /// naming no record pays nothing.
    pub(crate) async fn references_hold(
        &self,
        transaction: &mut Transaction<'_>,
        address: &RecordAddress,
        payload: &Value,
        span: Span,
    ) -> Result<()> {
        let Value::Object(fields) = payload else {
            return Ok(());
        };
        if !fields
            .values()
            .any(|value| matches!(value, Value::Record(_)))
        {
            return Ok(());
        }
        let declared = Catalog::new(transaction)
            .fields_on(address.namespace, address.database, address.table)
            .await?;
        for field in declared {
            let (Some(reference), Some(Value::Record(named))) =
                (field.references, fields.get(&field.name))
            else {
                continue;
            };
            let target = RecordAddress::new(
                address.namespace,
                address.database,
                reference.table,
                named.id.clone(),
            );
            let held = named.table == reference.table && transaction.get(&target).await?.is_some();
            if !held {
                return Err(Error::NoSuchReference {
                    field: field.name,
                    table: self.table_name(transaction, reference.table).await?,
                    named: self.named(transaction, named).await?,
                    span,
                });
            }
            transaction.guard_record(&target);
        }
        Ok(())
    }

    /// What deleting `address` does to the records naming it, run after it is
    /// gone from this transaction so a cycle of cascades ends.
    pub(crate) async fn references_released(
        &self,
        transaction: &mut Transaction<'_>,
        address: &RecordAddress,
        span: Span,
    ) -> Result<()> {
        let Some(definition) = Catalog::new(transaction).table(address.table).await? else {
            return Ok(());
        };
        if definition.referenced_by.is_empty() {
            return Ok(());
        }
        let deleted = Value::Record(RecordRef::new(address.table, address.id.clone()));
        for field in self
            .naming(transaction, &definition.referenced_by, address.table)
            .await?
        {
            let Some(reference) = field.references else {
                continue;
            };
            for (id, bytes) in transaction
                .scan_table(field.namespace, field.database, field.table)
                .await?
            {
                let mut record = decode_payload(&bytes).map_err(tessari_storage::Error::from)?;
                let Value::Object(fields) = &mut record else {
                    continue;
                };
                if fields.get(&field.name) != Some(&deleted) {
                    continue;
                }
                let naming = RecordAddress::new(field.namespace, field.database, field.table, id);
                match reference.on_delete {
                    OnDelete::Reject => {
                        return Err(Error::StillReferenced {
                            record: format!("{}:{}", definition.name, address.id.to_literal()),
                            by: format!(
                                "{}.{}",
                                self.table_name(transaction, field.table).await?,
                                field.name
                            ),
                            span,
                        });
                    }
                    OnDelete::Cascade => {
                        Box::pin(self.delete_record(transaction, naming, span)).await?;
                    }
                    OnDelete::Unset => {
                        fields.remove(&field.name);
                        Box::pin(self.put_record(transaction, naming, record, span)).await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Refuse emptying or dropping a table a field still references.
    pub(super) async fn not_referenced(
        &self,
        transaction: &mut Transaction<'_>,
        table: &TableRef,
        verb: &'static str,
    ) -> Result<()> {
        let (_, id) = self.resolve_table(transaction, table).await?;
        let Some(definition) = Catalog::new(transaction).table(id).await? else {
            return Ok(());
        };
        // A field of the table itself goes with it, so it holds nothing back.
        if let Some(field) = self
            .naming(transaction, &definition.referenced_by, id)
            .await?
            .into_iter()
            .find(|field| field.table != id)
        {
            return Err(Error::ReferencedTable {
                table: definition.name,
                by: format!(
                    "{}.{}",
                    self.table_name(transaction, field.table).await?,
                    field.name
                ),
                verb,
                span: table.span,
            });
        }
        Ok(())
    }

    /// Keep the referenced table's list current when a field holding a
    /// reference is declared or dropped.
    pub(super) async fn note_reference(
        &self,
        transaction: &mut Transaction<'_>,
        field: &FieldDefinition,
        names: bool,
    ) -> Result<()> {
        if let Some(reference) = field.references {
            Catalog::new(transaction)
                .set_referenced_by(reference.table, field.id, names)
                .await?;
        }
        Ok(())
    }

    /// The fields among `ids` that still exist and still reference `table`.
    async fn naming(
        &self,
        transaction: &mut Transaction<'_>,
        ids: &[tessari_types::FieldId],
        table: tessari_types::TableId,
    ) -> Result<Vec<FieldDefinition>> {
        let mut found = Vec::new();
        for id in ids {
            if let Some(field) = Catalog::new(transaction).field(*id).await?
                && field
                    .references
                    .is_some_and(|reference| reference.table == table)
            {
                found.push(field);
            }
        }
        Ok(found)
    }

    async fn table_name(
        &self,
        transaction: &mut Transaction<'_>,
        table: tessari_types::TableId,
    ) -> Result<String> {
        Ok(Catalog::new(transaction)
            .table(table)
            .await?
            .map_or_else(|| format!("#{}", table.get()), |definition| definition.name))
    }

    /// A record value as `table:id`, by the table's name.
    async fn named(&self, transaction: &mut Transaction<'_>, named: &RecordRef) -> Result<String> {
        Ok(format!(
            "{}:{}",
            self.table_name(transaction, named.table).await?,
            named.id.to_literal()
        ))
    }

    /// The field a statement names, as its definition.
    pub(super) async fn field_definition(
        &self,
        transaction: &mut Transaction<'_>,
        table: tessari_types::TableId,
        name: &Name,
    ) -> Result<Option<FieldDefinition>> {
        Ok(Catalog::new(transaction)
            .declared_fields(table)
            .await?
            .into_iter()
            .find(|field| field.name == name.text))
    }
}
