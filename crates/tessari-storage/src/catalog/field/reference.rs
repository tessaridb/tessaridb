//! `REFERENCES t [ON DELETE …]` on a field: which table its record values name,
//! and what deleting one of that table's records does (ADR-0134 D10).
//!
//! Held by the table's **id**, so a rename of the referenced table moves nothing
//! here. Stored as an optional part of the field's definition — a definition
//! written before references existed reads as one with none.

use std::collections::BTreeMap;

use tessari_types::{FieldId, TableId, Value};

use super::super::definition::{field_id, number, object};
use super::super::{Catalog, system};
use crate::error::{Error, Result};

const ENTITY: &str = "reference";
const FIELD_TABLE: &str = "table";
const FIELD_ON_DELETE: &str = "on_delete";

/// What deleting a referenced record does to the records naming it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnDelete {
    /// The delete is refused while a record names it.
    Reject,
    /// The records naming it are deleted too, their events run.
    Cascade,
    /// The field is set to `NONE` in the records naming it, their events run.
    Unset,
}

impl OnDelete {
    /// The word the definition is stored and shown by.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Reject => "reject",
            Self::Cascade => "cascade",
            Self::Unset => "unset",
        }
    }

    fn parse(word: &str) -> Option<Self> {
        match word {
            "reject" => Some(Self::Reject),
            "cascade" => Some(Self::Cascade),
            "unset" => Some(Self::Unset),
            _ => None,
        }
    }
}

/// The table a field's record values must name, and what a delete there does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldReference {
    /// The table referenced, in the field's own database.
    pub table: TableId,
    /// What deleting one of its records does.
    pub on_delete: OnDelete,
}

impl FieldReference {
    /// The value stored in the field's definition.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([
            (FIELD_TABLE.to_owned(), number(self.table.get())),
            (
                FIELD_ON_DELETE.to_owned(),
                Value::from(self.on_delete.name()),
            ),
        ]))
    }

    /// Read one back; an absence is no reference.
    ///
    /// # Errors
    ///
    /// [`Error::CatalogMalformed`] when the part is there and is not one this
    /// binary can read — guessing would let a delete through that the node
    /// which wrote it would refuse.
    pub(crate) fn from_field(held: Option<&Value>) -> Result<Option<Self>> {
        let Some(held) = held.filter(|held| !matches!(held, Value::None)) else {
            return Ok(None);
        };
        let fields = object(held, ENTITY)?;
        let on_delete = match fields.get(FIELD_ON_DELETE) {
            Some(Value::String(word)) => OnDelete::parse(word),
            _ => None,
        }
        .ok_or(Error::CatalogMalformed {
            entity: ENTITY,
            field: FIELD_ON_DELETE,
            found: held.type_name(),
        })?;
        Ok(Some(Self {
            table: TableId::new(field_id(fields, FIELD_TABLE, ENTITY)?),
            on_delete,
        }))
    }
}

impl Catalog<'_, '_> {
    /// Record on a table that `field` references it, or that it no longer does,
    /// so a delete there asks only the fields that name it.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub async fn set_referenced_by(
        &mut self,
        table: TableId,
        field: FieldId,
        names: bool,
    ) -> Result<()> {
        let Some(mut definition) = self.table(table).await? else {
            return Ok(());
        };
        definition.referenced_by.retain(|held| *held != field);
        if names {
            definition.referenced_by.push(field);
        }
        self.write(system::TABLES, table.get(), &definition.to_value());
        Ok(())
    }
}
