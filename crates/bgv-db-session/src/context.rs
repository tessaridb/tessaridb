//! Turning the names in a statement into the ids the store addresses by.
//!
//! Every resolution here happens **inside the caller's transaction**, which is
//! what makes a script able to define a table and write to it in one unit: the
//! definition is visible to the transaction that wrote it before anyone else can
//! see it at all.
//!
//! It is also where a scoped user's tenancy is enforced, and that placement is
//! the point: a statement may name a database directly rather than through
//! `USE`, so the only check that covers every path is the one at the resolution
//! every path performs.

use bgv_db_ql::{Span, TableRef};
use bgv_db_storage::{Catalog, IndexDefinition, Transaction};
use bgv_db_types::{DatabaseId, NamespaceId, Path, TableId};

use crate::error::{Error, Result};
use crate::session::Session;

/// The tenancy a statement runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Context {
    pub(crate) namespace: NamespaceId,
    pub(crate) database: DatabaseId,
}

impl Session<'_> {
    /// The namespace the session selected.
    pub(crate) fn namespace_id(
        &self,
        transaction: &mut Transaction<'_>,
        span: Span,
    ) -> Result<NamespaceId> {
        let name = self
            .namespace()
            .ok_or(Error::NoNamespaceSelected { span })?
            .to_owned();
        Catalog::new(transaction)
            .namespace_id(&name)?
            .ok_or(Error::Unknown {
                entity: "namespace",
                name,
                span,
            })
    }

    /// The tenancy a statement runs in, with `qualified` overriding the session's
    /// database when the statement named one.
    pub(crate) fn context(
        &self,
        transaction: &mut Transaction<'_>,
        qualified: Option<&str>,
        span: Span,
    ) -> Result<Context> {
        let namespace = self.namespace_id(transaction, span)?;
        let name = match qualified {
            Some(name) => name.to_owned(),
            None => self
                .database()
                .ok_or(Error::NoDatabaseSelected { span })?
                .to_owned(),
        };
        let database = Catalog::new(transaction)
            .database_id(namespace, &name)?
            .ok_or_else(|| Error::Unknown {
                entity: "database",
                name: name.clone(),
                span,
            })?;
        self.permits(namespace, database, &name, span)?;
        Ok(Context {
            namespace,
            database,
        })
    }

    /// The tenancy a `namespace.database` reference names.
    ///
    /// `prod.orders` names a tenancy the way it names a table elsewhere: the
    /// qualifier is the namespace and the name the database. Written once here
    /// so `DEFINE USER … ON prod.orders` resolves through the same catalog reads
    /// every other reference does, and cannot name a database that is not there.
    pub(crate) fn tenancy_of(
        &self,
        transaction: &mut Transaction<'_>,
        named: &TableRef,
    ) -> Result<Context> {
        let Some(namespace) = named.database.as_ref() else {
            // Unqualified: the name is the database, inside the session's
            // namespace.
            return self.context(transaction, Some(&named.name.text), named.span);
        };
        let namespace_id = Catalog::new(transaction)
            .namespace_id(&namespace.text)?
            .ok_or_else(|| Error::Unknown {
                entity: "namespace",
                name: namespace.text.clone(),
                span: named.span,
            })?;
        let database = Catalog::new(transaction)
            .database_id(namespace_id, &named.name.text)?
            .ok_or_else(|| Error::Unknown {
                entity: "database",
                name: named.name.text.clone(),
                span: named.span,
            })?;
        self.permits(namespace_id, database, &named.name.text, named.span)?;
        Ok(Context {
            namespace: namespace_id,
            database,
        })
    }

    /// The table a reference names, and the tenancy it lives in.
    pub(crate) fn resolve_table(
        &self,
        transaction: &mut Transaction<'_>,
        table: &TableRef,
    ) -> Result<(Context, TableId)> {
        let qualified = table.database.as_ref().map(|name| name.text.as_str());
        let context = self.context(transaction, qualified, table.span)?;
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &table.name.text)?
            .ok_or_else(|| Error::Unknown {
                entity: "table",
                name: table.name.text.clone(),
                span: table.span,
            })?;
        Ok((context, id))
    }

    /// The index that answers an equality on exactly this value, if one exists.
    ///
    /// Absence is not an error. Which access path a filter takes is decided by
    /// what exists, not by how the query was written — that is what lets an index
    /// be added later without rewriting a single query.
    ///
    /// The match is on the whole path, so an index on `address.city` serves a
    /// filter on `address.city` and one on `address` does not. That is the same
    /// rule composite indexes already follow: an index answers the question it
    /// projects, and no other.
    pub(crate) fn index_on_path(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        path: &Path,
    ) -> Result<Option<IndexDefinition>> {
        Ok(Catalog::new(transaction)
            .indexes_on(table)?
            .into_iter()
            .find(|index| index.fields.as_slice() == [path.clone()]))
    }

    /// The index whose entries are stored in this field's order, if one exists.
    ///
    /// Wider than [`Context::index_on_path`] by exactly one case: an index whose
    /// **leading** field is this path. `(last, first)` stores its entries by
    /// `last` first of all, so it holds the order `ORDER BY last` asks for — a
    /// fact about the key layout that an exact field-list match cannot see.
    ///
    /// A later field does not qualify and is not nearly-right: the entries for
    /// `first` are grouped inside each `last`, so reading them in key order
    /// yields `first` restarted once per `last`, which is not that field's order
    /// at any point.
    ///
    /// **An exact match still wins.** A single-field index on `last` and a
    /// composite `(last, first)` hold the same order, but the shorter one is
    /// fewer bytes per entry, so preferring it keeps every read this store
    /// already serves on the path it already took.
    ///
    /// Deliberately separate rather than a widening of `index_on_path`, whose
    /// other three callers ask a different question — *which index answers this
    /// value* — and for whom a leading match would be wrong.
    pub(crate) fn index_ordering_on_path(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        path: &Path,
    ) -> Result<Option<IndexDefinition>> {
        let indexes = Catalog::new(transaction).indexes_on(table)?;
        if let Some(exact) = indexes
            .iter()
            .find(|index| index.fields.as_slice() == [path.clone()])
        {
            return Ok(Some(exact.clone()));
        }
        Ok(indexes
            .into_iter()
            .find(|index| index.fields.first() == Some(path)))
    }
}
