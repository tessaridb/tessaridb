//! Running one statement against the store.

use bgv_db_encoding::encode_payload;
use bgv_db_ql::{Name, Span, StatementKind, TableRef};
use bgv_db_storage::{Catalog, Transaction};
use bgv_db_types::{IndexId, RecordId, TableId};

use crate::error::{Error, Result};
use crate::evaluate::{key_bound, within};
use crate::outcome::Outcome;
use crate::session::Session;

impl Session<'_> {
    pub(crate) fn execute(
        &self,
        transaction: &mut Transaction<'_>,
        kind: &StatementKind,
        span: Span,
    ) -> Result<Outcome> {
        match kind {
            StatementKind::DefineNamespace {
                name,
                if_not_exists,
            } => self.define_namespace(transaction, name, *if_not_exists),
            StatementKind::DefineDatabase {
                name,
                if_not_exists,
            } => self.define_database(transaction, name, *if_not_exists, span),
            StatementKind::DefineTable {
                name,
                if_not_exists,
            }
            | StatementKind::DefineSpace {
                name,
                if_not_exists,
            } => self.define_table(transaction, name, *if_not_exists, span),
            StatementKind::DefineIndex {
                name,
                table,
                fields,
                unique,
                if_not_exists,
            } => self.define_index(transaction, name, table, fields, *unique, *if_not_exists),
            StatementKind::DropTable { table } => {
                let (_, id) = self.resolve_table(transaction, table)?;
                Catalog::new(transaction).drop_table(id)?;
                Ok(Outcome::Done)
            }
            StatementKind::DropIndex { name, table } => {
                let (_, id) = self.resolve_table(transaction, table)?;
                let index = self.index_named(transaction, id, name)?;
                Catalog::new(transaction).drop_index(index)?;
                Ok(Outcome::Done)
            }
            StatementKind::Create { target, value } => {
                let (_, address) = self.address(transaction, target)?;
                // A create over a record that is already there is refused. The
                // alternative is silent replacement, which loses a record with
                // nothing anywhere to notice — and `UPDATE` and `SET` both say
                // replacement out loud.
                if transaction.get(&address)?.is_some() {
                    return Err(Error::RecordExists {
                        id: address.id.to_string(),
                        span: target.span,
                    });
                }
                let payload = self.evaluate(transaction, value)?;
                transaction.put(address, encode_payload(&payload).into_bytes());
                Ok(Outcome::Done)
            }
            StatementKind::Update { target, value } => {
                let (_, address) = self.address(transaction, target)?;
                if transaction.get(&address)?.is_none() {
                    return Err(Error::NoSuchRecord {
                        id: address.id.to_string(),
                        span: target.span,
                    });
                }
                let payload = self.evaluate(transaction, value)?;
                transaction.put(address, encode_payload(&payload).into_bytes());
                Ok(Outcome::Done)
            }
            // A key-value write replaces whatever was there, which is why it is
            // a different verb from `CREATE` rather than the same one.
            StatementKind::Set { target, value } => {
                let (_, address) = self.address(transaction, target)?;
                let payload = self.evaluate(transaction, value)?;
                transaction.put(address, encode_payload(&payload).into_bytes());
                Ok(Outcome::Done)
            }
            StatementKind::Delete { target } | StatementKind::Del { target } => {
                let (_, address) = self.address(transaction, target)?;
                transaction.delete(address);
                Ok(Outcome::Done)
            }
            StatementKind::Get { target } => {
                Ok(Outcome::Value(self.read_key(transaction, target)?))
            }
            StatementKind::Select(select) => {
                let (records, path) = self.read(transaction, select)?;
                Ok(Outcome::Records { records, path })
            }
            StatementKind::Keys { space, range } => self.keys(transaction, space, range.as_ref()),
            // The transaction verbs and `USE` never reach here; the session
            // handles them, because they change what the next statement runs in
            // rather than touching the store.
            StatementKind::Begin
            | StatementKind::Commit
            | StatementKind::Cancel
            | StatementKind::Use { .. } => Ok(Outcome::Done),
        }
    }

    fn define_namespace(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        if_not_exists: bool,
    ) -> Result<Outcome> {
        if if_not_exists
            && Catalog::new(transaction)
                .namespace_id(&name.text)?
                .is_some()
        {
            return Ok(Outcome::Done);
        }
        Catalog::new(transaction).create_namespace(&name.text)?;
        Ok(Outcome::Done)
    }

    fn define_database(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        if_not_exists: bool,
        span: Span,
    ) -> Result<Outcome> {
        let namespace = self.namespace_id(transaction, span)?;
        if if_not_exists
            && Catalog::new(transaction)
                .database_id(namespace, &name.text)?
                .is_some()
        {
            return Ok(Outcome::Done);
        }
        Catalog::new(transaction).create_database(namespace, &name.text)?;
        Ok(Outcome::Done)
    }

    fn define_table(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        if_not_exists: bool,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        if if_not_exists
            && Catalog::new(transaction)
                .table_id(context.namespace, context.database, &name.text)?
                .is_some()
        {
            return Ok(Outcome::Done);
        }
        Catalog::new(transaction).create_table(context.namespace, context.database, &name.text)?;
        Ok(Outcome::Done)
    }

    /// An index is declared here and **not** backfilled.
    ///
    /// Records that predate it are not writes, so maintenance never sees them:
    /// the index holds nothing about them, and a `UNIQUE` index does not
    /// constrain them either, until a backfill succeeds. That is stated in
    /// `docs/bgvql.md` §4 rather than left to be discovered.
    fn define_index(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        table: &TableRef,
        fields: &[Name],
        unique: bool,
        if_not_exists: bool,
    ) -> Result<Outcome> {
        let (_, id) = self.resolve_table(transaction, table)?;
        if if_not_exists && self.index_named(transaction, id, name).is_ok() {
            return Ok(Outcome::Done);
        }
        let fields = fields.iter().map(|field| field.text.clone()).collect();
        Catalog::new(transaction).create_index(id, &name.text, fields, unique)?;
        Ok(Outcome::Done)
    }

    fn index_named(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        name: &Name,
    ) -> Result<IndexId> {
        Catalog::new(transaction)
            .indexes_on(table)?
            .into_iter()
            .find(|index| index.name == name.text)
            .map(|index| index.id)
            .ok_or_else(|| Error::Unknown {
                entity: "index",
                name: name.text.clone(),
                span: name.span,
            })
    }

    /// The keys of a space, optionally bounded.
    ///
    /// The bound is applied to a scan rather than seeked to. The answer is the
    /// same either way; the cost is not, and `docs/bgvql.md` §6 says so rather
    /// than implying a seek this milestone does not perform.
    fn keys(
        &self,
        transaction: &mut Transaction<'_>,
        space: &TableRef,
        range: Option<&bgv_db_ql::RangeExpr>,
    ) -> Result<Outcome> {
        let (context, id) = self.resolve_table(transaction, space)?;
        let found = transaction.scan_table(context.namespace, context.database, id)?;
        let mut keys: Vec<RecordId> = found.into_iter().map(|(id, _)| id).collect();

        if let Some(range) = range {
            let start_value = self.evaluate(transaction, &range.start)?;
            let end_value = self.evaluate(transaction, &range.end)?;
            let start = key_bound(&start_value, range.start.span)?;
            let end = key_bound(&end_value, range.end.span)?;
            keys.retain(|key| within(key, &start, &end, range.inclusive));
        }
        Ok(Outcome::Keys(keys))
    }
}
