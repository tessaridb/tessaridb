//! Running one statement against the store.

use tessari_encoding::{NODE_ID_LEN, decode_payload};
use tessari_ql::{
    ConsumerSource, CreateTarget, FieldMapping, FieldPath, Name, ReachRef, Span, StatementKind,
    TableChange, TableRef,
};
use tessari_storage::{
    Catalog, FieldShape, IndexShape, QueueDeclaration, SeriesDeclaration, TableKind, TableShape,
    Transaction, VectorDistance, ViewDeclaration, violations,
};

use tessari_types::{IdentityKind, Value};

use crate::condition::boolean;
use crate::error::{Error, Result};
use crate::evaluate::Scope;
use crate::outcome::Outcome;
use crate::session::Session;

mod cluster;
mod containers;
mod defaults;
mod editing;
mod fields;
mod relations;
mod tables;
mod topic_consumer;
mod vaults;
mod writes;

use editing::{PartialSeal, answered, edge_identity, named_roles, violation_value};

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
    leads: Option<&'a ReachRef>,
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
                class,
            } => self.define_namespace(transaction, name, *if_not_exists, *replication, *class),
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
                split,
                conflict,
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
                        conflict: *conflict,
                        split: split.clone(),
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
                limit,
            } => self.define_table(
                transaction,
                name,
                TableShape {
                    kind: TableKind::Space(crate::kv::declared_space(*limit)),
                    ..TableShape::default()
                },
                *if_not_exists,
                span,
            ),
            // A topic declares no fields either; what it declares is how long it
            // keeps a message, how large one may be, and who may append (G037).
            StatementKind::DefineTopic {
                name,
                if_not_exists,
                clauses,
            } => self.define_table(
                transaction,
                name,
                TableShape {
                    kind: TableKind::Topic(crate::topic::declared_topic(*clauses)),
                    ..TableShape::default()
                },
                *if_not_exists,
                span,
            ),
            StatementKind::ReadTopic {
                topic,
                consumer,
                after,
                limit,
            } => self.read_topic(
                transaction,
                topic,
                crate::topic::Reading {
                    consumer: consumer.as_deref(),
                    after: after.as_ref(),
                    limit: limit.as_ref(),
                },
                span,
            ),
            StatementKind::DefineGroup {
                name,
                topic,
                if_not_exists,
                clauses,
            } => self.define_group(transaction, name, topic, *if_not_exists, clauses, span),
            StatementKind::DropGroup { name, topic } => {
                self.drop_group(transaction, name, topic, span)
            }
            StatementKind::AlterGroup {
                name,
                topic,
                start_at,
            } => self.alter_group(transaction, name, topic, start_at, span),
            StatementKind::AckTopic {
                topic,
                consumer,
                positions,
            } => self.ack_topic(transaction, topic, consumer, positions, span),
            StatementKind::NackTopic {
                topic,
                consumer,
                positions,
                delay,
            } => self.nack_topic(transaction, topic, consumer, (positions, *delay), span),
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
                crate::identity::UserDeclaration {
                    name,
                    scope: scope.as_ref(),
                    role,
                    password,
                    if_not_exists: *if_not_exists,
                },
                span,
            ),
            StatementKind::DefineNode {
                roles,
                endpoints,
                retain,
            } => self.define_node(roles.as_deref(), endpoints.as_deref(), *retain),
            StatementKind::DefineFailover {
                awareness,
                collection,
                round,
                campaign,
                lease,
            } => self.define_failover(
                transaction,
                [*awareness, *collection, *round, *campaign, *lease],
                span,
            ),
            StatementKind::DefineReplica {
                name,
                endpoint,
                roles,
                node,
                replicates,
                leads,
                if_not_exists,
            } => self.define_replica(
                transaction,
                &Peer {
                    name,
                    endpoint,
                    roles: roles.as_deref(),
                    node: *node,
                    replicates: replicates.as_ref(),
                    leads: leads.as_ref(),
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
            StatementKind::DefineTopicConsumer {
                name,
                topic,
                group,
                identity,
                mapping,
                destination,
                on_failure,
                parallelism,
                if_not_exists,
            } => self.define_topic_consumer(
                transaction,
                &topic_consumer::TopicDeclared {
                    name,
                    topic,
                    group,
                    identity,
                    mapping,
                    destination,
                    on_failure: *on_failure,
                    parallelism: *parallelism,
                },
                *if_not_exists,
                span,
            ),
            StatementKind::DropTopicConsumer { name } => {
                self.drop_topic_consumer(transaction, name, span)
            }
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
            StatementKind::Set {
                target,
                value,
                expire,
                condition,
            } => self.set_key(
                transaction,
                target,
                value,
                expire.as_ref(),
                condition.as_ref(),
                span,
            ),
            StatementKind::Incr { target, by } => {
                self.increment(transaction, target, by.as_ref(), span)
            }
            StatementKind::Expire { target, at } => self.expire_key(transaction, target, at),
            StatementKind::Persist { target } => self.persist_key(transaction, target),
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
                    conflict: None,
                    split: Vec::new(),
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
                    conflict: None,
                    split: Vec::new(),
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
                        conflict: None,
                        split: Vec::new(),
                    },
                    *if_not_exists,
                    span,
                )
            }
            StatementKind::DropQueue { name } => self.drop_queue(transaction, name, span),
            StatementKind::DefineSeries {
                name,
                retain,
                time,
                if_not_exists,
            } => self.define_table(
                transaction,
                name,
                TableShape {
                    schemafull: false,
                    kind: TableKind::Series(SeriesDeclaration {
                        retain: *retain,
                        time: time.as_ref().map(|field| field.text.clone()),
                    }),
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
                    conflict: None,
                    split: Vec::new(),
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
                    conflict: None,
                    split: Vec::new(),
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
            StatementKind::Keys {
                space,
                range,
                prefix,
                after,
                limit,
            } => self.keys(
                transaction,
                space,
                crate::kv::Walk {
                    range: range.as_ref(),
                    prefix: prefix.as_ref(),
                    after: after.as_ref(),
                    limit: *limit,
                },
            ),
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
}
