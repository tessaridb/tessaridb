//! Which tables a user may reach, and for what.
//!
//! # The rule, in one sentence
//!
//! **A user's grants, if they have any, are the whole story; a user with none is
//! governed by their role.**
//!
//! That is what lets this be added to a store that already has users without
//! changing what any of them may do, and it is what makes the feature able to do
//! its job: a role can only widen, and a permission system that cannot narrow is
//! decoration.
//!
//! # A grant is identified by its pair
//!
//! Not by an allocated id. The same rule an edge follows, and for the same
//! reason: it makes granting **idempotent**. Granting `read` twice is one grant,
//! so nothing accumulates that a revocation would have to find twice, and
//! re-running a provisioning script is not a way to build up state nobody meant.
//!
//! # A catalog entry like any other
//!
//! An ordinary record in the system tenancy (ADR-0009), so a grant takes part in
//! the transaction that issued it, survives a reopen without anything being kept
//! in memory, and reaches every replica through the same apply path as the table
//! it names.

use std::collections::BTreeMap;

use bgv_db_encoding::decode_payload;
use bgv_db_types::{RecordId, TableId, Value};

use super::user::Verb;
use super::{Catalog, system};
use crate::error::{Error, Result};

const FIELD_USER: &str = "user";
const FIELD_TABLE: &str = "table";
const FIELD_VERBS: &str = "verbs";
const FIELD_FIELDS: &str = "fields";

const ENTITY: &str = "grant";

/// What one user may do to one table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantDefinition {
    /// The user the grant is for.
    pub user: u32,
    /// The table it names.
    pub table: TableId,
    /// What may be done to it, smallest first and never repeated.
    pub verbs: Vec<Verb>,
    /// Which fields may be read, or empty for all of them.
    ///
    /// Empty is no restriction, which is the same rule the grant itself has one
    /// level up: what is named is the whole story, and naming nothing names no
    /// limit. Sorted and deduplicated for the same reason the verbs are — a
    /// stored set that depends on the order somebody typed it in is two values
    /// for one fact.
    pub fields: Vec<String>,
}

impl GrantDefinition {
    /// The record id a grant is stored under.
    ///
    /// Derived from the pair rather than allocated, which is what makes a repeat
    /// grant replace rather than accumulate. Text rather than a packed integer
    /// because a catalog somebody is reading by hand should say what it holds.
    #[must_use]
    pub fn identity(user: u32, table: TableId) -> RecordId {
        RecordId::Text(format!("{user}/{}", table.get()))
    }

    /// The value written to the catalog.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([
            (FIELD_USER.to_owned(), number(self.user)),
            (FIELD_TABLE.to_owned(), number(self.table.get())),
            (
                FIELD_VERBS.to_owned(),
                Value::Array(
                    self.verbs
                        .iter()
                        .map(|verb| Value::from(verb.name()))
                        .collect(),
                ),
            ),
            (
                FIELD_FIELDS.to_owned(),
                Value::Array(
                    self.fields
                        .iter()
                        .map(|name| Value::from(name.as_str()))
                        .collect(),
                ),
            ),
        ]))
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] when the stored value is not one.
    pub fn from_value(value: &Value) -> Result<Self> {
        let Value::Object(fields_in) = value else {
            return Err(malformed("body", "not an object"));
        };
        let user = whole(fields_in.get(FIELD_USER), FIELD_USER)?;
        let table = TableId::new(whole(fields_in.get(FIELD_TABLE), FIELD_TABLE)?);
        let Some(Value::Array(held)) = fields_in.get(FIELD_VERBS) else {
            return Err(malformed(FIELD_VERBS, "not an array"));
        };
        let mut verbs = Vec::new();
        for name in held {
            let Value::String(name) = name else {
                return Err(malformed(FIELD_VERBS, "not a name"));
            };
            // A verb this build does not know is corruption rather than
            // something to ignore: silently dropping it would quietly widen or
            // narrow what a user may do, and neither is a thing to guess at.
            verbs.push(Verb::parse(name).ok_or_else(|| malformed(FIELD_VERBS, "not a verb"))?);
        }
        verbs.sort_unstable();
        verbs.dedup();
        // Absent rather than empty in a store written before fields existed,
        // which reads as no restriction — the same thing an empty list means.
        let mut fields = Vec::new();
        if let Some(held) = fields_in.get(FIELD_FIELDS) {
            let Value::Array(named) = held else {
                return Err(malformed(FIELD_FIELDS, "not an array"));
            };
            for name in named {
                let Value::String(name) = name else {
                    return Err(malformed(FIELD_FIELDS, "not a name"));
                };
                fields.push(name.clone());
            }
        }
        fields.sort_unstable();
        fields.dedup();
        Ok(Self {
            user,
            table,
            verbs,
            fields,
        })
    }
}

fn malformed(field: &'static str, found: &'static str) -> Error {
    Error::CatalogMalformed {
        entity: ENTITY,
        field,
        found,
    }
}

fn number(value: u32) -> Value {
    Value::from(i64::from(value))
}

fn whole(value: Option<&Value>, field: &'static str) -> Result<u32> {
    let Some(Value::Number(bgv_db_types::Number::Integer(held))) = value else {
        return Err(malformed(field, "not a whole number"));
    };
    u32::try_from(*held).map_err(|_| malformed(field, "out of range"))
}

impl Catalog<'_, '_> {
    /// Give a user these verbs on this table, replacing what they had.
    ///
    /// Replacing rather than merging, so the statement says what the result is
    /// rather than what it adds — an operator reading `GRANT read ON users TO
    /// ada` should not have to know what ada had before to know what she has
    /// now.
    ///
    /// # Errors
    ///
    /// Returns an error when the write cannot be staged.
    pub fn grant(
        &mut self,
        user: u32,
        table: TableId,
        verbs: &[Verb],
        fields: &[String],
    ) -> Result<GrantDefinition> {
        let mut verbs = verbs.to_vec();
        verbs.sort_unstable();
        verbs.dedup();
        let mut fields = fields.to_vec();
        fields.sort_unstable();
        fields.dedup();
        let definition = GrantDefinition {
            user,
            table,
            verbs,
            fields,
        };
        self.transaction.put(
            system::address(system::GRANTS, GrantDefinition::identity(user, table)),
            bgv_db_encoding::encode_payload(&definition.to_value()).into_bytes(),
        );
        Ok(definition)
    }

    /// Take a grant away entirely.
    pub fn revoke(&mut self, user: u32, table: TableId) {
        self.transaction.delete(system::address(
            system::GRANTS,
            GrantDefinition::identity(user, table),
        ));
    }

    /// Every grant this user holds.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub fn grants_for(&self, user: u32) -> Result<Vec<GrantDefinition>> {
        let mut found = Vec::new();
        for (_, payload) in self.transaction.scan_table(
            system::SYSTEM_NAMESPACE,
            system::SYSTEM_DATABASE,
            system::GRANTS,
        )? {
            let held = GrantDefinition::from_value(&decode_payload(&payload)?)?;
            if held.user == user {
                found.push(held);
            }
        }
        Ok(found)
    }
}
