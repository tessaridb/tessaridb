//! How a user, their authorities and their grants are described.

use crate::error::Result;
use std::collections::BTreeMap;
use tessari_storage::{Catalog, GrantDefinition, Reach, UserDefinition};
use tessari_types::{DatabaseId, NamespaceId, TableId, Value};

/// One user, without the secret.
pub(crate) fn described_user(user: &UserDefinition) -> BTreeMap<String, Value> {
    let mut described = BTreeMap::from([("user".to_owned(), Value::from(user.name.as_str()))]);
    // Absent rather than a placeholder when no role summarises what the user
    // holds. A listing that printed `viewer` there would be describing an
    // authority they do not have, and the field below is the true answer.
    if let Some(role) = user.role {
        described.insert("role".to_owned(), Value::from(role.name()));
    }
    described
}

/// One user's authorities, with every reach named rather than numbered.
///
/// Named because a numbered reach is unreadable to the person who has to decide
/// whether it is right, and deciding that is the only reason to ask. Until the
/// enforcement wave lands this is also the **only** observable effect a grant
/// has, so a report without it would leave a grant unverifiable.
pub(crate) fn described_authorities(
    catalog: &Catalog<'_, '_>,
    user: &UserDefinition,
) -> Result<Value> {
    let mut described = Vec::new();
    for held in user.authorities.iter() {
        let reach = match held.reach {
            Reach::Store => "store".to_owned(),
            Reach::Namespace(namespace) => named_namespace(catalog, namespace)?,
            Reach::Database(namespace, database) => format!(
                "{}.{}",
                named_namespace(catalog, namespace)?,
                named_database(catalog, database)?
            ),
            // Never held — no authority comes at a shard's reach — and reported
            // faithfully if a stored row ever says so, because a report that
            // hid it would hide exactly the row worth seeing.
            Reach::Shard(namespace, database, table, shard) => format!(
                "{}.{}.{} shard {}",
                named_namespace(catalog, namespace)?,
                named_database(catalog, database)?,
                catalog
                    .table(table)?
                    .map_or_else(|| table.get().to_string(), |found| found.name),
                shard.get()
            ),
        };
        described.push(Value::Object(BTreeMap::from([
            ("authority".to_owned(), Value::from(held.kind.name())),
            ("reach".to_owned(), Value::from(reach.as_str())),
        ])));
    }
    Ok(Value::Array(described))
}

/// A namespace's name, or its number when the definition is gone.
///
/// A dropped namespace can still be named by an authority somebody holds, and
/// the number is a truthful answer where inventing a name would not be.
pub(crate) fn named_namespace(catalog: &Catalog<'_, '_>, namespace: NamespaceId) -> Result<String> {
    Ok(catalog
        .namespace(namespace)?
        .map_or_else(|| namespace.get().to_string(), |found| found.name))
}

/// A database's name, on the same terms.
pub(crate) fn named_database(catalog: &Catalog<'_, '_>, database: DatabaseId) -> Result<String> {
    Ok(catalog
        .database(database)?
        .map_or_else(|| database.get().to_string(), |found| found.name))
}

/// One grant, with the table named rather than numbered.
pub(crate) fn described_grant(catalog: &Catalog<'_, '_>, grant: &GrantDefinition) -> Result<Value> {
    let named = table_named(catalog, grant.table)?;
    Ok(Value::Object(BTreeMap::from([
        ("table".to_owned(), named),
        (
            "verbs".to_owned(),
            Value::Array(
                grant
                    .verbs
                    .iter()
                    .map(|verb| Value::from(verb.name()))
                    .collect(),
            ),
        ),
        (
            "fields".to_owned(),
            Value::Array(
                grant
                    .fields
                    .iter()
                    .map(|field| Value::from(field.as_str()))
                    .collect(),
            ),
        ),
    ])))
}

/// A table's name, or nothing when its definition has been dropped.
///
/// A grant outlives the table it names — dropping a table removes the definition
/// and leaves the grant — so this is an absence the report has to be able to
/// say rather than an error it can raise.
pub(crate) fn table_named(catalog: &Catalog<'_, '_>, table: TableId) -> Result<Value> {
    Ok(catalog
        .table(table)?
        .map_or(Value::None, |found| Value::from(found.name.as_str())))
}
