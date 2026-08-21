//! Turning the names in a statement into the ids the store addresses by.
//!
//! Every resolution here happens **inside the caller's transaction**, which is
//! what makes a script able to define a table and write to it in one unit: the
//! definition is visible to the transaction that wrote it before anyone else can
//! see it at all.

use bgv_db_ql::{Span, TableRef};
use bgv_db_storage::{Catalog, IndexDefinition, Transaction};
use bgv_db_types::{DatabaseId, NamespaceId, TableId};

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
            .ok_or(Error::Unknown {
                entity: "database",
                name,
                span,
            })?;
        Ok(Context {
            namespace,
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

    /// The index that answers a filter on exactly this field.
    ///
    /// A filter over an unindexed field is refused here rather than executed as
    /// a scan-and-filter, so that the only statement whose cost is a whole table
    /// is the one that says so.
    pub(crate) fn index_on_field(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        field: &str,
        span: Span,
    ) -> Result<IndexDefinition> {
        Catalog::new(transaction)
            .indexes_on(table)?
            .into_iter()
            .find(|index| {
                index.fields.len() == 1 && index.fields.first().is_some_and(|f| f == field)
            })
            .ok_or_else(|| Error::NoIndexOnField {
                field: field.to_owned(),
                span,
            })
    }
}
