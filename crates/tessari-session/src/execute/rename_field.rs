//! `ALTER TABLE t RENAME FIELD a TO b` (ADR-0134 D9).

use tessari_encoding::decode_payload;
use tessari_ql::{Name, Span, TableRef};
use tessari_storage::{Catalog, RecordAddress, Transaction};
use tessari_types::{Assertion, Operand, Path, Value};

use crate::error::{Error, Result};
use crate::evaluate::Part;
use crate::outcome::Outcome;
use crate::session::Session;

use super::rename::naming;

impl Session<'_> {
    /// Rename a top-level field: its declarations move, and every record's
    /// value moves from the old name to the new one, in this transaction.
    ///
    /// Refused while something reads the field by its old name and would not
    /// move with it — an index over it, another field's `ASSERT`, a view or an
    /// event on the table — and on a vault, and while a record already holds a
    /// value under the new name, which the move would overwrite.
    pub(super) async fn rename_field(
        &self,
        transaction: &mut Transaction<'_>,
        table: &TableRef,
        (from, to): (&Name, &Name),
        span: Span,
    ) -> Result<Outcome> {
        let refused = |why: String| Error::FieldNotRenamed {
            field: from.text.clone(),
            why,
            span,
        };
        if from.text.contains('.') || to.text.contains('.') {
            return Err(refused("only a top-level field is renamed".to_owned()));
        }
        let (context, id) = self.resolve_table(transaction, table).await?;
        self.refuse_reading_a_part(transaction, id, Part::Whole)
            .await?;
        let Some(definition) = Catalog::new(transaction).table(id).await? else {
            return Ok(Outcome::Done);
        };
        if definition.is_vault() {
            return Err(refused(
                "a vault's fields are sealed under their names".to_owned(),
            ));
        }
        for index in Catalog::new(transaction)
            .indexes_on(context.namespace, context.database, id)
            .await?
        {
            if index.fields.iter().any(|path| path.root() == from.text) {
                return Err(refused(format!(
                    "index `{}` is over it — drop the index, rename, and define it again",
                    index.name
                )));
            }
        }
        let fields: Vec<_> = Catalog::new(transaction)
            .fields()
            .await?
            .into_iter()
            .filter(|field| field.table == id)
            .collect();
        if let Some(reader) = fields.iter().find(|field| {
            field
                .assert
                .as_ref()
                .is_some_and(|held| reads(held, &from.text))
        }) {
            return Err(refused(format!(
                "the `ASSERT` on `{}` reads it",
                reader.name
            )));
        }
        if let Some(first) = naming(transaction, &context, &definition.name)
            .await?
            .first()
        {
            return Err(refused(format!("{first} reads this table by text")));
        }
        let records = transaction
            .sweep_table(context.namespace, context.database, id)
            .await?;
        let mut moved = Vec::new();
        for (record, bytes) in records {
            let Value::Object(mut held) = decode_payload(&bytes)? else {
                continue;
            };
            if held.contains_key(&to.text) {
                return Err(refused(format!(
                    "a record already holds `{}`, which the move would overwrite",
                    to.text
                )));
            }
            if let Some(value) = held.remove(&from.text) {
                held.insert(to.text.clone(), value);
                moved.push((record, held));
            }
        }
        let prefix = format!("{}.", from.text);
        for field in &fields {
            let renamed = if field.name == from.text {
                Some(to.text.clone())
            } else {
                field
                    .name
                    .strip_prefix(&prefix)
                    .map(|rest| format!("{}.{rest}", to.text))
            };
            if let Some(name) = renamed {
                Catalog::new(transaction)
                    .rename_field(field.id, &name)
                    .await?;
            }
        }
        for (record, held) in moved {
            self.write_record(
                transaction,
                RecordAddress::new(context.namespace, context.database, id, record),
                Value::Object(held),
                None,
                span,
            )
            .await?;
        }
        Ok(Outcome::Done)
    }
}

/// Whether an assertion reads the field by name.
fn reads(assertion: &Assertion, field: &str) -> bool {
    match assertion {
        Assertion::Compare {
            against: Operand::Field(path),
            ..
        } => rooted(path, field),
        Assertion::Compare { .. } => false,
        Assertion::All(parts) | Assertion::Any(parts) => {
            parts.iter().any(|part| reads(part, field))
        }
        Assertion::Not(part) => reads(part, field),
    }
}

fn rooted(path: &Path, field: &str) -> bool {
    path.root() == field
}

impl Session<'_> {
    /// `ALTER FIELD f TYPE x USING <expr>`: each record's `f` becomes what the
    /// expression answers over that record, in this transaction; the new
    /// declaration then holds the rewritten records at the commit, as any
    /// `ALTER FIELD` does (ADR-0134 D9).
    pub(super) async fn convert_field(
        &self,
        transaction: &mut Transaction<'_>,
        table: &TableRef,
        field: &Name,
        using: &tessari_ql::Expr,
    ) -> Result<Outcome> {
        let (context, id) = self.resolve_table(transaction, table).await?;
        self.refuse_reading_a_part(transaction, id, Part::Whole)
            .await?;
        let records = transaction
            .sweep_table(context.namespace, context.database, id)
            .await?;
        for (record, bytes) in records {
            let held = decode_payload(&bytes)?;
            let value = self
                .evaluate_in(
                    transaction,
                    using,
                    crate::evaluate::Scope::of(&held).identified(&record),
                )
                .await?;
            let Value::Object(mut fields) = held else {
                continue;
            };
            if value.is_present() {
                fields.insert(field.text.clone(), value);
            } else {
                fields.remove(&field.text);
            }
            self.write_record(
                transaction,
                RecordAddress::new(context.namespace, context.database, id, record),
                Value::Object(fields),
                None,
                using.span,
            )
            .await?;
        }
        Ok(Outcome::Done)
    }
}
