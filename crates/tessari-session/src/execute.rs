//! Running one statement against the store.

use std::collections::BTreeMap;
use tessari_encoding::{NODE_ID_LEN, Roles, decode_payload, encode_payload};
use tessari_ql::{
    Answer, Assignment, ColumnDeclaration, ConsumerSource, CreateTarget, EdgeClause, Edit,
    FieldMapping, FieldPath, Name, ReachRef, RecordTarget, Span, StatementKind, TableChange,
    TableRef,
};
use tessari_storage::{
    Catalog, ConsumerDefinition, EDGE_IN, EDGE_OUT, EdgeDeclaration, EdgeOrder, FieldShape,
    GEO_FIELD, IndexDefinition, IndexShape, Mapped, OnFailure, QueueDeclaration, RecordAddress,
    SeriesDeclaration, TableDefinition, TableKind, TableShape, Transaction, VECTOR_FIELD,
    VaultDeclaration, VectorDeclaration, VectorDistance, ViewDeclaration, Violation, violations,
};

use tessari_types::{
    Analyzer, FieldId, FieldKind, Filter, GraphId, IdentityKind, Path, RecordId, RecordRef,
    Replication, Step, TableId, Value,
};

use crate::condition::boolean;
use crate::context::Context;
use crate::error::{Depended, Error, Result};
use crate::evaluate::{Scope, key_bound, within};
use crate::generate;
use crate::geometry::on_the_grid;
use crate::outcome::Outcome;
use crate::session::Session;

/// The one message format this store reads.
///
/// Named rather than written twice, because the refusal below and the reader
/// that acts on it have to mean the same word.
const FORMAT_JSON: &str = "json";

/// A `DEFINE KAFKA CONSUMER` statement's parts, carried together.
///
/// Nine fields is more than a function signature should take, and the grouping
/// is not only clippy's preference: passing them as one borrow means a field
/// added to the statement cannot be silently dropped on the way to the catalog,
/// which is exactly the failure a long positional argument list invites.
/// What a `DEFINE REPLICA` says about a peer.
///
/// A struct for the reason [`Declared`] is one: the statement's clauses outgrew
/// what a function signature carries legibly, and grouping them keeps the caller
/// reading as the statement it is rather than as eight positional arguments in
/// an order nothing checks.
struct Peer<'a> {
    name: &'a Name,
    endpoint: &'a str,
    roles: Option<&'a [Name]>,
    node: Option<[u8; NODE_ID_LEN]>,
    replicates: Option<&'a ReachRef>,
}

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
                replication,
            } => self.define_namespace(transaction, name, *if_not_exists, *replication),
            StatementKind::AlterNamespace { name, replication } => {
                self.alter_namespace(transaction, name, *replication)
            }
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
                secret,
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
                    secret: *secret,
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
                node,
                replicates,
                if_not_exists,
            } => self.define_replica(
                transaction,
                &Peer {
                    name,
                    endpoint,
                    roles: roles.as_deref(),
                    node: *node,
                    replicates: replicates.as_ref(),
                },
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
            StatementKind::DeleteEdge {
                from,
                edges,
                to,
                answer,
            } => self.delete_edge(transaction, from, edges, to, *answer),
            StatementKind::DropTable { table } => {
                let (context, id) = self.resolve_table(transaction, table)?;
                // A bucket's bytes live in a companion table `DEFINE BUCKET`
                // created alongside it, and whose name carries a byte no
                // identifier can hold — so nothing can drop it by naming it, and
                // dropping the bucket alone orphans it forever. The corpus found
                // this by redefining a bucket it had just dropped and being told
                // the chunk table's name was taken.
                // A graph's own node collection is the same shape as the chunk
                // table below — a table the caller never declared, carrying a
                // name the caller did not choose — except that this one IS
                // nameable, so it is refused by name rather than protected by
                // an unspellable one. `DROP GRAPH` is the statement that removes
                // it; see `Error::TableBelongsToGraph`.
                if let Some(definition) = Catalog::new(transaction).table(id)?
                    && let Some(graph) = definition.graph
                    && let Some(graph) = Catalog::new(transaction).graph(graph)?
                    && graph.name == definition.name
                {
                    return Err(Error::TableBelongsToGraph {
                        table: definition.name,
                        graph: graph.name,
                        span,
                    });
                }
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
            StatementKind::DefineEdge {
                name,
                graph,
                from,
                to,
                if_not_exists,
            } => self.define_edge(transaction, name, graph, from, to, *if_not_exists, span),
            StatementKind::DropEdge { name } => self.drop_edge(transaction, name, span),
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
                        secret: false,
                        default: default.as_ref().map(|written| written.text.clone()),
                        analyzer: analyzer.as_ref().map(|named| named.text.clone()),
                        assert: assert.clone(),
                    },
                    false,
                )
            }
            StatementKind::AlterTable { table, change } => {
                let (_, id) = self.resolve_table(transaction, table)?;
                // A vault is declared strict and cannot be talked out of it.
                // Without this the protection above is one statement deep: a
                // caller who may alter the table turns the vault schemaless and
                // every field written afterwards is stored in the clear, with no
                // refusal anywhere and the vault still reporting as a vault.
                if matches!(change, TableChange::Schemaless)
                    && Catalog::new(transaction)
                        .table(id)?
                        .is_some_and(|definition| definition.is_vault())
                {
                    return Err(Error::VaultIsStrict {
                        table: table.name.text.clone(),
                        span: table.span,
                    });
                }
                Catalog::new(transaction)
                    .set_schemafull(id, matches!(change, TableChange::Schemafull))?;
                Ok(Outcome::Done)
            }
            // The answer is an array and never a refusal, because an operator
            // deciding whether to fix the data or the declaration needs all of
            // it. A statement that raised on the first disagreement would hand
            // them the same table one record at a time — which is the shape the
            // tightening statements already have, and the reason this one exists
            // beside them rather than instead of them.
            StatementKind::CheckTable { table } => {
                let (context, id) = self.resolve_table(transaction, table)?;
                let found = violations(transaction, context.namespace, context.database, id)?;
                Ok(Outcome::Value(Value::Array(
                    found.into_iter().map(violation_value).collect(),
                )))
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
                condition,
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
                // The condition is tested against the record **as stored**, and
                // before the edit is computed at all — so a refused update
                // writes nothing, touches no index, and never reaches the queue
                // guard or the sealing path.
                //
                // Against the stored record rather than against the payload,
                // which is the rule W205 had to find from the other side: a
                // guard that judges what is about to be written is not judging
                // what the caller said. `WHERE version = 1` beside
                // `SET version = 2` must compare the one already there, and
                // reading the payload would compare it against the value this
                // very statement is setting — always false, and silently.
                if let Some(condition) = condition {
                    let held = self.evaluate_in(transaction, condition, Scope::of(&before))?;
                    if !boolean(&held, condition.span)? {
                        return Err(Error::ConditionNotMet {
                            id: address.id.to_string(),
                            span: condition.span,
                        });
                    }
                }
                let (payload, partial) =
                    self.applied(transaction, edit, before.clone(), target.span)?;
                // One rule rather than two: the result of either shape is a
                // record being written, so `REQUIRED` + `DEFAULT` keeps meaning
                // "this field always holds a value" even when a caller sets one
                // to `none`.
                let (payload, partial) =
                    self.defaults_over(transaction, address.table, payload, partial)?;
                self.put_record_sealing(
                    transaction,
                    address,
                    payload.clone(),
                    partial.as_ref(),
                    span,
                )?;
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
                let (payload, partial) = self.applied(transaction, edit, existing, target.span)?;
                let (payload, partial) =
                    self.defaults_over(transaction, address.table, payload, partial)?;
                self.put_record_sealing(
                    transaction,
                    address,
                    payload.clone(),
                    partial.as_ref(),
                    span,
                )?;
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
            StatementKind::DeleteSpan {
                table,
                lower,
                upper,
                inclusive,
                span: at,
                limit,
            } => self.delete_span(
                transaction,
                table,
                crate::evaluate::IdentitySpan {
                    lower,
                    upper,
                    inclusive: *inclusive,
                    at: *at,
                },
                *limit,
            ),
            StatementKind::DefineBucket {
                name,
                max,
                if_not_exists,
            } => self.define_table(
                transaction,
                name,
                TableShape {
                    schemafull: false,
                    kind: TableKind::Bucket(*max),
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
            StatementKind::DefineVector {
                name,
                dimension,
                distance,
                if_not_exists,
            } => self.define_vector(
                transaction,
                name,
                *dimension,
                distance,
                *if_not_exists,
                span,
            ),
            StatementKind::DropVector { name } => self.drop_vector(transaction, name, span),
            StatementKind::DefineGeo {
                name,
                if_not_exists,
            } => self.define_geo(transaction, name, *if_not_exists, span),
            StatementKind::DropGeo { name } => self.drop_geo(transaction, name, span),
            StatementKind::DefineVault {
                name,
                if_not_exists,
            } => self.define_vault(transaction, name, *if_not_exists, span),
            StatementKind::DropVault { name } => self.drop_vault(transaction, name, span),
            StatementKind::DefineQueue {
                name,
                timeout,
                attempts,
                schemafull,
                graph,
                if_not_exists,
            } => {
                // Resolved before the queue is created, on `DEFINE TABLE`'s own
                // rule and for its reason: a table left standing with a
                // membership nothing resolves belongs to no graph anyone can
                // name, and `INFO FOR GRAPH` would never list it.
                let graph = self.resolve_graph(transaction, graph.as_ref())?;
                self.define_table(
                    transaction,
                    name,
                    TableShape {
                        // Both taken from the statement rather than fixed here.
                        // They were fixed until W208b¹, and what that cost was
                        // not theoretical: a table that is strict and is an end
                        // of a link — which is what a record model's work table
                        // normally is — could not be a queue at all.
                        schemafull: *schemafull,
                        kind: TableKind::Queue(QueueDeclaration {
                            timeout: *timeout,
                            attempts: *attempts,
                        }),
                        identity: IdentityKind::default(),
                        graph,
                    },
                    *if_not_exists,
                    span,
                )
            }
            StatementKind::DropQueue { name } => self.drop_queue(transaction, name, span),
            StatementKind::DefineSeries {
                name,
                retain,
                if_not_exists,
            } => self.define_table(
                transaction,
                name,
                TableShape {
                    schemafull: false,
                    kind: TableKind::Series(SeriesDeclaration { retain: *retain }),
                    // Fixed by the kind rather than offered as a clause, on the
                    // rule a vector store's width follows: the floor is a
                    // position in the key, and only a time-carrying identity has
                    // one. A counter would make the retention a predicate over
                    // some field, which is the thing this engine exists to stop
                    // being — and a series table declared with the wrong
                    // identity could not be corrected afterwards, since records
                    // keep the names they were given.
                    identity: IdentityKind::Uuid,
                    graph: None,
                },
                *if_not_exists,
                span,
            ),
            StatementKind::DropSeries { name } => self.drop_series(transaction, name, span),
            StatementKind::DefineView {
                name,
                read,
                if_not_exists,
            } => self.define_table(
                transaction,
                name,
                TableShape {
                    // A view declares no fields — its shape is whatever its read
                    // answers with — so strictness has nothing to be about, and
                    // `false` is the value that says so rather than a default
                    // nobody chose.
                    schemafull: false,
                    kind: TableKind::View(ViewDeclaration { read: read.clone() }),
                    identity: IdentityKind::default(),
                    graph: None,
                },
                *if_not_exists,
                span,
            ),
            StatementKind::DropView { name } => self.drop_view(transaction, name, span),
            StatementKind::Claim { table, count, span } => {
                self.claim(transaction, table, *count, *span)
            }
            StatementKind::ClaimRecord { target, span } => {
                self.claim_record(transaction, target, *span)
            }
            StatementKind::Release {
                target,
                consumer,
                span,
            } => self.release(transaction, target, consumer.as_deref(), *span),
            StatementKind::ReleaseAll {
                table,
                consumer,
                span,
            } => self.release_all(transaction, table, consumer.as_deref(), *span),
            StatementKind::Reveal {
                target,
                fields,
                span,
            } => self.reveal(transaction, target, fields, *span),
            StatementKind::AddRecipient {
                target,
                recipient,
                material,
                span,
            } => {
                // Both evaluated before the record is read, so an expression
                // that refuses does so without having touched it.
                let recipient = self.recipient_name(transaction, recipient, *span)?;
                let material = self.evaluate(transaction, material)?;
                self.change_recipients(transaction, target, *span, move |fields, table| {
                    tessari_storage::add_recipient(fields, table, &recipient, material)
                })
            }
            StatementKind::RemoveRecipient {
                target,
                recipient,
                span,
            } => {
                let recipient = self.recipient_name(transaction, recipient, *span)?;
                self.change_recipients(transaction, target, *span, move |fields, table| {
                    tessari_storage::remove_recipient(fields, table, &recipient)
                })
            }
            StatementKind::UnsealVault { passphrase, span } => {
                self.unseal_vault(transaction, passphrase, *span)
            }
            StatementKind::SealVault { .. } => {
                self.store.vault().seal()?;
                Ok(Outcome::Done)
            }
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
                    suggestion: answered.suggestion,
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
        self.put_record_sealing(transaction, address, payload, None, span)
    }

    /// The same write, made by the engine rather than by a caller.
    ///
    /// The **only** two callers are `CLAIM` and `RELEASE`, and they exist
    /// because the fields they set are exactly the fields [`Session::put_record`]
    /// refuses. A named path rather than a flag, so that "this write may set the
    /// engine's fields" is a thing the reader can see at the call site instead of
    /// a `true` in an argument list.
    pub(crate) fn put_engine_record(
        &self,
        transaction: &mut Transaction<'_>,
        address: RecordAddress,
        payload: Value,
        span: Span,
    ) -> Result<()> {
        self.write_record(transaction, address, payload, None, span)
    }

    /// The same write, told which fields a partial vault edit supplied.
    ///
    /// `None` is every caller but the two edit paths, and means "seal this
    /// record whole": mint a data key, seal every secret field, write a fresh
    /// key set. `Some` means the payload already carries the record's untouched
    /// envelopes and only the named fields are plaintext, so the record's own
    /// data key and key set are reused — which is what keeps a recipient's wrap
    /// valid across an edit.
    fn put_record_sealing(
        &self,
        transaction: &mut Transaction<'_>,
        address: RecordAddress,
        payload: Value,
        partial: Option<&PartialSeal>,
        span: Span,
    ) -> Result<()> {
        // Every **caller-driven** record write passes through here — the two
        // creates, the insert, the update, the upsert, the set and both vault
        // edits — which is why the queue's engine-field rule sits here rather
        // than in each of them. One rule in one place, and a write path added
        // later inherits it instead of having to remember it. This placement was
        // not the first one tried: the guard sat one level up, in `put_record`,
        // and `UPDATE` reached the write without passing it.
        //
        // It takes the payload by value and hands it back because the rule has
        // two halves: refuse a caller that introduces or changes one of the
        // engine's fields, and carry forward the ones a whole-record write
        // simply left out.
        let mut payload = payload;
        crate::queue::hold_engine_fields(transaction, &address, &mut payload, span)?;
        self.write_record(transaction, address, payload, partial, span)
    }

    /// The write itself, with no question asked about who is making it.
    ///
    /// Split from [`Session::put_record_sealing`] so that the engine's own two
    /// writes — the claim and the release, which set exactly the fields that
    /// funnel refuses — have a path that is *named* rather than a flag passed
    /// into a shared one. A reader at the call site can see which kind of write
    /// it is without following an argument.
    fn write_record(
        &self,
        transaction: &mut Transaction<'_>,
        address: RecordAddress,
        payload: Value,
        partial: Option<&PartialSeal>,
        span: Span,
    ) -> Result<()> {
        let payload = on_the_grid(payload, span)?;
        // Sealing sits between the geometry boundary and the encoder, and the
        // order is the point: `on_the_grid` transforms values, sealing replaces
        // them with ciphertext, and nothing downstream of the encoder can tell
        // the difference — which is what closes the index, the feed, the log and
        // the backup in one move. A vault write reaching the encoder unsealed is
        // the failure this placement exists to make unreachable.
        let payload = match partial {
            Some(edit) => tessari_storage::reseal_named(
                transaction,
                &address,
                payload,
                &edit.keys,
                &edit.named,
            )?,
            None => tessari_storage::seal_secrets(transaction, &address, payload)?,
        };
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
        let context = self.context(transaction, None, edges.span)?;
        // An edge *kind* is looked for first, because it is the narrower word: a
        // kind and an edge table cannot share a name (both claim it in the same
        // catalog), so finding one settles which path this is.
        if let Some(kind) = Catalog::new(transaction).edge_kind_id(
            context.namespace,
            context.database,
            &edges.name.text,
        )? {
            return self.relate_in_graph(transaction, kind, from, edges, to, value);
        }

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

        let address = RecordAddress::new(
            context.namespace,
            context.database,
            edge_table,
            edge_identity(&out, &into),
        );
        // An edge is an ordinary record, so an edge table's declarations apply
        // to it — including their defaults.
        let payload = self.with_defaults(transaction, edge_table, Value::Object(fields))?;
        self.put_record(transaction, address, payload, edges.span)?;
        Ok(Outcome::Done)
    }

    /// `RELATE person:1->works_at->company:1` — an edge of a declared kind.
    ///
    /// The record written here is never read by a walk. It exists so that the
    /// edge is an ordinary mutation, which is what carries it and the adjacency
    /// derived from it through the log to every replica; the neighbours and their
    /// properties are read from the adjacency entries instead.
    fn relate_in_graph(
        &self,
        transaction: &mut Transaction<'_>,
        kind: tessari_types::EdgeKindId,
        from: &RecordTarget,
        edges: &TableRef,
        to: &RecordTarget,
        value: Option<&tessari_ql::Expr>,
    ) -> Result<Outcome> {
        let declared =
            Catalog::new(transaction)
                .edge_kind(kind)?
                .ok_or_else(|| Error::Unknown {
                    entity: "edge kind",
                    name: edges.name.text.clone(),
                    span: edges.span,
                })?;
        let (_, out) = self.address(transaction, from)?;
        let (_, into) = self.address(transaction, to)?;
        // Checked after both endpoints resolve, so a relation naming a record
        // that is not there fails as the missing record rather than as a pair the
        // kind does not join. The order matters too: an unordered check would
        // accept `company:1->works_at->person:1`.
        if out.table != declared.from || into.table != declared.to {
            return Err(Error::EndpointsNotDeclared {
                table: edges.name.text.clone(),
                span: edges.span,
            });
        }

        let mut fields = match value {
            Some(expression) => match self.evaluate(transaction, expression)? {
                Value::Object(given) => given,
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

        // Identified by its endpoints, as an edge-table edge is: relating the
        // same pair twice replaces one record rather than adding a second, which
        // is what makes `RELATE` idempotent and keeps the adjacency a set.
        let address = RecordAddress::new(
            declared.namespace,
            declared.database,
            declared.edges,
            edge_identity(&out, &into),
        );
        self.put_record(transaction, address, Value::Object(fields), edges.span)?;
        Ok(Outcome::Done)
    }

    /// `DELETE person:1->works_at->company:1` — one edge, by what it joins.
    ///
    /// The caller writes the two endpoints and the edge, exactly as they wrote
    /// them to create it, and the identity is derived here by the same rule that
    /// derived it there. That is the whole statement: without it, removing an
    /// edge means reconstructing `"person:1->company:1"` by hand, which is a
    /// caller depending on an internal encoding to undo what `RELATE` did — and
    /// a caller who derives it slightly differently deletes nothing and is told
    /// it worked.
    ///
    /// **The adjacency needs no code here.** The entries are derived from this
    /// record's own mutation in `adjacency::maintain`, so the tombstone written
    /// below removes both of them in the batch that carries it — the same reason
    /// `DROP EDGE` needed no sweep.
    fn delete_edge(
        &self,
        transaction: &mut Transaction<'_>,
        from: &RecordTarget,
        edges: &TableRef,
        to: &RecordTarget,
        answer: Answer,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, edges.span)?;
        let (_, out) = self.address(transaction, from)?;
        let (_, into) = self.address(transaction, to)?;

        // An edge kind is looked for first, for the reason `relate` looks for it
        // first: a kind and an edge table cannot share a name, so finding one
        // settles which path this is.
        let address = if let Some(kind) = Catalog::new(transaction).edge_kind_id(
            context.namespace,
            context.database,
            &edges.name.text,
        )? {
            let declared =
                Catalog::new(transaction)
                    .edge_kind(kind)?
                    .ok_or_else(|| Error::Unknown {
                        entity: "edge kind",
                        name: edges.name.text.clone(),
                        span: edges.span,
                    })?;
            // Refused rather than answered with a no-op. The derived identity
            // for a pair the kind does not join cannot exist, so deleting it
            // would succeed and remove nothing — and a caller who wrote the
            // endpoints the wrong way round would be told their edge is gone.
            // `RELATE` refuses the same two shapes, and an asymmetry between the
            // statement that writes an edge and the one that removes it is the
            // surprising thing, not the refusal.
            if out.table != declared.from || into.table != declared.to {
                return Err(Error::EndpointsNotDeclared {
                    table: edges.name.text.clone(),
                    span: edges.span,
                });
            }
            RecordAddress::new(
                declared.namespace,
                declared.database,
                declared.edges,
                edge_identity(&out, &into),
            )
        } else {
            let (context, edge_table) = self.resolve_table(transaction, edges)?;
            let declared = Catalog::new(transaction)
                .table(edge_table)?
                .filter(|found| found.is_edge())
                .ok_or_else(|| Error::NotAnEdgeTable {
                    table: edges.name.text.clone(),
                    span: edges.span,
                })?;
            if let Some(pair) = declared.edge_endpoints()
                && (out.table != pair.from || into.table != pair.to)
            {
                return Err(Error::EndpointsNotDeclared {
                    table: edges.name.text.clone(),
                    span: edges.span,
                });
            }
            RecordAddress::new(
                context.namespace,
                context.database,
                edge_table,
                edge_identity(&out, &into),
            )
        };

        // An edge that is not there deletes as a record that is not there does:
        // `BEFORE` answers `NONE`, which is the true answer to what was removed.
        let before = match transaction.get(&address)? {
            Some(held) => decode_payload(&held)?,
            None => Value::None,
        };
        transaction.delete(address);
        Ok(answered(answer, before, Value::None))
    }

    fn define_namespace(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        if_not_exists: bool,
        replication: Option<Replication>,
    ) -> Result<Outcome> {
        if if_not_exists
            && Catalog::new(transaction)
                .namespace_id(&name.text)?
                .is_some()
        {
            // The clause is not applied on this branch, and that is the same
            // reading `IF NOT EXISTS` already has everywhere else: the
            // statement did nothing because the namespace was there, so it
            // changes nothing about it either. A definition that quietly
            // re-set a policy on a namespace it did not create would be an
            // `ALTER` wearing a `DEFINE`'s spelling.
            return Ok(Outcome::Done);
        }
        // Asked only of the branch that actually creates one, and only when the
        // statement said nothing: a store with no peers has nowhere to put a
        // second copy, so there the bare form is what a single-node install has
        // always written and is stored as *never stated*. A store that declares
        // a peer is a cluster, and there a namespace holding one copy is a
        // decision somebody is making — ADR-0060's whole point — so it is
        // written down rather than inherited.
        if replication.is_none() {
            let peers = Catalog::new(transaction).replicas()?.len();
            if peers > 0 {
                return Err(Error::ReplicationUnstated {
                    namespace: name.text.clone(),
                    peers,
                    span: name.span,
                });
            }
        }
        let definition = Catalog::new(transaction).create_namespace(&name.text)?;
        if let Some(replication) = replication {
            // Through the same call an `ALTER` makes, so the two statements
            // cannot set this field differently.
            Catalog::new(transaction).set_replication(definition.id, replication)?;
        }
        Ok(Outcome::Done)
    }

    /// `ALTER NAMESPACE prod REPLICATION FACTOR 3`
    ///
    /// Turning replication on for a namespace that already holds data, and off
    /// again (owner requirement D12). **Nothing is redistributed**, and the
    /// absence of a repair step is the point rather than an omission: the log
    /// already holds every write the namespace ever took, so a follower that
    /// begins subscribing replays it from origin. Cassandra's `ALTER KEYSPACE`
    /// needs a `nodetool repair` afterwards because its replicas hold data
    /// rather than a history; ours needs none because the history is the store.
    fn alter_namespace(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        replication: Replication,
    ) -> Result<Outcome> {
        let Some(namespace) = Catalog::new(transaction).namespace_id(&name.text)? else {
            return Err(Error::Unknown {
                entity: "namespace",
                name: name.text.clone(),
                span: name.span,
            });
        };
        Catalog::new(transaction).set_replication(namespace, replication)?;
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

    /// `DEFINE GRAPH social` — the graph, **and the collection its own nodes
    /// live in**.
    ///
    /// Two statements behind one word, exactly as [`Self::define_vector`] and
    /// [`Self::define_geo`] are three behind theirs, and for the same reason: a
    /// graph that owns no records is a label rather than a structure. Without
    /// the collection a caller cannot write a single node until they have
    /// declared a table of their own and marked it `IN <graph>` — so the word
    /// named a structure and delivered a membership flag, which is the objection
    /// that was raised against it twice.
    ///
    /// The collection takes the **graph's own name**, which is what makes
    /// `CREATE social:1 = { … }` the obvious spelling and keeps the declared
    /// engines symmetrical: `DEFINE VECTOR embeddings` is written into as
    /// `embeddings`, and now `DEFINE GRAPH social` is written into as `social`.
    ///
    /// The two names do not collide because [`qualify`] reserves a name under
    /// its **level**, so `graph:<ns>/<db>/social` and `table:<ns>/<db>/social`
    /// are separate reservations. That same reservation is load-bearing a second
    /// time: it is what guarantees the graph's node collection is the *one*
    /// member that can carry the graph's name, which is how [`Self::drop_graph`]
    /// tells it apart from a table the caller attached. A pre-existing table
    /// called `social` therefore refuses this statement with `NameTaken` rather
    /// than being silently adopted, and the graph row rolls back with it.
    ///
    /// A collection rather than a declared table, because the node shape is the
    /// caller's to decide — `DEFINE FIELD … ON social` narrows it afterwards for
    /// anyone who wants that, the same way it would on any other collection.
    ///
    /// [`qualify`]: tessari_storage::Catalog
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
        let graph = Catalog::new(transaction).create_graph(
            context.namespace,
            context.database,
            &name.text,
        )?;
        self.define_table(
            transaction,
            name,
            TableShape {
                schemafull: false,
                kind: TableKind::Collection,
                identity: IdentityKind::default(),
                graph: Some(graph.id),
            },
            if_not_exists,
            span,
        )?;
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
        // The graph's own node collection is not a dependant — it is part of the
        // structure being dropped, and it carries the graph's name because
        // nothing else is allowed to. Counting it here would make every graph
        // this store creates permanently undroppable, refused by a table the
        // caller never declared and cannot name. That is the companion-table
        // shape the bucket already found once; see `StatementKind::DropTable`.
        let (own, attached): (Vec<_>, Vec<_>) = Catalog::new(transaction)
            .tables_in(context.namespace, context.database)?
            .into_iter()
            .filter(|table| table.graph == Some(id))
            .partition(|table| table.name == name.text);
        if let Some(first) = attached.first() {
            return Err(Error::StillDepended {
                depended: Depended::GraphByTable,
                name: name.text.clone(),
                count: attached.len(),
                first: first.name.clone(),
                span,
            });
        }
        let kinds: Vec<_> = Catalog::new(transaction)
            .edge_kinds_in(context.namespace, context.database)?
            .into_iter()
            .filter(|kind| kind.graph == id)
            .collect();
        if let Some(first) = kinds.first() {
            return Err(Error::StillDepended {
                depended: Depended::GraphByEdgeKind,
                name: name.text.clone(),
                count: kinds.len(),
                first: first.name.clone(),
                span,
            });
        }
        for table in own {
            Catalog::new(transaction).drop_table(table.id)?;
        }
        Catalog::new(transaction).drop_graph(id)?;
        Ok(Outcome::Done)
    }

    /// `DEFINE EDGE works_at IN social FROM person TO company`.
    ///
    /// Everything is resolved before anything is written, so a declaration that
    /// names a graph or a table that is not there leaves the store exactly as it
    /// found it — the same ordering the membership clause keeps.
    #[expect(
        clippy::too_many_arguments,
        reason = "the statement's own shape; a struct here would name a grouping \
                  the grammar does not have"
    )]
    fn define_edge(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        graph: &Name,
        from: &Name,
        to: &Name,
        if_not_exists: bool,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        if if_not_exists
            && Catalog::new(transaction)
                .edge_kind_id(context.namespace, context.database, &name.text)?
                .is_some()
        {
            return Ok(Outcome::Done);
        }
        let graph_id = Catalog::new(transaction)
            .graph_id(context.namespace, context.database, &graph.text)?
            .ok_or_else(|| Error::Unknown {
                entity: "graph",
                name: graph.text.clone(),
                span,
            })?;
        let declared =
            Catalog::new(transaction)
                .graph(graph_id)?
                .ok_or_else(|| Error::Unknown {
                    entity: "graph",
                    name: graph.text.clone(),
                    span,
                })?;

        let mut endpoints = Vec::with_capacity(2);
        for endpoint in [from, to] {
            let id = Catalog::new(transaction)
                .table_id(context.namespace, context.database, &endpoint.text)?
                .ok_or_else(|| Error::Unknown {
                    entity: "table",
                    name: endpoint.text.clone(),
                    span,
                })?;
            // Both endpoints must be in the graph, and this is the refusal that
            // bounds a walk: a far side outside the structure would let a
            // traversal leave it and still answer.
            let member = Catalog::new(transaction)
                .table(id)?
                .is_some_and(|table| table.graph == Some(graph_id));
            if !member {
                return Err(Error::EndpointOutsideGraph {
                    table: endpoint.text.clone(),
                    graph: graph.text.clone(),
                    span,
                });
            }
            endpoints.push(id);
        }

        // The companion table holds the edges as ordinary records, which is what
        // carries them — and the adjacency derived from them — through the log to
        // every replica. Nothing can name it.
        let edges = Catalog::new(transaction).create_table(
            context.namespace,
            context.database,
            &Catalog::edges_named(&name.text),
            TableShape::default(),
        )?;
        Catalog::new(transaction).create_edge_kind(
            &declared,
            &name.text,
            endpoints[0],
            endpoints[1],
            edges.id,
        )?;
        Ok(Outcome::Done)
    }

    /// `DROP EDGE works_at` — the kind, its edges, and the adjacency they wrote.
    ///
    /// The edges are deleted rather than the entries being range-swept, and that
    /// is deliberate: deleting a record produces a tombstone in the same log
    /// record, and the adjacency derived from it is removed in the batch that
    /// carries the deletion. A second, parallel way to remove an entry is how one
    /// of the two ends up forgotten.
    fn drop_edge(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        let id = Catalog::new(transaction)
            .edge_kind_id(context.namespace, context.database, &name.text)?
            .ok_or_else(|| Error::Unknown {
                entity: "edge kind",
                name: name.text.clone(),
                span,
            })?;
        let kind = Catalog::new(transaction)
            .edge_kind(id)?
            .ok_or_else(|| Error::Unknown {
                entity: "edge kind",
                name: name.text.clone(),
                span,
            })?;

        let edges: Vec<_> = transaction
            .scan_table(context.namespace, context.database, kind.edges)?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        for id in edges {
            transaction.delete(RecordAddress::new(
                context.namespace,
                context.database,
                kind.edges,
                id,
            ));
        }
        Catalog::new(transaction).drop_table(kind.edges)?;
        Catalog::new(transaction).drop_edge_kind(id)?;
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
        self.refuse_indexing_a_secret(transaction, id, table, fields)?;
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
                    secret: false,
                    default: column.default.as_ref().map(|written| written.text.clone()),
                    analyzer: column.analyzer.as_ref().map(|named| named.text.clone()),
                    assert: column.assert.clone(),
                },
                if_not_exists,
            )?;
        }
        Ok(outcome)
    }

    /// `DEFINE VECTOR embeddings DIMENSION 768 DISTANCE cosine`
    ///
    /// **A desugaring, not a second implementation**, on exactly the reasoning
    /// [`Self::define_table_with_columns`] records. The statement stands for
    /// three:
    ///
    /// ```text
    /// DEFINE COLLECTION embeddings;
    /// DEFINE FIELD vector ON embeddings TYPE vector<768> REQUIRED;
    /// DEFINE INDEX vector ON embeddings FIELDS vector VECTOR cosine;
    /// ```
    ///
    /// and it runs them through the same three functions the long spellings do.
    /// That is what discharges the criterion this node was written for: there is
    /// no store-only path that could disagree with the field one, because the
    /// store's path **is** the field one. A width declared here is checked by
    /// the store's apply pass, where a replica reaches the same verdict from the
    /// record alone — not because this function arranged it, but because there
    /// is nothing else here to arrange.
    ///
    /// The field and the index share the name `vector`. Fields and indexes are
    /// separate namespaces, so nothing collides, and the store then has one name
    /// to remember rather than two — `INFO FOR TABLE` reads
    /// `DEFINE INDEX vector ON embeddings FIELDS vector VECTOR cosine`, which
    /// says what it is.
    ///
    /// `IF NOT EXISTS` covers all three, for the reason it covers a table and
    /// its columns: tolerating the table and then refusing at the field makes
    /// the statement un-re-runnable, which is the opposite of what the words ask.
    fn define_vector(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        dimension: usize,
        distance: &Name,
        if_not_exists: bool,
        span: Span,
    ) -> Result<Outcome> {
        let distance =
            VectorDistance::parse(&distance.text).ok_or_else(|| Error::NoSuchDistance {
                name: distance.text.clone(),
                span: distance.span,
            })?;
        // Written as a conversion rather than a cast, so that raising the
        // ceiling is a question the compiler asks rather than a truncation
        // nobody sees. The parser refuses anything above it, so the only way
        // here is through a change to that ceiling.
        let held = u32::try_from(dimension).map_err(|_| {
            tessari_ql::Error::VectorWidthAboveTheCeiling {
                most: tessari_ql::WIDEST_VECTOR,
                span,
            }
        })?;
        let outcome = self.define_table(
            transaction,
            name,
            TableShape {
                schemafull: false,
                kind: TableKind::Vector(VectorDeclaration {
                    dimension: held,
                    distance,
                }),
                identity: IdentityKind::default(),
                graph: None,
            },
            if_not_exists,
            span,
        )?;
        let table = TableRef {
            database: None,
            name: name.clone(),
            span: name.span,
        };
        let field = Name {
            text: VECTOR_FIELD.to_owned(),
            span: name.span,
        };
        self.define_field(
            transaction,
            &field,
            &table,
            FieldKind::vector(dimension).ok_or(tessari_ql::Error::VectorWidthBelowOne { span })?,
            FieldShape {
                // The one property the three loose statements cannot express
                // between them: `TYPE vector<n>` leaves a field optional, so a
                // record with no vector at all is legal in a table — and is not
                // a record of a vector store.
                required: true,
                secret: false,
                default: None,
                analyzer: None,
                assert: None,
            },
            if_not_exists,
        )?;
        self.define_index(
            transaction,
            &field,
            &table,
            &[FieldPath {
                path: Path::field(VECTOR_FIELD),
                span: name.span,
            }],
            IndexShape {
                unique: false,
                search: false,
                spatial: false,
                vector: Some(distance),
            },
            if_not_exists,
        )?;
        Ok(outcome)
    }

    /// `DROP VECTOR embeddings` — the store's definition, its field and its
    /// index declaration.
    ///
    /// **Not its records.** This calls [`Catalog::drop_table`], which removes the
    /// catalog rows and does not reach what is stored under them — the rule every
    /// drop in this language follows, and the reason there is no `CASCADE`. The
    /// summary line said "its records" until it was read against the code; a
    /// wrong sentence here is worse than a wrong one in the manual, because the
    /// next person to reason about the statement reads it and stops checking.
    ///
    /// Refuses a table that is not one, rather than dropping it. The two words
    /// name different things even where they would remove the same rows, and a
    /// `DROP VECTOR` that quietly removed an ordinary table would be a typo with
    /// the blast radius of a table.
    /// `DROP QUEUE jobs`
    ///
    /// Refuses a table that is not a queue by reporting it as unknown, the shape
    /// `DROP VECTOR` already uses: a word that removed a table of another kind
    /// would make `DROP QUEUE` a second spelling of `DROP TABLE`, and the two
    /// answer to different grants for different reasons.
    /// `DROP SERIES readings`
    ///
    /// Refuses a name that is not a series for [`Self::drop_queue`]'s reason:
    /// the word in the statement is a claim about what is being removed, and a
    /// `DROP SERIES` that removed a plain table would be a statement doing
    /// something other than what it says.
    fn drop_series(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        let unknown = || Error::Unknown {
            entity: "series",
            name: name.text.clone(),
            span,
        };
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(unknown)?;
        let is_series = Catalog::new(transaction)
            .table(id)?
            .is_some_and(|definition| matches!(definition.kind, TableKind::Series(_)));
        if !is_series {
            return Err(unknown());
        }
        Catalog::new(transaction).drop_table(id)?;
        Ok(Outcome::Done)
    }

    fn drop_queue(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        let unknown = || Error::Unknown {
            entity: "queue",
            name: name.text.clone(),
            span,
        };
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(unknown)?;
        let is_queue = Catalog::new(transaction)
            .table(id)?
            .is_some_and(|definition| matches!(definition.kind, TableKind::Queue(_)));
        if !is_queue {
            return Err(unknown());
        }
        Catalog::new(transaction).drop_table(id)?;
        Ok(Outcome::Done)
    }

    /// `DROP VIEW active` — the definition, and there is nothing else.
    ///
    /// A view holds no records, no index and no keyspace, so dropping one frees
    /// nothing and orphans nothing. It refuses a name of another kind for the
    /// reason `DROP QUEUE` does: a word that removed a table of another kind
    /// would make the statement's own name the least reliable thing about it.
    fn drop_view(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        let unknown = || Error::Unknown {
            entity: "view",
            name: name.text.clone(),
            span,
        };
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(unknown)?;
        let is_view = Catalog::new(transaction)
            .table(id)?
            .is_some_and(|definition| matches!(definition.kind, TableKind::View(_)));
        if !is_view {
            return Err(unknown());
        }
        Catalog::new(transaction).drop_table(id)?;
        Ok(Outcome::Done)
    }

    fn drop_vector(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(|| Error::Unknown {
                entity: "vector store",
                name: name.text.clone(),
                span,
            })?;
        let is_vector = Catalog::new(transaction)
            .table(id)?
            .is_some_and(|definition| matches!(definition.kind, TableKind::Vector(_)));
        if !is_vector {
            return Err(Error::Unknown {
                entity: "vector store",
                name: name.text.clone(),
                span,
            });
        }
        Catalog::new(transaction).drop_table(id)?;
        Ok(Outcome::Done)
    }

    /// `DEFINE GEO places` — the collection, its geometry field and its index.
    ///
    /// The same three calls [`Session::define_vector`] makes, in the same order,
    /// through the same functions. There is no geo-only path: what the word
    /// creates is what the three statements create, which is what makes the
    /// round trip through `INFO` honest.
    fn define_geo(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        if_not_exists: bool,
        span: Span,
    ) -> Result<Outcome> {
        let outcome = self.define_table(
            transaction,
            name,
            TableShape {
                schemafull: false,
                kind: TableKind::Geo,
                identity: IdentityKind::default(),
                graph: None,
            },
            if_not_exists,
            span,
        )?;
        let table = TableRef {
            database: None,
            name: name.clone(),
            span: name.span,
        };
        let field = Name {
            text: GEO_FIELD.to_owned(),
            span: name.span,
        };
        self.define_field(
            transaction,
            &field,
            &table,
            FieldKind::Geometry,
            FieldShape {
                // The property the three loose statements cannot express between
                // them, exactly as in the vector store: `TYPE geometry` leaves
                // the field optional, and a record with no geometry is legal in
                // a table while being a record a place store cannot answer for.
                required: true,
                secret: false,
                default: None,
                analyzer: None,
                assert: None,
            },
            if_not_exists,
        )?;
        self.define_index(
            transaction,
            &field,
            &table,
            &[FieldPath {
                path: Path::field(GEO_FIELD),
                span: name.span,
            }],
            IndexShape {
                unique: false,
                search: false,
                spatial: true,
                vector: None,
            },
            if_not_exists,
        )?;
        Ok(outcome)
    }

    /// `DROP GEO places` — the store's definition, its geometry field and its
    /// spatial index declaration, and not its records; see
    /// [`Session::drop_vector`] for why the distinction is written down.
    ///
    /// Refuses a table that is not one, for the reason [`Session::drop_vector`]
    /// does: the words name different things even where they would remove the
    /// same rows, and a `DROP GEO` that quietly removed an ordinary table would
    /// be a typo with the blast radius of a table.
    /// `DEFINE VAULT team`
    ///
    /// A table of the vault kind, carrying a key minted here and wrapped under
    /// the store's master key. That is why this is the **one** declaration that
    /// needs an unsealed store: there is no way to defer the key without
    /// creating a vault nothing can ever write to, and a declaration that
    /// succeeded and left the key for later would be a vault that refuses every
    /// write while `INFO` reports it as ready.
    fn define_vault(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        if_not_exists: bool,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        // The scope is computed by the storage layer's own function, not
        // rebuilt here. The write path recomputes the same binding from the
        // stored definition, and two implementations of one binding produce a
        // vault that accepts every write and opens nothing, with both halves
        // looking correct in isolation.
        let key = tessari_storage::mint_vault_key(
            self.store,
            context.namespace,
            context.database,
            &name.text,
        )?;
        self.define_table(
            transaction,
            name,
            TableShape {
                // **Strict**, unlike every other declared store, and this is the
                // one place the default is wrong rather than merely different.
                //
                // What seals a field is the `SECRET` marker on its declaration.
                // A field nobody declared carries no marker, so in a schemaless
                // vault it is accepted and written in the clear — beside the
                // sealed fields, inside the store whose whole promise is that it
                // holds nothing readable. The caller doing it is doing the most
                // ordinary thing a schemaless store allows, and believes the
                // record is protected because the record is in a vault.
                //
                // An earlier comment here argued that `SCHEMAFULL` would be a
                // second thing to remember for a property it does not provide.
                // It provides exactly one property and this is it: strictness is
                // what makes *declared* and *sealed* the same set.
                schemafull: true,
                kind: TableKind::Vault(VaultDeclaration { key }),
                identity: IdentityKind::default(),
                graph: None,
            },
            if_not_exists,
            span,
        )
    }

    /// `DROP VAULT team` — the crypto-shred.
    ///
    /// Dropping the definition destroys the wrapped key with it, and the key is
    /// the only copy: every record of this vault in every backup, snapshot and
    /// replica that will ever be restored becomes ciphertext under a key that
    /// exists nowhere. That is the deletion claim, and it is the only one a
    /// store like this can honestly make — a row delete says something about the
    /// live table and nothing about the data.
    ///
    /// It does **not** require an unsealed store. Destroying a key needs no key,
    /// and demanding one would mean a store that cannot be unsealed can never be
    /// cleaned up — which is exactly the store an operator most wants to shred.
    fn drop_vault(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        let missing = || Error::Unknown {
            entity: "vault",
            name: name.text.clone(),
            span,
        };
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(missing)?;
        let is_vault = Catalog::new(transaction)
            .table(id)?
            .is_some_and(|definition| definition.is_vault());
        if !is_vault {
            return Err(missing());
        }
        Catalog::new(transaction).drop_table(id)?;
        Ok(Outcome::Done)
    }

    /// `UNSEAL VAULT WITH '…'` — the master key enters this process.
    ///
    /// # The first unseal creates the store's root, and says so
    ///
    /// A store that has never held a secret has no root record, and something
    /// has to make one. Rather than add a second statement for a once-in-a-store
    /// act, this one initialises when there is nothing to unlock — and the
    /// outcome says **which** of the two happened, because the hazard here is
    /// that a mistyped passphrase on an empty store becomes the passphrase, and
    /// there is deliberately no path that replaces a root once written.
    ///
    /// Saying which happened is what makes that hazard survivable: an operator
    /// who expected *unsealed* and reads *initialised* knows immediately, while
    /// the store still holds nothing. Q-415 carries the open question of whether
    /// initialisation should be its own statement anyway.
    fn unseal_vault(
        &self,
        transaction: &mut Transaction<'_>,
        passphrase: &str,
        span: Span,
    ) -> Result<Outcome> {
        let _ = span;
        if let Some(root) = Catalog::new(transaction).vault_root()? {
            self.store.vault().unseal(&root.0, passphrase)?;
            return Ok(Outcome::Value(Value::from("unsealed")));
        }
        let root = tessari_storage::initialise_root(self.store, passphrase)?;
        Catalog::new(transaction).set_vault_root(&root);
        Ok(Outcome::Value(Value::from("initialised")))
    }

    /// `REVEAL password FROM team:github` — the only path to a plaintext.
    ///
    /// Reads the stored record, opens the record's data key, and opens each
    /// named secret field under it. Every other read path in this store sees
    /// what is on disk, which is ciphertext.
    ///
    /// # What it refuses, and why each refusal is here rather than in the parser
    ///
    /// A field that is not declared `SECRET` is refused rather than returned in
    /// the clear. `REVEAL` answers with plaintext, so a caller reading its answer
    /// has no way to tell which entries were ever sealed — and a verb that
    /// sometimes returns a secret and sometimes returns whatever was lying about
    /// is one whose output nobody can reason about. The parser cannot make this
    /// refusal because it does not know what any name refers to.
    ///
    /// A table that is not a vault is refused for the same reason: `REVEAL` over
    /// an ordinary table would be a `SELECT` wearing a word that promises more.
    /// Resolve a record in a vault, for the three statements that name one.
    ///
    /// A table that is not a vault is reported as **no such vault** rather than
    /// as a wrong kind, which is the same answer `REVEAL` gives: a caller who
    /// may not reach a table learns nothing from these verbs that `SELECT`
    /// would not have told them, and one who may reach it gets a message naming
    /// the word they should have used.
    pub(crate) fn vault_record(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        span: Span,
    ) -> Result<(RecordAddress, TableDefinition, BTreeMap<String, Value>)> {
        let missing = || Error::Unknown {
            entity: "vault",
            name: target.table.name.text.clone(),
            span,
        };
        let (_context, address) = self.address(transaction, target)?;
        let definition = Catalog::new(transaction)
            .table(address.table)?
            .ok_or_else(missing)?;
        if !definition.is_vault() {
            return Err(missing());
        }
        let Some(stored) = transaction.get(&address)? else {
            return Err(Error::NoSuchRecord {
                id: address.id.to_string(),
                span,
            });
        };
        let Value::Object(held) = decode_payload(&stored)? else {
            return Err(missing());
        };
        Ok((address, definition, held))
    }

    /// A recipient's name, which must be text.
    ///
    /// Anything else is refused by **type** — never by value. A caller who wrote
    /// a field reference here would otherwise have the store quote whatever that
    /// field holds back at them, and on a vault's record that is the one thing
    /// this feature exists to keep unquoted.
    fn recipient_name(
        &self,
        transaction: &mut Transaction<'_>,
        expression: &tessari_ql::Expr,
        span: Span,
    ) -> Result<String> {
        match self.evaluate(transaction, expression)? {
            Value::String(name) => Ok(name),
            other => Err(Error::RecipientIsNotAName {
                found: other.type_name(),
                span,
            }),
        }
    }

    /// Add or remove one entry in a record's recipient set.
    ///
    /// # Why this does not go through `put_record`
    ///
    /// Two reasons, and both are structural rather than stylistic. The write
    /// path **refuses** a payload carrying the reserved key set at all, so this
    /// change cannot be expressed as an ordinary write. And a write through it
    /// re-seals: a fresh data key and fresh nonces for every secret field, so
    /// every ciphertext on the record would change — which is precisely what
    /// criterion F2 forbids, and what would make an added recipient
    /// indistinguishable from a rewritten secret in a backup diff.
    ///
    /// Nothing is re-indexed, and that is correct rather than an omission: the
    /// only field this touches is the key set, an index over a secret field is
    /// refused at declaration, and no indexed value changes. The test that
    /// holds this true reads through an index on a plain field after a
    /// recipient is added.
    fn change_recipients(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        span: Span,
        change: impl FnOnce(
            &mut BTreeMap<String, Value>,
            &str,
        ) -> std::result::Result<(), tessari_storage::Error>,
    ) -> Result<Outcome> {
        let (address, definition, mut held) = self.vault_record(transaction, target, span)?;
        change(&mut held, &definition.name)?;
        transaction.put(address, encode_payload(&Value::Object(held)).into_bytes());
        Ok(Outcome::Done)
    }

    /// `REVEAL` — and the record of it, written before the answer leaves.
    ///
    /// # Why the audit is here and not inside the opening
    ///
    /// Because the property is about ORDER, and order is only visible from the
    /// place that owns both events. Written afterwards, every crash, kill,
    /// timeout and partial write between the decryption and the log produces a
    /// secret release with no record — and the two orderings are
    /// indistinguishable whenever nothing fails, which is why the defect
    /// survives review.
    ///
    /// The refusal is recorded too. A denial is the reconnaissance signal: the
    /// first evidence of somebody probing what exists and what they can reach,
    /// and without it the earliest thing the trail shows is a successful read,
    /// which is the point at which the damage is already done.
    ///
    /// A trail that cannot be written **refuses**, including refusing to report
    /// the refusal it was trying to record. That is deliberate: the alternative
    /// leaks whether a record exists to a caller who has disabled the trail.
    fn reveal(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        fields: &[Name],
        span: Span,
    ) -> Result<Outcome> {
        let (_context, address) = self.address(transaction, target)?;
        let opened = self.open_secrets(transaction, target, fields, span);
        let asked: Vec<String> = fields.iter().map(|field| field.text.clone()).collect();
        let record = address.id.to_literal();
        self.store.audit().record(
            self.store,
            &tessari_storage::VaultRead {
                actor: self
                    .identity
                    .user()
                    .map_or("anonymous", |user| user.name.as_str()),
                namespace: address.namespace,
                database: address.database,
                vault: &target.table.name.text,
                record: &record,
                fields: &asked,
                served: opened.is_ok(),
            },
        )?;
        opened
    }

    /// The opening itself, with no knowledge that it is being recorded.
    fn open_secrets(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        fields: &[Name],
        span: Span,
    ) -> Result<Outcome> {
        let missing = || Error::Unknown {
            entity: "vault",
            name: target.table.name.text.clone(),
            span,
        };
        let (_context, address) = self.address(transaction, target)?;
        let id = address.table;
        let definition = Catalog::new(transaction).table(id)?.ok_or_else(missing)?;
        if !definition.is_vault() {
            return Err(missing());
        }

        let secrets: BTreeMap<String, ()> = Catalog::new(transaction)
            .fields_on(id)?
            .into_iter()
            .filter(|field| field.secret)
            .map(|field| (field.name, ()))
            .collect();

        // Named fields are checked against the declaration BEFORE the record is
        // read, so a caller cannot use the difference between "no such field"
        // and "no such record" to learn which records exist.
        let wanted: Vec<String> = if fields.is_empty() {
            secrets.keys().cloned().collect()
        } else {
            for field in fields {
                if !secrets.contains_key(&field.text) {
                    return Err(Error::NotASecret {
                        field: field.text.clone(),
                        vault: target.table.name.text.clone(),
                        span,
                    });
                }
            }
            fields.iter().map(|field| field.text.clone()).collect()
        };

        let Some(stored) = transaction.get(&address)? else {
            return Ok(Outcome::Value(Value::None));
        };
        let Value::Object(held) = decode_payload(&stored)? else {
            return Err(missing());
        };

        let data_key = tessari_storage::open_data_key(transaction, &address, &definition, &held)?;
        let mut opened = BTreeMap::new();
        for name in wanted {
            let Some(Value::Bytes(envelope)) = held.get(&name) else {
                // Declared secret, absent from this record. Reported as absent
                // rather than skipped: a caller who asked for three fields and
                // got two has no way to tell which one was missing.
                opened.insert(name, Value::None);
                continue;
            };
            let value =
                tessari_storage::open_field(&data_key, &address, &definition, &name, envelope)?;
            opened.insert(name, value);
        }
        Ok(Outcome::Value(Value::Object(opened)))
    }

    /// Refuse an index whose fields include one the vault seals.
    ///
    /// Checked against the **declaration** rather than against any record, so a
    /// vault with no rows yet refuses exactly as one with a million does. The
    /// alternative — noticing at index-build time — would accept the statement
    /// and fail later, by which point the declaration is in the catalog and the
    /// failure looks like the data's fault.
    fn refuse_indexing_a_secret(
        &self,
        transaction: &mut Transaction<'_>,
        id: TableId,
        table: &TableRef,
        fields: &[tessari_ql::FieldPath],
    ) -> Result<()> {
        if !Catalog::new(transaction)
            .table(id)?
            .is_some_and(|definition| definition.is_vault())
        {
            return Ok(());
        }
        let secrets: Vec<String> = Catalog::new(transaction)
            .fields_on(id)?
            .into_iter()
            .filter(|field| field.secret)
            .map(|field| field.name)
            .collect();
        for field in fields {
            // The **root** of the path, because indexing `password.length` is
            // indexing the secret just as surely as indexing `password` is — it
            // is a projection of the plaintext, and a projection of a plaintext
            // is a plaintext somebody derived.
            let root = field.path.root();
            if secrets.iter().any(|secret| secret == root) {
                return Err(Error::NotIndexable {
                    field: root.to_owned(),
                    table: table.name.text.clone(),
                    span: table.span,
                });
            }
        }
        Ok(())
    }

    fn drop_geo(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(|| Error::Unknown {
                entity: "geo store",
                name: name.text.clone(),
                span,
            })?;
        let is_geo = Catalog::new(transaction)
            .table(id)?
            .is_some_and(|definition| definition.kind == TableKind::Geo);
        if !is_geo {
            return Err(Error::Unknown {
                entity: "geo store",
                name: name.text.clone(),
                span,
            });
        }
        Catalog::new(transaction).drop_table(id)?;
        Ok(Outcome::Done)
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
        if shape.secret
            && !Catalog::new(transaction)
                .table(id)?
                .is_some_and(|definition| definition.is_vault())
        {
            return Err(Error::SecretNeedsVault {
                table: table.name.text.clone(),
                span: table.span,
            });
        }
        // An analyzer is attached to a field by **name**, and nothing in the
        // catalog enforces the link — which is why `DROP ANALYZER` counts the
        // fields naming one before it removes it. Resolving the name here closes
        // that guard's other end. Without it a single misspelling reaches the
        // exact state the drop-side refusal exists to prevent, and the symptom
        // is not an error anybody sees: it is a search that quietly stops
        // matching.
        if let Some(named) = &shape.analyzer {
            let declared = Catalog::new(transaction)
                .analyzers()?
                .into_iter()
                .any(|held| &held.name == named);
            if !declared {
                return Err(Error::Unknown {
                    entity: "analyzer",
                    name: named.clone(),
                    span: name.span,
                });
            }
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
        peer: &Peer<'_>,
        if_not_exists: bool,
    ) -> Result<Outcome> {
        let declared = Catalog::new(transaction)
            .replicas()?
            .into_iter()
            .any(|found| found.name == peer.name.text);
        if if_not_exists && declared {
            return Ok(Outcome::Done);
        }
        // The words are read before the name is claimed, so a misspelled role
        // leaves nothing behind: the statement either declares the peer it was
        // asked for or declares nothing.
        let roles = peer
            .roles
            .map(named_roles)
            .transpose()?
            .unwrap_or(Roles::NONE);
        // A subscription on a row that names no node used to be refused here,
        // on the reasoning that a grant needs somebody to hold it and the peer
        // door — which looks a follower up by the id its certificate proved —
        // would never find one written against nobody. That was true while
        // nothing could ever bind such a row, and W282 is the wave that makes it
        // false: the row is bound by the first inbound greeting, and the grant
        // becomes findable at the moment the peer arrives (Q-611).
        //
        // It is inert until then rather than broad: `Subscriptions::granted`
        // matches `row.node == Some(follower)`, so an unbound row answers
        // nobody. What changes is only *when* the grant takes effect, never who
        // it can reach — and the recipient is still a node this cluster issued a
        // peer credential to, which is the act that admits a member.
        let replicates = match peer.replicates {
            None => None,
            Some(named) => Some(self.reach_of(transaction, named)?),
        };
        Catalog::new(transaction).create_replica(
            &peer.name.text,
            peer.endpoint,
            roles,
            peer.node,
            replicates,
        )?;
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
    /// Fold defaults in, and tell a partial vault edit about anything they added.
    ///
    /// A default fires only for a declared field the payload is missing, so on an
    /// edit it fires only for a field declared *after* the record was written —
    /// and the value it writes is plaintext. If such a field is a secret and the
    /// reseal never hears its name, it is carried past the sealer and reaches the
    /// encoder in the clear, which is the one outcome this whole module exists to
    /// make unreachable.
    ///
    /// So the names the defaults introduce join the named set. That is sound for
    /// the same reason the rest of the set is: they were produced here, not read
    /// back out of the store, so they are plaintext by construction rather than
    /// by inspection.
    fn defaults_over(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        payload: Value,
        partial: Option<PartialSeal>,
    ) -> Result<(Value, Option<PartialSeal>)> {
        let before: Vec<String> = match (&partial, &payload) {
            (Some(_), Value::Object(fields)) => fields.keys().cloned().collect(),
            _ => Vec::new(),
        };
        let payload = self.with_defaults(transaction, table, payload)?;
        let Some(mut edit) = partial else {
            return Ok((payload, None));
        };
        if let Value::Object(fields) = &payload {
            for name in fields.keys() {
                if !before.contains(name) {
                    edit.named.insert(name.clone());
                }
            }
        }
        Ok((payload, Some(edit)))
    }

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
    ) -> Result<(Value, Option<PartialSeal>)> {
        // An edit that computes from the record cannot compute from a vault's,
        // and this is where that is said. `SET` and `MERGE` build on `existing`;
        // in a vault `existing` holds the store's own `#keys` map and the
        // *ciphertext* of every sealed field, so the write that followed refused
        // with `VaultReservedField` — naming a field the caller never wrote and
        // cannot see — or, in a vault with a second secret, with a schema
        // violation saying a `string` field held bytes.
        //
        // Neither refusal was wrong about the write; both were unreadable about
        // the cause. And the cause is not a defect to route around: computing
        // from a sealed field means opening it, opening one is `REVEAL`, and
        // `REVEAL` writes an audit entry before it answers. An `UPDATE` that
        // quietly opened three secrets to re-seal them would put plaintext in
        // this process with nothing anywhere recording that it was there.
        //
        // So the whole record is the unit of a vault write, which is what
        // `UPDATE … = { … }` already is.
        let sealed_record = matches!(
            &existing,
            Value::Object(fields) if fields.contains_key(tessari_storage::KEYS_FIELD)
        );
        match edit {
            // Replacing the whole record is a write like a create, so the
            // defaults apply to it the same way — and so does the fresh data
            // key, which is why `= { … }` still clears the recipient set while
            // the two edits below no longer do.
            Edit::Whole(value) => Ok((self.evaluate(transaction, value)?, None)),
            Edit::Fields(assignments) if sealed_record => {
                let (base, keys) = without_the_key_set(existing);
                let named = assignments
                    .iter()
                    .map(|assignment| assignment.route.path.root().to_owned())
                    .collect();
                let payload = self.edited(transaction, base, assignments, span, true)?;
                Ok((payload, Some(PartialSeal { keys, named })))
            }
            Edit::Fields(assignments) => Ok((
                self.edited(transaction, existing, assignments, span, false)?,
                None,
            )),
            Edit::Merge(value) => {
                // The value position, like every other object literal — see
                // `Edit::Merge`. Computing from the record is `SET`'s job, which
                // is also why `MERGE` needs no restriction on a vault: it never
                // reads the record in the first place.
                let incoming = self.evaluate(transaction, value)?;
                let Value::Object(supplied) = &incoming else {
                    return Err(Error::MergeIsNotAnObject {
                        found: incoming.type_name(),
                        span,
                    });
                };
                if sealed_record {
                    let named = supplied.keys().cloned().collect();
                    let (base, keys) = without_the_key_set(existing);
                    return Ok((merged(base, incoming), Some(PartialSeal { keys, named })));
                }
                Ok((merged(existing, incoming), None))
            }
        }
    }

    fn edited(
        &self,
        transaction: &mut Transaction<'_>,
        existing: Value,
        assignments: &[Assignment],
        span: Span,
        sealed: bool,
    ) -> Result<Value> {
        let mut record = existing;
        let mut wanted = Vec::with_capacity(assignments.len());
        for assignment in assignments {
            // On a vault, the assignment is evaluated against **no record**, and
            // the evaluator is then its own detector for an expression that
            // reads one. The alternative — walking the expression looking for
            // field references — is the piece that would be incomplete by
            // construction, and its incompleteness would evaluate a secret to
            // its ciphertext rather than refusing.
            let scope = if sealed {
                crate::evaluate::Scope::none()
            } else {
                crate::evaluate::Scope::of(&record)
            };
            let held = self
                .evaluate_in(transaction, &assignment.value, scope)
                .map_err(|error| match error {
                    Error::NoRecordInScope { .. } if sealed => {
                        Error::VaultEditComputesFromTheRecord { span }
                    }
                    other => other,
                })?;
            wanted.push(held);
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
/// The identity an edge record is written under.
///
/// One function rather than the formula repeated at each site, because `RELATE`
/// writes it and `DELETE a->e->b` has to derive the *same* string to find what
/// was written. Two copies that drift do not fail: the delete addresses a key
/// nothing is under, removes nothing, and reports success.
///
/// It is derived rather than supplied so that relating the same pair twice
/// replaces one record instead of adding a second — which is what makes `RELATE`
/// idempotent and keeps a node's adjacency a set.
fn edge_identity(out: &RecordAddress, into: &RecordAddress) -> RecordId {
    RecordId::from(format!(
        "{}:{}->{}:{}",
        out.table, out.id, into.table, into.id
    ))
}

fn answered(answer: Answer, before: Value, after: Value) -> Outcome {
    match answer {
        Answer::Nothing => Outcome::Done,
        Answer::Before => Outcome::Value(before),
        Answer::After => Outcome::Value(after),
    }
}

/// What a partial vault edit hands the sealer.
///
/// Two facts the storage layer cannot work out for itself: the record's own key
/// set, read from the store rather than from anything a caller wrote, and the
/// names the edit supplied. Everything not named is already an envelope.
struct PartialSeal {
    /// The record's `#keys`, exactly as it was stored.
    keys: Value,
    /// The fields this edit wrote, and therefore the only ones to seal.
    named: std::collections::BTreeSet<String>,
}

/// Split a stored vault record into the part an edit works on and its key set.
///
/// The key set comes off because the payload an edit produces goes back through
/// the sealer, and the sealer refuses a payload carrying one — that refusal is
/// what stops a caller injecting a key map, and it is not weakened for the edit
/// path. The set travels beside the payload instead, in [`PartialSeal`].
fn without_the_key_set(record: Value) -> (Value, Value) {
    let Value::Object(mut fields) = record else {
        return (record, Value::None);
    };
    let keys = fields
        .remove(tessari_storage::KEYS_FIELD)
        .unwrap_or(Value::None);
    (Value::Object(fields), keys)
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

/// One disagreement, as the answer carries it.
///
/// The rule is a stable word and the detail is the store's own sentence, so a
/// caller scripting a repair matches on the first and shows the second — rather
/// than parsing a message written for a person, which is the thing that breaks
/// when the message is improved.
fn violation_value(found: Violation) -> Value {
    Value::Object(BTreeMap::from([
        ("record".to_owned(), Value::String(found.record)),
        ("field".to_owned(), Value::String(found.field)),
        ("rule".to_owned(), Value::String(found.rule.to_owned())),
        ("detail".to_owned(), Value::String(found.detail)),
    ]))
}
