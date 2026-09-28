//! Reading a catalog row's fields back into a definition.

use super::{FIELD_CEILING, FIELD_ID, FIELD_IDENTITY, FIELD_NAME};
use crate::error::{Error, Result};
use std::collections::BTreeMap;
use tessari_types::{IdentityKind, Number, Value};

/// The byte ceiling a table's entry carries, when it carries one.
///
/// Absent reads as no ceiling rather than as a fault, which is what every entry
/// written before the clause existed is. Present-but-not-a-positive-count is a
/// fault, because a stored zero would have to mean either "unbounded" or
/// "accepts nothing" and the entry does not say which.
pub(crate) fn ceiling(fields: &BTreeMap<String, Value>) -> Result<Option<u64>> {
    let malformed = || Error::CatalogMalformed {
        entity: "table",
        field: FIELD_CEILING,
        found: "not a whole number of bytes above zero",
    };
    match fields.get(FIELD_CEILING) {
        None => Ok(None),
        Some(Value::Number(Number::Integer(held))) => u64::try_from(*held)
            .ok()
            .filter(|held| *held > 0)
            .map(Some)
            .ok_or_else(malformed),
        Some(_) => Err(malformed()),
    }
}

/// A record counter read back out of a stored value.
///
/// A negative integer is malformed rather than wrapped into an enormous count:
/// nothing writes one, so one being there means this record is not what this
/// build takes it for, and reading it as `18446744073709551615` would hand the
/// table an identity space it has already spent.
pub(crate) fn count_of(value: &Value, entity: &'static str, field: &'static str) -> Result<u64> {
    let malformed = |found: &'static str| Error::CatalogMalformed {
        entity,
        field,
        found,
    };
    let Value::Number(Number::Integer(raw)) = value else {
        return Err(malformed(value.type_name()));
    };
    u64::try_from(*raw).map_err(|_| malformed("number"))
}

pub(crate) fn object<'a>(
    value: &'a Value,
    entity: &'static str,
) -> Result<&'a BTreeMap<String, Value>> {
    match value {
        Value::Object(fields) => Ok(fields),
        other => Err(Error::CatalogMalformed {
            entity,
            field: FIELD_ID,
            found: other.type_name(),
        }),
    }
}

/// An identifier read back out of a stored value.
///
/// An integer too wide for the identifier is refused rather than narrowed: a
/// truncated id addresses a different entity, and nothing downstream could tell.
pub(crate) fn id_of(value: &Value, entity: &'static str, field: &'static str) -> Result<u32> {
    let malformed = |found: &'static str| Error::CatalogMalformed {
        entity,
        field,
        found,
    };
    let Value::Number(Number::Integer(raw)) = value else {
        return Err(malformed(value.type_name()));
    };
    u32::try_from(*raw).map_err(|_| malformed("number"))
}

pub(crate) fn field_id(
    fields: &BTreeMap<String, Value>,
    field: &'static str,
    entity: &'static str,
) -> Result<u32> {
    match fields.get(field) {
        Some(value) => id_of(value, entity, field),
        None => Err(Error::CatalogMalformed {
            entity,
            field,
            found: "none",
        }),
    }
}

/// A boolean property of a definition.
///
/// Absent reads as `false`, so an entry written before the property existed is
/// readable rather than refused. A value of the wrong type is **not** read as
/// `false`: something wrote a well-formed value that is not a flag, which is an
/// integrity problem, and defaulting it would silently drop a constraint.
///
/// One reader for every flag, because a second copy that drifted would not fail
/// to compile — it would change what a table is.
pub(crate) fn flag(
    fields: &BTreeMap<String, Value>,
    field: &'static str,
    entity: &'static str,
) -> Result<bool> {
    match fields.get(field) {
        Some(Value::Bool(declared)) => Ok(*declared),
        None => Ok(false),
        Some(other) => Err(Error::CatalogMalformed {
            entity,
            field,
            found: other.type_name(),
        }),
    }
}

/// How a table names a record the caller did not name.
///
/// Absent reads as [`IdentityKind::Int`] — the flags' contract, for the same
/// reason: a table written before the property existed is one that used the
/// default, not one whose declaration is unreadable.
///
/// A word this build does not recognise is **refused**, which is the one place
/// this differs from a flag. An unknown flag can only be a `true` nobody wrote;
/// an unknown identity scheme is a table already naming records some other way,
/// and reading it as `int` would put two schemes in one table.
pub(crate) fn identity_kind(
    fields: &BTreeMap<String, Value>,
    entity: &'static str,
) -> Result<IdentityKind> {
    match fields.get(FIELD_IDENTITY) {
        None => Ok(IdentityKind::Int),
        Some(Value::String(word)) => IdentityKind::parse(word).ok_or(Error::CatalogMalformed {
            entity,
            field: FIELD_IDENTITY,
            found: "an unknown identity scheme",
        }),
        Some(other) => Err(Error::CatalogMalformed {
            entity,
            field: FIELD_IDENTITY,
            found: other.type_name(),
        }),
    }
}

pub(crate) fn field_name(fields: &BTreeMap<String, Value>, entity: &'static str) -> Result<String> {
    match fields.get(FIELD_NAME) {
        Some(Value::String(name)) => Ok(name.clone()),
        other => Err(Error::CatalogMalformed {
            entity,
            field: FIELD_NAME,
            found: other.map_or("none", Value::type_name),
        }),
    }
}
