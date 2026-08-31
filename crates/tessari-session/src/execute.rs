//! Running one statement against the store.

use std::collections::BTreeMap;
use tessari_encoding::{Roles, decode_payload, encode_payload};
use tessari_ql::{
    Answer, Assignment, ColumnDeclaration, ConsumerSource, CreateTarget, EdgeClause, Edit,
    FieldMapping, FieldPath, Name, RecordTarget, Span, StatementKind, TableChange, TableRef,
};
use tessari_storage::{
    Catalog, ConsumerDefinition, EDGE_IN, EDGE_OUT, EdgeDeclaration, EdgeOrder, FieldShape,
    IndexDefinition, IndexShape, Mapped, OnFailure, RecordAddress, TableKind, TableShape,
    Transaction, VectorDistance,
};

use tessari_types::{
    Analyzer, FieldId, FieldKind, Filter, GraphId, IdentityKind, Path, RecordId, RecordRef, Step,
    TableId, Value,
};

use crate::context::Context;
use crate::error::{Depended, Error, Result};
use crate::evaluate::{key_bound, within};
use crate::generate;
use crate::geometry::on_the_grid;
use crate::outcome::Outcome;
use crate::session::Session;

/// The one message format this store reads.
///
/// Named rather than written twice, because the refusal below and the reader
/// that acts on it have to mean the same word.
const FORMAT_JSON: &str = "json";

/// A `DEFINE CONSUMER` statement's parts, carried together.
///
/// Nine fields is more than a function signature should take, and the grouping
/// is not only clippy's preference: passing them as one borrow means a field
/// added to the statement cannot be silently dropped on the way to the catalog,
/// which is exactly the failure a long positional argument list invites.
struct Declared<'a> {
    name: &'a Name,
    source: &'a ConsumerSource,
    group: &'a str,
    format: &'a Name,
    identity: &'a FieldPath,
    mapping: &'a [FieldMapping],
    destination: &'a TableRef,
    on_failure: tessari_ql::OnFailure,
    parallelism: Option<u32>,
}

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
                columns,
                schemafull,
                edge,
                identity,
                graph,
                if_not_exists,
            } => {
                // The endpoints are resolved **before** the table is created, so
                // a declaration naming a table that is not there refuses having
                // written nothing. An edge table whose endpoint is a dangling
                // name could never refuse a `RELATE` against it, which is the
                // whole capability the clause was added for.
                let kind = self.edge_kind(transaction, edge.as_ref(), columns)?;
                // Resolved before the table is created for the same reason, and
                // it is the same failure: a table left standing with a
                // membership nothing resolves belongs to no graph anyone can
                // name, and `INFO FOR GRAPH` would never list it.
                let graph = self.resolve_graph(transaction, graph.as_ref())?;
                self.define_table_with_columns(
                    transaction,
                    name,
                    columns,
                    TableShape {
                        schemafull: *schemafull,
                        kind,
                        identity: *identity,
                        graph,
                    },
                    *if_not_exists,
                    span,
                )
            }
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
                kind.clone(),
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
                roles,
                if_not_exists,
            } => self.define_replica(
                transaction,
                name,
                endpoint,
                roles.as_deref(),
                *if_not_exists,
            ),
            StatementKind::DefineConsumer {
                name,
                source,
                group,
                format,
                identity,
                mapping,
                destination,
                on_failure,
                parallelism,
                if_not_exists,
            } => self.define_consumer(
                transaction,
                &Declared {
                    name,
                    source,
                    group,
                    format,
                    identity,
                    mapping,
                    destination,
                    on_failure: *on_failure,
                    parallelism: *parallelism,
                },
                *if_not_exists,
            ),
            StatementKind::DropConsumer { name } => self.drop_consumer(transaction, name, span),
            StatementKind::AlterUser { name, change } => {
                self.alter_user(transaction, name, change, span)
            }
            StatementKind::DropUser { name } => self.drop_user(transaction, name, span),
            StatementKind::Grant {
                verbs,
                table,
                fields,
                user,
            } => self.grant(transaction, verbs, table, fields, user, span),
            StatementKind::Revoke { verbs, table, user } => {
                self.revoke(transaction, verbs, table, user, span)
            }
            StatementKind::GrantAuthority { kinds, reach, user } => {
                self.grant_authority(transaction, kinds, reach, user, span)
            }
            StatementKind::RevokeAuthority { kinds, reach, user } => {
                self.revoke_authority(transaction, kinds, reach, user, span)
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
                spatial,
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
                    spatial: *spatial,
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
            } => self.relate(transaction, from, edges, to, value.as_deref()),
            StatementKind::DropTable { table } => {
                let (context, id) = self.resolve_table(transaction, table)?;
                // A bucket's bytes live in a companion table `DEFINE BUCKET`
                // created alongside it, and whose name carries a byte no
                // identifier can hold — so nothing can drop it by naming it, and
                // dropping the bucket alone orphans it forever. The corpus found
                // this by redefining a bucket it had just dropped and being told
                // the chunk table's name was taken.
                let chunks = Catalog::new(transaction)
                    .table(id)?
                    .filter(|definition| definition.is_bucket())
                    .map(|definition| Catalog::chunks_named(&definition.name));
                if let Some(name) = chunks
                    && let Some(chunk_id) = Catalog::new(transaction).table_id(
                        context.namespace,
                        context.database,
                        &name,
                    )?
                {
                    Catalog::new(transaction).drop_table(chunk_id)?;
                }
                Catalog::new(transaction).drop_table(id)?;
                Ok(Outcome::Done)
            }
            StatementKind::DropIndex { name, table } => {
                let (_, id) = self.resolve_table(transaction, table)?;
                let index = self.index_named(transaction, id, name)?;
                Catalog::new(transaction).drop_index(index.id)?;
                Ok(Outcome::Done)
            }
            StatementKind::DropAnalyzer { name } => self.drop_analyzer(transaction, name, span),
            StatementKind::DropReplica { name } => self.drop_replica(transaction, name, span),
            StatementKind::DropDatabase { name } => self.drop_database(transaction, name, span),
            StatementKind::DropNamespace { name } => self.drop_namespace(transaction, name, span),
            StatementKind::DefineGraph {
                name,
                if_not_exists,
            } => self.define_graph(transaction, name, *if_not_exists, span),
            StatementKind::DropGraph { name } => self.drop_graph(transaction, name, span),
            // Drop and declare in ONE transaction, which is what makes this
            // more than sugar: the catalog change and the rows ride the same log
            // record, so the store's schema pass holds every stored row to the
            // NEW declaration and refuses the alteration outright when one does
            // not fit — writing neither the removal nor the replacement.
            StatementKind::AlterField {
                name,
                table,
                kind,
                required,
                default,
                analyzer,
                assert,
            } => {
                let (_, id) = self.resolve_table(transaction, table)?;
                let field = self.field_named(transaction, id, name)?;
                Catalog::new(transaction).drop_field(field)?;
                self.define_field(
                    transaction,
                    name,
                    table,
                    kind.clone(),
                    FieldShape {
                        required: *required,
                        default: default.as_ref().map(|written| written.text.clone()),
                        analyzer: analyzer.as_ref().map(|named| named.text.clone()),
                        assert: assert.clone(),
                    },
                    false,
                )
            }
            StatementKind::AlterTable { table, change } => {
                let (_, id) = self.resolve_table(transaction, table)?;
                Catalog::new(transaction)
                    .set_schemafull(id, matches!(change, TableChange::Schemafull))?;
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
            StatementKind::Create {
                target: CreateTarget::Named(target),
                value,
                answer,
            } => {
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
                self.put_record(transaction, address, payload.clone(), span)?;
                Ok(answered(*answer, Value::None, payload))
            }
            StatementKind::Create {
                target: CreateTarget::Generated(table),
                value,
                answer,
            } => self.create_named_by_the_store(transaction, table, value, *answer, span),
            StatementKind::Insert {
                table,
                columns,
                rows,
            } => self.insert(transaction, table, columns, rows, span),
            StatementKind::Update {
                target,
                edit,
                answer,
            } => {
                let (_, address) = self.writable(transaction, target)?;
                let Some(existing) = transaction.get(&address)? else {
                    return Err(Error::NoSuchRecord {
                        id: address.id.to_string(),
                        span: target.span,
                    });
                };
                let before = decode_payload(&existing)?;
                let payload = self.applied(transaction, edit, before.clone(), target.span)?;
                // One rule rather than two: the result of either shape is a
                // record being written, so `REQUIRED` + `DEFAULT` keeps meaning
                // "this field always holds a value" even when a caller sets one
                // to `none`.
                let payload = self.with_defaults(transaction, address.table, payload)?;
                self.put_record(transaction, address, payload.clone(), span)?;
                Ok(answered(*answer, before, payload))
            }
            // Neither `CREATE`'s "it must be absent" nor `UPDATE`'s "it must be
            // present". A record that is not there starts as an empty object, so
            // the three edit shapes need no case of their own: `= { … }` writes
            // the value, and `SET` and `MERGE` fold into nothing and produce
            // exactly what they name.
            // Never answers: it fails, and the failure discards the work above
            // it in the transaction. That is what makes it a guard rather than a
            // log line.
            StatementKind::Throw { value } => {
                let message = match self.evaluate(transaction, value)? {
                    // A string is used as written, so `THROW 'already paid'`
                    // reads back exactly as it was typed rather than quoted.
                    Value::String(text) => text,
                    other => other.to_string(),
                };
                Err(Error::Thrown { message, span })
            }
            StatementKind::Upsert {
                target,
                edit,
                answer,
            } => {
                let (_, address) = self.writable(transaction, target)?;
                let held = transaction.get(&address)?;
                // `BEFORE` over a record that was not there answers `NONE`. That
                // is the true answer to the question the caller asked, and it is
                // exactly what distinguishes an upsert that created from one
                // that replaced — which is the reason to ask.
                let before = match &held {
                    Some(held) => decode_payload(held)?,
                    None => Value::None,
                };
                let existing = match held {
                    Some(held) => decode_payload(&held)?,
                    None => Value::Object(std::collections::BTreeMap::new()),
                };
                let payload = self.applied(transaction, edit, existing, target.span)?;
                let payload = self.with_defaults(transaction, address.table, payload)?;
                self.put_record(transaction, address, payload.clone(), span)?;
                Ok(answered(*answer, before, payload))
            }
            // A key-value write replaces whatever was there, which is why it is
            // a different verb from `CREATE` rather than the same one.
            StatementKind::Set { target, value } => {
                let (_, address) = self.writable(transaction, target)?;
                let payload = self.evaluate(transaction, value)?;
                self.put_record(transaction, address, payload, span)?;
                Ok(Outcome::Done)
            }
            StatementKind::Delete { target, answer } => {
                // A file's bytes go with its metadata, in this commit. A bucket
                // that kept chunks nothing describes would leak space nothing
                // could ever find its way back to.
                self.clear_file(transaction, target)?;
                let (_, address) = self.address(transaction, target)?;
                let before = match transaction.get(&address)? {
                    Some(held) => decode_payload(&held)?,
                    None => Value::None,
                };
                transaction.delete(address);
                Ok(answered(*answer, before, Value::None))
            }
            StatementKind::Del { target } => {
                self.clear_file(transaction, target)?;
                let (_, address) = self.address(transaction, target)?;
                transaction.delete(address);
                Ok(Outcome::Done)
            }
            StatementKind::DeleteWhere {
                table,
                condition,
                limit,
            } => self.delete_where(transaction, table, condition, *limit),
            StatementKind::DefineBucket {
                name,
                if_not_exists,
            } => self.define_table(
                transaction,
                name,
                TableShape {
                    schemafull: false,
                    kind: TableKind::Bucket,
                    identity: IdentityKind::default(),
                    graph: None,
                },
                *if_not_exists,
                span,
            ),
            // A collection is lenient because that is what the word means, not
            // because a flag was left off: it declares no fields, so there is
            // nothing for strictness to constrain. The `collection` flag is what
            // keeps it distinguishable from a table that was told to be lenient.
            StatementKind::DefineCollection {
                name,
                identity,
                if_not_exists,
            } => self.define_table(
                transaction,
                name,
                TableShape {
                    schemafull: false,
                    kind: TableKind::Collection,
                    identity: *identity,
                    graph: None,
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
                // `None` twice: a statement is the outermost read there is, so
                // its own clause is the only ceiling in force, and nothing is
                // holding its records except the caller who asked for them —
                // which is the case the held ceiling exists to tell apart from a
                // read built into memory for somebody else (Q-209).
                let answered = self.read(transaction, select, None, None)?;
                Ok(Outcome::Records {
                    records: answered.records,
                    plan: answered.plan,
                    notes: answered.notes,
                    only: select.only.is_some(),
                })
            }
            StatementKind::Explain(select) => self.explain(transaction, select),
            StatementKind::Info { subject } => self.info(transaction, subject, span),
            StatementKind::Keys { space, range } => self.keys(transaction, space, range.as_ref()),
            // The transaction verbs and `USE` never reach here; the session
            // handles them, because they change what the next statement runs in
            // rather than touching the store.
            // Both evaluate an expression and answer with its value. What the
            // run loop does with that value is where they part: a `RETURN`'s
            // value is the script's answer and is handed to the caller, while a
            // `LET`'s is substituted into the statements below it and the
            // statement itself reports `Done`. Neither decision belongs here —
            // this layer runs one statement and knows nothing of the ones
            // around it.
            StatementKind::Let { value, .. } | StatementKind::Return { value } => {
                Ok(Outcome::Value(self.evaluate(transaction, value)?))
            }

            StatementKind::Begin
            | StatementKind::Commit
            | StatementKind::Cancel
            | StatementKind::Verify
            | StatementKind::Use { .. } => Ok(Outcome::Done),
        }
    }

    /// Write a record, after every shape in it has crossed the geometry boundary.
    ///
    /// Every record write in this crate goes through here, which is the point.
    /// The boundary is not a rule about a field that was declared `TYPE
    /// geometry` — validity is a property of the value, not of a declaration
    /// about it, and the store's default table declares nothing at all. So the
    /// check runs on whatever arrives, wherever it arrives.
    ///
    /// It cannot live in the storage layer's schema check instead. That check
    /// reads an already-encoded payload, so it has no way to snap: snapping is a
    /// transformation and the payload is downstream of it. It also returns early
    /// for a table whose schema constrains nothing, which is exactly the table
    /// most geometry will be written to.
    fn put_record(
        &self,
        transaction: &mut Transaction<'_>,
        address: RecordAddress,
        payload: Value,
        span: Span,
    ) -> Result<()> {
        let payload = on_the_grid(payload, span)?;
        transaction.put(address, encode_payload(&payload).into_bytes());
        Ok(())
    }

    /// Write one record the store names itself: `CREATE users = { … }`.
    ///
    /// # Why this is the path the documentation leads with
    ///
    /// Asking a caller to invent a name per record is asking for the collision
    /// they will eventually write, and it puts a decision in front of every
    /// example that the store is better placed to make. The addressed form
    /// stays for the caller who has a name already — an import, a migration, a
    /// foreign key — which is the case it was always right for.
    ///
    /// # What it answers with, and why the identity is the default answer
    ///
    /// Without a `RETURN` clause this answers the produced identity rather than
    /// [`Outcome::Done`]. `Done` would be the one honest thing it must not say:
    /// the caller did not choose the identity, cannot derive it, and has no
    /// second statement that would find the record again — so a write that
    /// reported only that it happened would be a write nothing can reach.
    /// `RETURN AFTER` still answers the record, because a caller who asked for
    /// the record asked for the record.
    ///
    /// The shape is [`Outcome::Keys`], which is what `INSERT` already answers
    /// with for the same reason. One shape for one idea, so the surface that
    /// renders it has one arm to add rather than two to keep in step — this
    /// project's own scar on that is `plan/reported.rs`.
    fn create_named_by_the_store(
        &self,
        transaction: &mut Transaction<'_>,
        table: &TableRef,
        value: &tessari_ql::Expr,
        answer: Answer,
        span: Span,
    ) -> Result<Outcome> {
        let (context, id) = self.resolve_table(transaction, table)?;
        // The same refusal `Session::writable` gives, reached directly for the
        // same reason `insert` reaches it directly: that one takes a record
        // target and this statement names no record.
        if Catalog::new(transaction)
            .table(id)?
            .is_some_and(|found| found.is_bucket())
        {
            return Err(Error::NotWrittenByHand {
                table: table.name.text.clone(),
                span: table.span,
            });
        }
        // Evaluated before the identity is allocated, so a value that refuses
        // does not spend a number. The counter is transactional and would roll
        // back anyway; not spending it keeps the sequence gapless for a reader,
        // and a gap in a sequence reads as a deletion.
        let payload = self.evaluate(transaction, value)?;
        let payload = self.with_defaults(transaction, id, payload)?;
        // Free by construction — `free_identity` does the read that establishes
        // it, so nothing here writes over a record that was already there.
        let identity = self.free_identity(transaction, &context, id, table.span)?;
        let address = RecordAddress::new(context.namespace, context.database, id, identity.clone());
        self.put_record(transaction, address, payload.clone(), span)?;
        Ok(match answer {
            Answer::After => Outcome::Value(payload),
            _ => Outcome::Keys(vec![identity]),
        })
    }

    /// An identity the table will give a record it is not given a name for, and
    /// which no record in it holds.
    ///
    /// # Why the scheme is the table's
    ///
    /// It is read from the declaration rather than decided here. A store that
    /// chose per statement would name records into one table under two schemes,
    /// and afterwards nothing could say which one a missing record had been
    /// written under. A table with no stored scheme reads [`IdentityKind::Int`],
    /// which is the default `DEFINE TABLE` writes and what the declaration would
    /// have said had the choice existed when that table was made.
    ///
    /// # Why the counter walks past an identity somebody named
    ///
    /// One table has **one** identity space, and the caller may write into it by
    /// hand: `CREATE users:1` and `CREATE users = { … }` address the same table.
    /// A counter that started at 1 against a table whose low identities were
    /// imported would collide, and — because a refusal discards the counter's
    /// advance along with the rest of the transaction — it would collide again
    /// on the next attempt, and every attempt after that. That is not a bad
    /// error message; it is a table that can never again be written to without
    /// naming the record. So the counter advances until it finds an identity
    /// nothing holds.
    ///
    /// **The cost is a read per identity walked past, and it is paid once.** The
    /// counter keeps its advance when the statement commits, so the skipping is
    /// amortised over the table's life rather than repeated. The shape that is
    /// genuinely slow is a table given millions of named identities from 1
    /// upwards and *then* asked to generate — one statement pays for all of
    /// them. `IDENTITY uuid` is the declaration for a table expecting that, and
    /// it needs no counter at all.
    ///
    /// A repeated **UUID** is not walked past. It cannot happen unless the
    /// machine's randomness is broken, and a store that quietly drew again would
    /// be hiding that rather than reporting it.
    fn free_identity(
        &self,
        transaction: &mut Transaction<'_>,
        context: &Context,
        table: TableId,
        span: Span,
    ) -> Result<RecordId> {
        let kind = Catalog::new(transaction)
            .table(table)?
            .map_or_else(IdentityKind::default, |found| found.identity);
        loop {
            let identity = match kind {
                IdentityKind::Uuid => RecordId::Uuid(generate::uuid_v7(span)?),
                IdentityKind::Int => {
                    let number = Catalog::new(transaction).next_record_number(table)?;
                    // The counter is a `u64` and a record identity is an `i64`,
                    // so the boundary is crossed with a check rather than an
                    // `as`. It cannot refuse — the counter declines to *store* a
                    // number past `i64::MAX`, so a number it answered is one
                    // this store can spend — and the branch is written anyway
                    // because a cast that wrapped here would hand out an
                    // identity that already names a record, silently.
                    let held = i64::try_from(number).map_err(|_| {
                        tessari_storage::Error::IdSpaceExhausted {
                            level: tessari_storage::RECORD_LEVEL,
                        }
                    })?;
                    RecordId::Int(held)
                }
            };
            let address =
                RecordAddress::new(context.namespace, context.database, table, identity.clone());
            if transaction.get(&address)?.is_none() {
                return Ok(identity);
            }
            if matches!(kind, IdentityKind::Uuid) {
                return Err(Error::RecordExists {
                    id: identity.to_string(),
                    span,
                });
            }
        }
    }

    /// Write a batch of records the store names itself.
    ///
    /// # One transaction, and why that needs no code here
    ///
    /// The rows are written in a loop against the transaction this statement was
    /// handed, and a row that refuses leaves through `?` — so `session::run`
    /// never reaches its `commit`, and the rows already written go with it. The
    /// batch is atomic because the transaction is, not because anything here
    /// arranges it. The test for it uses a **middle** row, because an
    /// implementation that committed as it went would still pass a batch whose
    /// only bad row is the last.
    ///
    /// # Why the produced identity is checked against the store
    ///
    /// [`Self::free_identity`] does the read that makes the identity free, and a
    /// read per row is what that costs. A store that skipped it and wrote over a
    /// record would lose it with nothing anywhere to notice — and the case is not
    /// hypothetical, because the caller may name identities in this same table
    /// by hand.
    ///
    /// The refusal tells the caller nothing they could have used: they did not
    /// choose the identity and cannot choose the next one, so this is not the
    /// existence oracle that keeps `CREATE` at `read` + `write`. `INSERT` needs
    /// `write` alone — see `identity::Needs::of`.
    fn insert(
        &self,
        transaction: &mut Transaction<'_>,
        table: &TableRef,
        columns: &[Name],
        rows: &[Vec<tessari_ql::Expr>],
        span: Span,
    ) -> Result<Outcome> {
        let (context, id) = self.resolve_table(transaction, table)?;
        // A bucket's records describe bytes the store holds, so one written by
        // hand can lie about them. The same refusal `Session::writable` gives,
        // for the same reason — reached here directly because that one takes a
        // record target and an insert names no record.
        if Catalog::new(transaction)
            .table(id)?
            .is_some_and(|found| found.is_bucket())
        {
            return Err(Error::NotWrittenByHand {
                table: table.name.text.clone(),
                span: table.span,
            });
        }

        let mut produced = Vec::with_capacity(rows.len());
        for row in rows {
            let mut fields = BTreeMap::new();
            for (column, value) in columns.iter().zip(row) {
                fields.insert(column.text.clone(), self.evaluate(transaction, value)?);
            }
            let payload = self.with_defaults(transaction, id, Value::Object(fields))?;

            let identity = self.free_identity(transaction, &context, id, table.span)?;
            let address =
                RecordAddress::new(context.namespace, context.database, id, identity.clone());
            self.put_record(transaction, address, payload, span)?;
            produced.push(identity);
        }
        Ok(Outcome::Keys(produced))
    }

    /// The kind a `DEFINE TABLE` produces, with any declared pair resolved.
    ///
    /// Resolution happens here rather than inside the table's own creation
    /// because an endpoint that does not exist has to refuse before anything is
    /// written: a table carrying a dangling endpoint id could refuse nothing,
    /// and the clause exists only to refuse.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unknown`] when an endpoint names no table, or when the
    /// order names a field the statement does not declare.
    /// The graph a `DEFINE TABLE … IN social` clause names, resolved to its id.
    ///
    /// The graph is looked up in the tenancy the statement is running in, which
    /// is where `DEFINE GRAPH` put it. A name that resolves to nothing refuses
    /// here, before the table exists, so a refusal leaves the store exactly as
    /// it found it.
    fn resolve_graph(
        &self,
        transaction: &mut Transaction<'_>,
        graph: Option<&Name>,
    ) -> Result<Option<GraphId>> {
        let Some(graph) = graph else {
            return Ok(None);
        };
        let context = self.context(transaction, None, graph.span)?;
        let id = Catalog::new(transaction)
            .graph_id(context.namespace, context.database, &graph.text)?
            .ok_or_else(|| Error::Unknown {
                entity: "graph",
                name: graph.text.clone(),
                span: graph.span,
            })?;
        Ok(Some(id))
    }

    fn edge_kind(
        &self,
        transaction: &mut Transaction<'_>,
        edge: Option<&EdgeClause>,
        columns: &[ColumnDeclaration],
    ) -> Result<TableKind> {
        let Some(edge) = edge else {
            return Ok(TableKind::Table);
        };
        let EdgeClause::Between(declared) = edge else {
            return Ok(TableKind::Edge(None));
        };
        let (_, from_id) = self.resolve_table(transaction, &declared.from)?;
        let (_, to_id) = self.resolve_table(transaction, &declared.to)?;
        // The ordering field has to be one the table declares. Nothing else can
        // guarantee an edge carries it, and the order is the endpoint index's
        // key suffix rather than a sort applied afterwards: an edge missing the
        // field has no place to be written, and the failure would surface much
        // later as neighbours arriving in roughly the right sequence.
        let order = match &declared.order {
            Some(ordering) => {
                if !columns
                    .iter()
                    .any(|column| column.name.text == ordering.field.text)
                {
                    return Err(Error::Unknown {
                        entity: "field",
                        name: ordering.field.text.clone(),
                        span: ordering.field.span,
                    });
                }
                Some(EdgeOrder {
                    field: ordering.field.text.clone(),
                    descending: ordering.descending,
                })
            }
            None => None,
        };
        Ok(TableKind::Edge(Some(EdgeDeclaration {
            from: from_id,
            to: to_id,
            order,
        })))
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
        value: Option<&tessari_ql::Expr>,
    ) -> Result<Outcome> {
        let (context, edge_table) = self.resolve_table(transaction, edges)?;
        let Some(definition) = Catalog::new(transaction).table(edge_table)? else {
            return Err(Error::NotAnEdgeTable {
                table: edges.name.text.clone(),
                span: edges.span,
            });
        };
        if !definition.is_edge() {
            return Err(Error::NotAnEdgeTable {
                table: edges.name.text.clone(),
                span: edges.span,
            });
        }
        let declared = definition.edge_endpoints().cloned();
        let (_, out) = self.address(transaction, from)?;
        let (_, into) = self.address(transaction, to)?;
        // A table that declared its pair refuses a link between any other, and
        // that refusal is the whole of what the clause buys. It is checked after
        // both endpoints resolve so that a link naming a record that is not
        // there fails as the missing record it is, rather than as a pair the
        // table does not join.
        if let Some(declared) = declared
            && (out.table != declared.from || into.table != declared.to)
        {
            return Err(Error::EndpointsNotDeclared {
                table: edges.name.text.clone(),
                span: edges.span,
            });
        }

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
        self.put_record(transaction, address, payload, edges.span)?;
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

    /// `DEFINE GRAPH social` — the structure node tables belong to.
    fn define_graph(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        if_not_exists: bool,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        if if_not_exists
            && Catalog::new(transaction)
                .graph_id(context.namespace, context.database, &name.text)?
                .is_some()
        {
            return Ok(Outcome::Done);
        }
        Catalog::new(transaction).create_graph(context.namespace, context.database, &name.text)?;
        Ok(Outcome::Done)
    }

    /// `DROP GRAPH social` — refused while a table still belongs to it.
    ///
    /// The same stance [`Self::drop_database`] takes one level away: the
    /// statement asks whether anything still depends, because it holds the span
    /// to refuse with. Dropping anyway would leave every member table pointing
    /// at an id nothing resolves, and the symptom would surface later as a walk
    /// that finds no graph rather than now as the drop that caused it.
    fn drop_graph(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        let id = Catalog::new(transaction)
            .graph_id(context.namespace, context.database, &name.text)?
            .ok_or_else(|| Error::Unknown {
                entity: "graph",
                name: name.text.clone(),
                span,
            })?;
        let members: Vec<_> = Catalog::new(transaction)
            .tables_in(context.namespace, context.database)?
            .into_iter()
            .filter(|table| table.graph == Some(id))
            .collect();
        if let Some(first) = members.first() {
            return Err(Error::StillDepended {
                depended: Depended::GraphByTable,
                name: name.text.clone(),
                count: members.len(),
                first: first.name.clone(),
                span,
            });
        }
        Catalog::new(transaction).drop_graph(id)?;
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
    /// `DEFINE TABLE t (a string, b int REQUIRED)` — the table, then its fields.
    ///
    /// **A desugaring, not a second implementation.** Each column goes through
    /// the same `define_field` the long spelling does, which is what makes a
    /// constraint declared here behave the way one declared there does — it is
    /// checked against the rows already in the table, and a violation refuses
    /// the whole statement. Reimplementing the field half would have produced
    /// the one thing this criterion is about: two spellings that agree on the
    /// happy path and disagree on the day the data does not fit.
    ///
    /// Fields are declared **after** the table exists and in written order, so
    /// a failure at the third column rolls back the first two and the table
    /// with them: the statement's transaction is the unit, and a half-declared
    /// table is not a state this can leave behind.
    ///
    /// `IF NOT EXISTS` covers the whole declaration rather than the table
    /// alone. The alternative makes the statement un-re-runnable — the table is
    /// tolerated and the first column then refuses — which is the opposite of
    /// what the words ask for.
    fn define_table_with_columns(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        columns: &[ColumnDeclaration],
        shape: TableShape,
        if_not_exists: bool,
        span: Span,
    ) -> Result<Outcome> {
        let outcome = self.define_table(transaction, name, shape, if_not_exists, span)?;
        if columns.is_empty() {
            return Ok(outcome);
        }
        let table = TableRef {
            database: None,
            name: name.clone(),
            span: name.span,
        };
        for column in columns {
            self.define_field(
                transaction,
                &column.name,
                &table,
                column.kind.clone(),
                FieldShape {
                    required: column.required,
                    default: column.default.as_ref().map(|written| written.text.clone()),
                    analyzer: column.analyzer.as_ref().map(|named| named.text.clone()),
                    assert: column.assert.clone(),
                },
                if_not_exists,
            )?;
        }
        Ok(outcome)
    }

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
            let expression = tessari_ql::parse_expression(written)?;
            let value = self.evaluate(transaction, &expression)?;
            if !kind.accepts(&value) {
                return Err(Error::DefaultDoesNotMatch {
                    field: name.text.clone(),
                    declared: kind.name().into_owned(),
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
        let named = roles.map(named_roles).transpose()?;
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
        roles: Option<&[Name]>,
        if_not_exists: bool,
    ) -> Result<Outcome> {
        let declared = Catalog::new(transaction)
            .replicas()?
            .into_iter()
            .any(|found| found.name == name.text);
        if if_not_exists && declared {
            return Ok(Outcome::Done);
        }
        // The words are read before the name is claimed, so a misspelled role
        // leaves nothing behind: the statement either declares the peer it was
        // asked for or declares nothing.
        let roles = roles.map(named_roles).transpose()?.unwrap_or(Roles::NONE);
        Catalog::new(transaction).create_replica(&name.text, endpoint, roles)?;
        Ok(Outcome::Done)
    }

    /// Declare a consumer, after checking everything it names actually exists.
    ///
    /// The order matters and is the same one `DEFINE REPLICA` uses for its
    /// roles: everything that can be refused is refused **before** the name is
    /// claimed, so a statement either declares the consumer it was asked for or
    /// declares nothing. Here that covers three things a mistyped statement gets
    /// wrong — an unknown format, a destination that does not exist, and a
    /// mapping that names the same record field twice.
    fn define_consumer(
        &self,
        transaction: &mut Transaction<'_>,
        declared: &Declared<'_>,
        if_not_exists: bool,
    ) -> Result<Outcome> {
        if if_not_exists
            && Catalog::new(transaction)
                .consumers()?
                .iter()
                .any(|found| found.name == declared.name.text)
        {
            return Ok(Outcome::Done);
        }

        // Refused where the store knows what it knows, with the span the author
        // can see — the rule a vector distance and a node role already follow.
        // There is one format today, and an unknown one is a consumer that would
        // start and then fail on its first message rather than at declaration.
        if declared.format.text != FORMAT_JSON {
            return Err(Error::Unknown {
                entity: "message format",
                name: declared.format.text.clone(),
                span: declared.format.span,
            });
        }

        // The destination is resolved rather than remembered, which is what
        // removes the race the two-object design cannot: a consumer whose
        // destination does not exist is refused here instead of starting and
        // discovering it later, with messages already read.
        let (context, destination) = self.resolve_table(transaction, declared.destination)?;

        let mut mapping = Vec::with_capacity(declared.mapping.len());
        for pair in declared.mapping {
            // Two message fields landing on one record field is a mapping whose
            // result depends on which one is applied last. Refused rather than
            // ordered, because there is no ordering that is not arbitrary.
            if mapping.iter().any(|held: &Mapped| held.to == pair.to.text) {
                return Err(Error::DuplicateMapping {
                    field: pair.to.text.clone(),
                    span: pair.to.span,
                });
            }
            mapping.push(Mapped {
                from: pair.from.path.to_string(),
                to: pair.to.text.clone(),
            });
        }

        let definition = ConsumerDefinition {
            // Replaced by the catalog when the record is written; the field
            // exists on the way in only because the definition is one type.
            id: 0,
            name: declared.name.text.clone(),
            brokers: declared.source.brokers.clone(),
            topic: declared.source.topic.clone(),
            group: declared.group.to_owned(),
            format: declared.format.text.clone(),
            identity: declared.identity.path.to_string(),
            mapping,
            namespace: context.namespace,
            database: context.database,
            destination,
            on_failure: match declared.on_failure {
                tessari_ql::OnFailure::Stop => OnFailure::Stop,
                tessari_ql::OnFailure::Quarantine => OnFailure::Quarantine,
            },
            // `None` reads as one, not as "decide for me". The parser has
            // already refused a zero, so this cannot be a consumer that runs
            // nothing.
            parallelism: declared.parallelism.unwrap_or(1),
            // Whose authority the writes will carry. `None` only on an open
            // store, where there is nobody to record and nothing to enforce —
            // the same condition under which the first user is declared.
            declarer: self.identity.user().map(|user| user.id),
        };
        // A name already taken is refused by the catalog itself, which is where
        // every other declaration's collision is decided.
        Catalog::new(transaction).create_consumer(&definition)?;
        Ok(Outcome::Done)
    }

    /// Forget a consumer.
    ///
    /// Removing the declaration is all this does here. Stopping whatever is
    /// running is the runner's job, and it learns of the change the same way a
    /// follower does — by reading the catalog — rather than by being called from
    /// inside a transaction that has not committed yet.
    fn drop_consumer(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let found = Catalog::new(transaction)
            .consumers()?
            .into_iter()
            .find(|held| held.name == name.text);
        let Some(consumer) = found else {
            return Err(Error::Unknown {
                entity: "consumer",
                name: name.text.clone(),
                span,
            });
        };
        Catalog::new(transaction).drop_consumer(&consumer)?;
        Ok(Outcome::Done)
    }

    /// `DROP ANALYZER simple` — refused while a field still names it.
    ///
    /// The reference is by **name** rather than by id
    /// (`FieldDefinition::analyzer`), so nothing in the catalog enforces it and
    /// nothing would notice it break. What a dangling reference produces is a
    /// search that quietly stops matching — a wrong answer indistinguishable
    /// from a right one, which is the shape this store refuses everywhere.
    ///
    /// Every field in the store is read, not every field on one table: an
    /// analyzer is declared once for the whole store, so no single table can
    /// answer whether it is still attached.
    fn drop_analyzer(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let found = Catalog::new(transaction)
            .analyzers()?
            .into_iter()
            .find(|held| held.name == name.text);
        let Some(analyzer) = found else {
            return Err(Error::Unknown {
                entity: "analyzer",
                name: name.text.clone(),
                span,
            });
        };
        let attached: Vec<String> = Catalog::new(transaction)
            .fields()?
            .into_iter()
            .filter(|field| field.analyzer.as_deref() == Some(name.text.as_str()))
            .map(|field| field.name)
            .collect();
        if let Some(first) = attached.first() {
            return Err(Error::StillDepended {
                depended: Depended::AnalyzerByField,
                name: name.text.clone(),
                count: attached.len(),
                first: first.clone(),
                span,
            });
        }
        Catalog::new(transaction).drop_analyzer(analyzer.id)?;
        Ok(Outcome::Done)
    }

    /// `DROP REPLICA warsaw` — stops counting an endpoint as a peer.
    ///
    /// Nothing depends on a peer the way a field depends on an analyzer, so
    /// there is no refusal here: a replica declaration is a statement about who
    /// we send to, and withdrawing it is complete on its own.
    fn drop_replica(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let found = Catalog::new(transaction)
            .replicas()?
            .into_iter()
            .find(|held| held.name == name.text);
        let Some(replica) = found else {
            return Err(Error::Unknown {
                entity: "replica",
                name: name.text.clone(),
                span,
            });
        };
        Catalog::new(transaction).drop_replica(replica.id)?;
        Ok(Outcome::Done)
    }

    /// `DROP DATABASE staging` — refused while it still holds a table.
    ///
    /// The bound is the one `DELETE … LIMIT` established: a destructive
    /// statement carrying no predicate at all is the widest thing this language
    /// can be asked to run, and the person writing it is thinking about one
    /// name. The refusal counts and names, so acting on it needs no second
    /// query.
    fn drop_database(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, Some(name.text.as_str()), span)?;
        let held = Catalog::new(transaction).tables_in(context.namespace, context.database)?;
        if let Some(first) = held.first() {
            return Err(Error::StillDepended {
                depended: Depended::DatabaseByTable,
                name: name.text.clone(),
                count: held.len(),
                first: first.name.clone(),
                span,
            });
        }
        Catalog::new(transaction).drop_database(context.database)?;
        Ok(Outcome::Done)
    }

    /// `DROP NAMESPACE acme` — refused while it still holds a database.
    ///
    /// One level up from [`Self::drop_database`] and refusing on the same
    /// ground. Resolved by name against the catalog rather than through the
    /// session's tenancy, because a namespace is what a tenancy is selected
    /// *within* — asking the context for it would require having selected it.
    fn drop_namespace(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let id = Catalog::new(transaction)
            .namespace_id(&name.text)?
            .ok_or_else(|| Error::Unknown {
                entity: "namespace",
                name: name.text.clone(),
                span,
            })?;
        let held = Catalog::new(transaction).databases_in(id)?;
        if let Some(first) = held.first() {
            return Err(Error::StillDepended {
                depended: Depended::NamespaceByDatabase,
                name: name.text.clone(),
                count: held.len(),
                first: first.name.clone(),
                span,
            });
        }
        Catalog::new(transaction).drop_namespace(id)?;
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
            let expression = tessari_ql::parse_expression(&written)?;
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
    /// same either way; the cost is not, and `docs/tessariql.md` §6 says so rather
    /// than implying a seek this milestone does not perform.
    fn keys(
        &self,
        transaction: &mut Transaction<'_>,
        space: &TableRef,
        range: Option<&tessari_ql::RangeExpr>,
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
    /// The record an edit produces, whichever of the three shapes it is.
    ///
    /// Shared by `UPDATE` and `UPSERT` so the two cannot drift: the only thing
    /// that separates them is what they assert about the record beforehand, and
    /// a second copy of this match is how that stops being true.
    fn applied(
        &self,
        transaction: &mut Transaction<'_>,
        edit: &Edit,
        existing: Value,
        span: Span,
    ) -> Result<Value> {
        match edit {
            // Replacing the whole record is a write like a create, so the
            // defaults apply to it the same way.
            Edit::Whole(value) => self.evaluate(transaction, value),
            Edit::Fields(assignments) => self.edited(transaction, existing, assignments, span),
            Edit::Merge(value) => {
                // The value position, like every other object literal — see
                // `Edit::Merge`. Computing from the record is `SET`'s job.
                let incoming = self.evaluate(transaction, value)?;
                let Value::Object(_) = incoming else {
                    return Err(Error::MergeIsNotAnObject {
                        found: incoming.type_name(),
                        span,
                    });
                };
                Ok(merged(existing, incoming))
            }
        }
    }

    fn edited(
        &self,
        transaction: &mut Transaction<'_>,
        existing: Value,
        assignments: &[Assignment],
        span: Span,
    ) -> Result<Value> {
        let mut record = existing;
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

/// The roles a list of words names, folded into one set.
///
/// Shared by `DEFINE NODE` and `DEFINE REPLICA` because they name the same field
/// on the same membership row (ADR-0018 §2) from the two sides — this node, and
/// a peer. A second copy would not fail to compile if it drifted; it would
/// change which words a peer may be declared with, and only on the side nobody
/// was looking at.
///
/// An unrecognised word is refused rather than skipped: a role this build has no
/// name for is one the operator believes they set.
fn named_roles(named: &[Name]) -> Result<Roles> {
    named.iter().try_fold(Roles::NONE, |carried, role| {
        Roles::parse(&role.text)
            .map(|found| carried.and(found))
            .ok_or(Error::Unknown {
                entity: "role",
                name: role.text.clone(),
                span: role.span,
            })
    })
}

/// The outcome a write reports, given what it was asked to answer with.
///
/// One place rather than four, so that the four writes cannot come to disagree
/// about what `AFTER` means. `Nothing` is the default and stays `Done`: a write
/// that answered with a record by default would make every caller pay to ship
/// back a value most of them already have.
fn answered(answer: Answer, before: Value, after: Value) -> Outcome {
    match answer {
        Answer::Nothing => Outcome::Done,
        Answer::Before => Outcome::Value(before),
        Answer::After => Outcome::Value(after),
    }
}

/// Two records folded into one: `incoming` over `existing`.
///
/// Deep where **both** sides hold an object and total everywhere else. That rule
/// is the whole of it, and the shapes it settles are worth naming:
///
/// - object over object — merged, one level deeper;
/// - anything over anything else — the incoming value, whole. An array replaces
///   an array rather than concatenating or merging by position, because there is
///   no reading of "merge these two lists" that is right more often than it is
///   surprising;
/// - a field the incoming object does not name — left exactly as it was, which
///   is the point of the verb;
/// - an explicit `NULL` — written, because `NULL` is a value here and means
///   "known to be nothing". Removing a field is `SET route = NONE`, which says
///   removal out loud rather than hiding it inside a merge.
fn merged(existing: Value, incoming: Value) -> Value {
    match (existing, incoming) {
        (Value::Object(mut into), Value::Object(from)) => {
            for (name, value) in from {
                let folded = match (into.remove(&name), value) {
                    (Some(held @ Value::Object(_)), value @ Value::Object(_)) => {
                        merged(held, value)
                    }
                    (_, value) => value,
                };
                into.insert(name, folded);
            }
            Value::Object(into)
        }
        // One side is not an object, so there is nothing to fold into: the
        // incoming value stands whole. The top level never reaches here — the
        // caller refuses a non-object there — but a route below it does, and
        // that is the "incoming wins" rule doing its job.
        (_, incoming) => incoming,
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
