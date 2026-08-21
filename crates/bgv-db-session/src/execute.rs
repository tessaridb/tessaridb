//! Running one statement against the store.

use bgv_db_encoding::encode_payload;
use bgv_db_ql::{Name, RecordTarget, Span, StatementKind, TableRef};
use bgv_db_storage::{Catalog, EDGE_IN, EDGE_OUT, RecordAddress, TableShape, Transaction};
use std::collections::BTreeMap;

use bgv_db_types::{FieldId, FieldKind, IndexId, RecordId, RecordRef, TableId, Value};

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
                schemafull,
                edge,
                if_not_exists,
            } => self.define_table(
                transaction,
                name,
                TableShape {
                    schemafull: *schemafull,
                    edge: *edge,
                },
                *if_not_exists,
                span,
            ),
            // A space holds single values rather than named fields (ADR-0010),
            // so there is nothing for a schema to declare about one.
            StatementKind::DefineSpace {
                name,
                if_not_exists,
            } => self.define_table(
                transaction,
                name,
                TableShape::default(),
                *if_not_exists,
                span,
            ),
            StatementKind::DefineField {
                name,
                table,
                kind,
                if_not_exists,
            } => self.define_field(transaction, name, table, *kind, *if_not_exists),
            StatementKind::DropField { name, table } => {
                let (_, id) = self.resolve_table(transaction, table)?;
                let field = self.field_named(transaction, id, name)?;
                Catalog::new(transaction).drop_field(field)?;
                Ok(Outcome::Done)
            }
            StatementKind::DefineIndex {
                name,
                table,
                fields,
                unique,
                if_not_exists,
            } => self.define_index(transaction, name, table, fields, *unique, *if_not_exists),
            StatementKind::Relate {
                from,
                edges,
                to,
                value,
            } => self.relate(transaction, from, edges, to, value.as_ref()),
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

    /// Write an edge between two records.
    ///
    /// The edge is an ordinary record in the edge table, carrying `out` and `in`
    /// as record references — so it takes part in the transaction, replicates
    /// through the same path, and is found by the endpoint indexes the table was
    /// given when it was declared. Nothing about a graph needed its own keyspace.
    ///
    /// **The edge's identity is derived from its endpoints**, which makes
    /// `RELATE` idempotent: re-asserting a link that is already there replaces it
    /// rather than adding a second copy. That is the right default for a caller
    /// that re-states what it knows, and it is why two edges between the same
    /// pair in the same table are one edge with properties rather than two
    /// records (Q-41).
    fn relate(
        &self,
        transaction: &mut Transaction<'_>,
        from: &RecordTarget,
        edges: &TableRef,
        to: &RecordTarget,
        value: Option<&bgv_db_ql::Expr>,
    ) -> Result<Outcome> {
        let (context, edge_table) = self.resolve_table(transaction, edges)?;
        if !Catalog::new(transaction)
            .table(edge_table)?
            .is_some_and(|found| found.edge)
        {
            return Err(Error::NotAnEdgeTable {
                table: edges.name.text.clone(),
                span: edges.span,
            });
        }
        let (_, out) = self.address(transaction, from)?;
        let (_, into) = self.address(transaction, to)?;

        let mut fields = match value {
            Some(expression) => match self.evaluate(transaction, expression)? {
                Value::Object(given) => given,
                // A non-object edge property has nowhere to live beside the two
                // endpoints, so it is refused where it is written rather than
                // silently dropped.
                other => {
                    return Err(Error::EdgePropertiesNotAnObject {
                        found: other.type_name(),
                        span: edges.span,
                    });
                }
            },
            None => BTreeMap::new(),
        };
        fields.insert(
            EDGE_OUT.to_owned(),
            Value::Record(RecordRef::new(out.table, out.id.clone())),
        );
        fields.insert(
            EDGE_IN.to_owned(),
            Value::Record(RecordRef::new(into.table, into.id.clone())),
        );

        let id = RecordId::from(format!(
            "{}:{}->{}:{}",
            out.table, out.id, into.table, into.id
        ));
        let address = RecordAddress::new(context.namespace, context.database, edge_table, id);
        transaction.put(address, encode_payload(&Value::Object(fields)).into_bytes());
        Ok(Outcome::Done)
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
        shape: TableShape,
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
        Catalog::new(transaction).create_table(
            context.namespace,
            context.database,
            &name.text,
            shape,
        )?;
        Ok(Outcome::Done)
    }

    /// The definition is written here; its entries are built by the commit.
    ///
    /// Nothing more is needed at this layer, and that is the point: a catalog
    /// entry is an ordinary record, so index maintenance sees the definition in
    /// the same log record and projects the table's rows under it into the same
    /// batch. The definition and its entries land together or neither does,
    /// inside an open `BEGIN` as much as outside one.
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

    /// The declaration is written here; the rows answer for it in the commit.
    ///
    /// Nothing more is needed at this layer, for the reason `define_index` needs
    /// nothing more: a catalog entry is an ordinary record, so the store's schema
    /// check sees the declaration in the same log record and holds every row of
    /// the table to it — the ones already there and the ones this transaction
    /// writes. The declaration and the rows it constrains land together or
    /// neither does.
    fn define_field(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        table: &TableRef,
        kind: FieldKind,
        if_not_exists: bool,
    ) -> Result<Outcome> {
        let (_, id) = self.resolve_table(transaction, table)?;
        if if_not_exists && self.field_named(transaction, id, name).is_ok() {
            return Ok(Outcome::Done);
        }
        Catalog::new(transaction).create_field(id, &name.text, kind)?;
        Ok(Outcome::Done)
    }

    fn field_named(
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
