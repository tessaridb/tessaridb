//! Running one statement against the store.

use bgv_db_encoding::{Roles, decode_payload, encode_payload};
use bgv_db_ql::{Assignment, Edit, FieldPath, Name, RecordTarget, Span, StatementKind, TableRef};
use bgv_db_storage::{
    Catalog, EDGE_IN, EDGE_OUT, FieldShape, IndexDefinition, IndexShape, RecordAddress, TableShape,
    Transaction, VectorDistance,
};
use std::collections::BTreeMap;

use bgv_db_types::{
    Analyzer, FieldId, FieldKind, Filter, Path, RecordId, RecordRef, Step, TableId, Value,
};

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
                    bucket: false,
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
                required,
                default,
                analyzer,
                assert,
                if_not_exists,
            } => self.define_field(
                transaction,
                name,
                table,
                *kind,
                FieldShape {
                    required: *required,
                    default: default.as_ref().map(|written| written.text.clone()),
                    analyzer: analyzer.as_ref().map(|named| named.text.clone()),
                    assert: assert.clone(),
                },
                *if_not_exists,
            ),
            StatementKind::DefineAnalyzer {
                name,
                filters,
                if_not_exists,
            } => self.define_analyzer(transaction, name, filters, *if_not_exists),
            StatementKind::DefineUser {
                name,
                scope,
                role,
                password,
                if_not_exists,
            } => self.define_user(
                transaction,
                name,
                scope.as_ref(),
                role,
                password,
                *if_not_exists,
                span,
            ),
            StatementKind::DefineNode { roles, endpoints } => {
                self.define_node(roles.as_deref(), endpoints.as_deref())
            }
            StatementKind::DefineReplica {
                name,
                endpoint,
                if_not_exists,
            } => self.define_replica(transaction, name, endpoint, *if_not_exists),
            StatementKind::DropUser { name } => self.drop_user(transaction, name),
            StatementKind::Grant {
                verbs,
                table,
                fields,
                user,
            } => self.grant(transaction, verbs, table, fields, user, span),
            StatementKind::Revoke { verbs, table, user } => {
                self.revoke(transaction, verbs, table, user, span)
            }
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
                search,
                vector,
                if_not_exists,
            } => self.define_index(
                transaction,
                name,
                table,
                fields,
                IndexShape {
                    unique: *unique,
                    search: *search,
                    vector: match vector {
                        Some(named) => {
                            Some(VectorDistance::parse(&named.text).ok_or_else(|| {
                                Error::NoSuchDistance {
                                    name: named.text.clone(),
                                    span: named.span,
                                }
                            })?)
                        }
                        None => None,
                    },
                },
                *if_not_exists,
            ),
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
                Catalog::new(transaction).drop_index(index.id)?;
                Ok(Outcome::Done)
            }
            // Writing the definition again is the whole statement: the entries
            // are derived from it, so a definition arriving in a log record is
            // what makes them get built — see `Catalog::rebuild_index`.
            StatementKind::RebuildIndex { name, table } => {
                let (_, id) = self.resolve_table(transaction, table)?;
                let index = self.index_named(transaction, id, name)?;
                Catalog::new(transaction).rebuild_index(&index);
                Ok(Outcome::Done)
            }
            StatementKind::Create { target, value } => {
                let (_, address) = self.writable(transaction, target)?;
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
                let payload = self.with_defaults(transaction, address.table, payload)?;
                transaction.put(address, encode_payload(&payload).into_bytes());
                Ok(Outcome::Done)
            }
            StatementKind::Update { target, edit } => {
                let (_, address) = self.writable(transaction, target)?;
                let Some(existing) = transaction.get(&address)? else {
                    return Err(Error::NoSuchRecord {
                        id: address.id.to_string(),
                        span: target.span,
                    });
                };
                let payload = match edit {
                    // Replacing the whole record is a write like a create, so
                    // the defaults apply to it the same way.
                    Edit::Whole(value) => self.evaluate(transaction, value)?,
                    Edit::Fields(assignments) => {
                        self.edited(transaction, &existing, assignments, target.span)?
                    }
                };
                // One rule rather than two: the result of either shape is a
                // record being written, so `REQUIRED` + `DEFAULT` keeps meaning
                // "this field always holds a value" even when a caller sets one
                // to `none`.
                let payload = self.with_defaults(transaction, address.table, payload)?;
                transaction.put(address, encode_payload(&payload).into_bytes());
                Ok(Outcome::Done)
            }
            // A key-value write replaces whatever was there, which is why it is
            // a different verb from `CREATE` rather than the same one.
            StatementKind::Set { target, value } => {
                let (_, address) = self.writable(transaction, target)?;
                let payload = self.evaluate(transaction, value)?;
                transaction.put(address, encode_payload(&payload).into_bytes());
                Ok(Outcome::Done)
            }
            StatementKind::Delete { target } | StatementKind::Del { target } => {
                // A file's bytes go with its metadata, in this commit. A bucket
                // that kept chunks nothing describes would leak space nothing
                // could ever find its way back to.
                self.clear_file(transaction, target)?;
                let (_, address) = self.address(transaction, target)?;
                transaction.delete(address);
                Ok(Outcome::Done)
            }
            StatementKind::DeleteWhere { table, condition } => {
                self.delete_where(transaction, table, condition)
            }
            StatementKind::DefineBucket {
                name,
                if_not_exists,
            } => self.define_table(
                transaction,
                name,
                TableShape {
                    schemafull: false,
                    edge: false,
                    bucket: true,
                },
                *if_not_exists,
                span,
            ),
            StatementKind::Put {
                target,
                start,
                value,
            } => {
                let bytes = match self.evaluate(transaction, value)? {
                    Value::Bytes(bytes) => bytes,
                    // Text is accepted because a file is very often text, and
                    // making a caller write `0x…` for a document would be a
                    // ceremony with no property behind it. Nothing else is: a
                    // file is bytes, and guessing at an encoding for a number or
                    // an object is a decision this store has no business taking.
                    Value::String(text) => text.into_bytes(),
                    other => {
                        return Err(Error::FileIsNotBytes {
                            found: other.type_name(),
                            span,
                        });
                    }
                };
                self.put_file(transaction, target, *start, &bytes)
            }
            StatementKind::Read {
                target,
                start,
                limit,
            } => self.read_file(transaction, target, *start, *limit),
            StatementKind::Backup { from } => self.backup(*from),
            StatementKind::Get { target } => {
                Ok(Outcome::Value(self.read_key(transaction, target)?))
            }
            StatementKind::Select(select) => {
                let (records, path) = self.read(transaction, select)?;
                Ok(Outcome::Records { records, path })
            }
            StatementKind::Explain(select) => self.explain(transaction, select),
            StatementKind::Info { subject } => self.info(transaction, subject, span),
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
        // An edge is an ordinary record, so an edge table's declarations apply
        // to it — including their defaults.
        let payload = self.with_defaults(transaction, edge_table, Value::Object(fields))?;
        transaction.put(address, encode_payload(&payload).into_bytes());
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
        fields: &[FieldPath],
        shape: IndexShape,
        if_not_exists: bool,
    ) -> Result<Outcome> {
        let (_, id) = self.resolve_table(transaction, table)?;
        if if_not_exists && self.index_named(transaction, id, name).is_ok() {
            return Ok(Outcome::Done);
        }
        let fields = fields.iter().map(|field| field.path.clone()).collect();
        Catalog::new(transaction).create_index(id, &name.text, fields, shape)?;
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
        shape: FieldShape,
        if_not_exists: bool,
    ) -> Result<Outcome> {
        let (_, id) = self.resolve_table(transaction, table)?;
        if if_not_exists && self.field_named(transaction, id, name).is_ok() {
            return Ok(Outcome::Done);
        }
        // The default is stored as the text it was written as, so it is read
        // back by parsing rather than by decoding a syntax tree — and a
        // definition stays legible in a dump.
        //
        // It is also **evaluated once, here**, and checked against the kind the
        // field declares. That is the same symmetry the rest of this wave
        // follows: a declaration is checked when it is made rather than when it
        // first bites. Without it, `DEFAULT 'open'` on a `TYPE int` field, or a
        // default naming something that does not exist, would be accepted and
        // would then fail on the first write — by which time the declaration is
        // in the catalog and the failure looks like the write's fault.
        if let Some(written) = &shape.default {
            let expression = bgv_db_ql::parse_expression(written)?;
            let value = self.evaluate(transaction, &expression)?;
            if !kind.accepts(&value) {
                return Err(Error::DefaultDoesNotMatch {
                    field: name.text.clone(),
                    declared: kind.name(),
                    found: value.type_name(),
                    span: name.span,
                });
            }
        }
        Catalog::new(transaction).create_field(id, &name.text, kind, shape)?;
        Ok(Outcome::Done)
    }

    fn define_analyzer(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        filters: &[Filter],
        if_not_exists: bool,
    ) -> Result<Outcome> {
        let declared = Catalog::new(transaction)
            .analyzers()?
            .into_iter()
            .any(|found| found.name == name.text);
        if if_not_exists && declared {
            return Ok(Outcome::Done);
        }
        Catalog::new(transaction).create_analyzer(&name.text, Analyzer::new(filters.to_vec()))?;
        Ok(Outcome::Done)
    }

    /// `DEFINE NODE ROLES … ENDPOINTS …` — the local half of the configuration.
    ///
    /// # It does not run in the transaction, and that is not an oversight
    ///
    /// The identity lives in the `META` keyspace, which is not the log
    /// (ADR-0018 §1). A `META` write therefore cannot be part of a log
    /// transaction, and taking `transaction` here to look symmetrical with
    /// `DEFINE REPLICA` would be a durability claim the substrate does not
    /// support — a `CANCEL` afterwards would leave the roles changed while the
    /// caller believed otherwise. `BACKUP` is store-scoped for the same reason
    /// and is spelled the same way.
    ///
    /// An unknown role is refused here rather than in the grammar, for the
    /// reason a vector distance is: which roles exist is the store's question,
    /// and this is where the store knows what it knows.
    fn define_node(&self, roles: Option<&[Name]>, endpoints: Option<&[String]>) -> Result<Outcome> {
        let named = roles
            .map(|named| {
                named.iter().try_fold(Roles::NONE, |carried, role| {
                    Roles::parse(&role.text)
                        .map(|found| carried.and(found))
                        .ok_or(Error::Unknown {
                            entity: "role",
                            name: role.text.clone(),
                            span: role.span,
                        })
                })
            })
            .transpose()?;
        self.store
            .configure_node(named, endpoints.map(<[String]>::to_vec))?;
        Ok(Outcome::Done)
    }

    /// `DEFINE REPLICA second AT '…'` — the replicated half.
    ///
    /// This one **does** run in the transaction, because a peer is a catalog
    /// record: it commits with whatever else the script did and reaches every
    /// node through the ordinary apply path (ADR-0009).
    fn define_replica(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        endpoint: &str,
        if_not_exists: bool,
    ) -> Result<Outcome> {
        let declared = Catalog::new(transaction)
            .replicas()?
            .into_iter()
            .any(|found| found.name == name.text);
        if if_not_exists && declared {
            return Ok(Outcome::Done);
        }
        Catalog::new(transaction).create_replica(&name.text, endpoint)?;
        Ok(Outcome::Done)
    }

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
    fn with_defaults(
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
            let expression = bgv_db_ql::parse_expression(&written)?;
            let value = self.evaluate(transaction, &expression)?;
            fields.insert(field.name, value);
        }
        Ok(Value::Object(fields))
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

    /// The definition of the index this table calls `name`.
    ///
    /// The definition rather than the id, because a caller that only wants the
    /// id can take it — and the one caller that wants the whole thing would
    /// otherwise have to look it up twice and handle a second absence that
    /// cannot happen.
    fn index_named(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        name: &Name,
    ) -> Result<IndexDefinition> {
        Catalog::new(transaction)
            .indexes_on(table)?
            .into_iter()
            .find(|index| index.name == name.text)
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

impl Session<'_> {
    /// The record a field-level `UPDATE` produces.
    ///
    /// # Every right-hand side sees the record as it was
    ///
    /// All of them are evaluated **before** any of them is applied, so
    /// `SET a = b, b = a` swaps rather than assigning `b` to both. It is SQL's
    /// rule and it is the only one that fits in a sentence; a left-to-right rule
    /// would make the meaning of a statement depend on the order somebody
    /// happened to type its clauses in.
    ///
    /// # Assigning `none` removes the field
    ///
    /// `Value::None` means the field is not there, so writing it into the object
    /// would say the field is there and holds not-being-there — the contradiction
    /// the value system spends its own rules avoiding, and the one a projection
    /// already refuses to produce.
    ///
    /// # A missing intermediate is refused, never created
    ///
    /// `SET a.b.c = 1` on a record with no `a` is an error naming the route.
    /// Creating the objects would be the store writing structure nobody asked
    /// for — the same call this store makes about zero-filling a hole in a file.
    fn edited(
        &self,
        transaction: &mut Transaction<'_>,
        existing: &[u8],
        assignments: &[Assignment],
        span: Span,
    ) -> Result<Value> {
        let mut record = decode_payload(existing)?;
        let mut wanted = Vec::with_capacity(assignments.len());
        for assignment in assignments {
            wanted.push(self.evaluate_in(
                transaction,
                &assignment.value,
                crate::evaluate::Scope::of(&record),
            )?);
        }

        if !matches!(record, Value::Object(_)) {
            // A record that is not an object has no named fields to change. The
            // key-value model stores single values that way (ADR-0010), and
            // `SET` is the verb for those.
            return Err(Error::NoSuchRouteToAssign {
                route: assignments
                    .first()
                    .map_or_else(String::new, |first| first.route.path.to_string()),
                span,
            });
        }
        for (assignment, held) in assignments.iter().zip(wanted) {
            let route = &assignment.route.path;
            let steps = route.steps();
            let Some((last, above)) = steps.split_last() else {
                // A bare field name: the root of the route is the field.
                set_field(&mut record, route.root(), held, &assignment.route, span)?;
                continue;
            };
            let Step::Field(name) = last else {
                // A position or `[*]`: assigning into an array by index is its
                // own question and `[*]` has three contexts, none of them this.
                return Err(Error::NoSuchRouteToAssign {
                    route: route.to_string(),
                    span: assignment.route.span,
                });
            };
            let parent = Path::new(route.root().to_owned(), above.to_vec());
            let Some(target) = parent.resolve_mut(&mut record) else {
                return Err(Error::NoSuchRouteToAssign {
                    route: parent.to_string(),
                    span: assignment.route.span,
                });
            };
            set_field(target, name, held, &assignment.route, span)?;
        }
        Ok(record)
    }
}

/// Put a value into one field of an object, or take it out.
fn set_field(
    holder: &mut Value,
    name: &str,
    held: Value,
    route: &FieldPath,
    span: Span,
) -> Result<()> {
    let Value::Object(fields) = holder else {
        return Err(Error::NoSuchRouteToAssign {
            route: route.path.to_string(),
            span,
        });
    };
    if held.is_present() {
        fields.insert(name.to_owned(), held);
    } else {
        fields.remove(name);
    }
    Ok(())
}
