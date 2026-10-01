//! Field defaults, and finding fields and indexes by name.

use tessari_ql::Name;
use tessari_storage::{Catalog, IndexDefinition, Transaction};

use tessari_types::{FieldId, TableId, Value};

use crate::error::{Error, Result};
use crate::session::Session;

use super::PartialSeal;

impl Session<'_> {
    /// A record with its table's defaults filled in.
    ///
    /// Applied by the session rather than by the store, because a default is
    /// about what gets **written** and not about what is valid: the value is
    /// materialised before the store ever sees it, so a replica applies a record
    /// that already carries it and nothing has to be evaluated twice.
    ///
    /// Only fields the record leaves absent are filled. A record that supplies
    /// `null` supplied a value, and a default replacing it would make `null`
    /// unwritable on any field that has one.
    /// Fold defaults in, and tell a partial vault edit about anything they added.
    ///
    /// A default fires only for a declared field the payload is missing, so on an
    /// edit it fires only for a field declared *after* the record was written —
    /// and the value it writes is plaintext. If such a field is a secret and the
    /// reseal never hears its name, it is carried past the sealer and reaches the
    /// encoder in the clear, which is the one outcome this whole module exists to
    /// make unreachable.
    ///
    /// So the names the defaults introduce join the named set. That is sound for
    /// the same reason the rest of the set is: they were produced here, not read
    /// back out of the store, so they are plaintext by construction rather than
    /// by inspection.
    pub(super) fn defaults_over(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        payload: Value,
        partial: Option<PartialSeal>,
    ) -> Result<(Value, Option<PartialSeal>)> {
        let before: Vec<String> = match (&partial, &payload) {
            (Some(_), Value::Object(fields)) => fields.keys().cloned().collect(),
            _ => Vec::new(),
        };
        let payload = self.with_defaults(transaction, table, payload)?;
        let Some(mut edit) = partial else {
            return Ok((payload, None));
        };
        if let Value::Object(fields) = &payload {
            for name in fields.keys() {
                if !before.contains(name) {
                    edit.named.insert(name.clone());
                }
            }
        }
        Ok((payload, Some(edit)))
    }

    pub(super) fn with_defaults(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        payload: Value,
    ) -> Result<Value> {
        let Value::Object(mut fields) = payload else {
            return Ok(payload);
        };
        let declared = Catalog::new(transaction).fields_on(table)?;
        for field in declared {
            let Some(written) = field.default else {
                continue;
            };
            if fields.get(&field.name).is_some_and(Value::is_present) {
                continue;
            }
            let expression = tessari_ql::parse_expression(&written)?;
            let value = self.evaluate(transaction, &expression)?;
            fields.insert(field.name, value);
        }
        Ok(Value::Object(fields))
    }

    pub(super) fn field_named(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        name: &Name,
    ) -> Result<FieldId> {
        Catalog::new(transaction)
            .fields_on(table)?
            .into_iter()
            .find(|field| field.name == name.text)
            .map(|field| field.id)
            .ok_or_else(|| Error::Unknown {
                entity: "field",
                name: name.text.clone(),
                span: name.span,
            })
    }

    /// The definition of the index this table calls `name`.
    ///
    /// The definition rather than the id, because a caller that only wants the
    /// id can take it — and the one caller that wants the whole thing would
    /// otherwise have to look it up twice and handle a second absence that
    /// cannot happen.
    pub(super) fn index_named(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        name: &Name,
    ) -> Result<IndexDefinition> {
        Catalog::new(transaction)
            .field_indexes_on(table)?
            .into_iter()
            .find(|index| index.name == name.text)
            .ok_or_else(|| Error::Unknown {
                entity: "index",
                name: name.text.clone(),
                span: name.span,
            })
    }
}
