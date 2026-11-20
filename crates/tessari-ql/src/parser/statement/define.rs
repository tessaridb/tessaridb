//! DEFINE, and the kinds of table and index it declares.

mod declared;
mod event;
mod feed;
mod param;
mod search;
use super::Parser;
use tessari_types::{ConflictPolicy, IdentityKind, RecordId};

use crate::ast::{EdgeClause, Name, StatementKind, TableExpiry};
use crate::error::{Error, Result};
use crate::token::Keyword;

impl Parser<'_> {
    pub(super) fn define_statement(&mut self) -> Result<StatementKind> {
        self.advance();
        match self.peek_keyword() {
            Some(Keyword::Namespace) => {
                self.advance();
                let if_not_exists = self.eat_if_not_exists()?;
                let name = self.name()?;
                // Read in clause order, and the two are read in separate
                // statements rather than inside the struct literal because
                // field initialisers are evaluated in source order and a later
                // reordering of the fields would silently reorder the grammar.
                let replication = self.replication_clause()?;
                let class = self.replication_class_clause()?;
                let acknowledge = self.acknowledgement_clause()?;
                // The split/merge default the namespace states for the tables
                // born into it, in both directions: `SPLIT AUTOMATICALLY`
                // turns the default on for what the namespace creates, and
                // `SPLIT MANUALLY` states the off a re-run definition wants
                // without the IF NOT EXISTS branch having to special-case it —
                // the clause is applied only when the namespace is created,
                // the reading its replication neighbour already has.
                let auto_split = if self.eat_word("split") {
                    if self.eat_word("automatically") {
                        Some(true)
                    } else if self.eat_word("manually") {
                        Some(false)
                    } else {
                        return Err(
                            self.error_here("`AUTOMATICALLY` or `MANUALLY` after `SPLIT`")
                        );
                    }
                } else {
                    None
                };
                Ok(StatementKind::DefineNamespace {
                    name,
                    if_not_exists,
                    replication,
                    class,
                    acknowledge,
                    auto_split,
                })
            }
            Some(Keyword::Database) => {
                self.advance();
                let if_not_exists = self.eat_if_not_exists()?;
                Ok(StatementKind::DefineDatabase {
                    name: self.name()?,
                    if_not_exists,
                })
            }
            Some(Keyword::Table) => {
                self.advance();
                let if_not_exists = self.eat_if_not_exists()?;
                let name = self.name()?;
                // The columns come before the flags rather than in the same
                // order-free loop: they are the subject of the statement and
                // the flags are adjectives on it, and `DEFINE TABLE t
                // SCHEMAFULL (…)` reads as though the parentheses qualified
                // `SCHEMAFULL`.
                let columns = self.columns()?;
                // Either marker, in either order, and neither twice. Order-free
                // because there is no reading under which one has to precede the
                // other, and a grammar that insisted would only be remembered
                // wrong.
                //
                // A declared table is **strict by default**, so `schemafull`
                // starts true and `SCHEMALESS` is what turns it off. The pair is
                // read as two words rather than one optional word, because a
                // script that says which reading it wants keeps saying it after
                // the default moves again.
                let mut strictness: Option<bool> = None;
                let mut edge: Option<EdgeClause> = None;
                let mut identity: Option<IdentityKind> = None;
                let mut graph: Option<Name> = None;
                let mut conflict: Option<ConflictPolicy> = None;
                let mut split: Option<Vec<RecordId>> = None;
                let mut partition: Option<Name> = None;
                let mut spread = false;
                let mut expire: Option<TableExpiry> = None;
                let mut store: Option<crate::ast::StoreKind> = None;
                let mut partitions: Option<(u64, String)> = None;
                loop {
                    if strictness.is_none() && self.eat_keyword(Keyword::Schemafull) {
                        strictness = Some(true);
                    } else if strictness.is_none() && self.eat_keyword(Keyword::Schemaless) {
                        strictness = Some(false);
                    } else if edge.is_none() && self.eat_keyword(Keyword::Edge) {
                        edge = Some(self.edge_clause()?);
                    } else if identity.is_none() && self.eat_word("identity") {
                        identity = Some(self.identity_kind()?);
                        // `SPREAD` — a contextual word, read only here, where
                        // it says how the identity just named is generated
                        // (ADR-0113 D1).
                        spread = self.eat_word("spread");
                    } else if graph.is_none() && self.eat_keyword(Keyword::In) {
                        graph = Some(self.name()?);
                    // `LAST WRITER WINS` / `REFUSE CONFLICTS` — contextual
                    // words, reserving nothing, for the reason
                    // `replication_class_clause` gives about `MULTI MASTER`:
                    // `last`, `wins`, `refuse` and `conflicts` are ordinary
                    // English that a stored script may already use as a name,
                    // and reserving one retroactively refuses every script that
                    // did. The phrase is what an operator already calls the
                    // thing, so what they type is what `INFO FOR` reads back.
                    } else if conflict.is_none() && self.eat_word("last") {
                        self.expect_word("writer", "`WRITER` after `LAST`")?;
                        self.expect_word("wins", "`WINS` after `LAST WRITER`")?;
                        conflict = Some(ConflictPolicy::LastWriterWins);
                    } else if conflict.is_none() && self.eat_word("refuse") {
                        self.expect_word("conflicts", "`CONFLICTS` after `REFUSE`")?;
                        conflict = Some(ConflictPolicy::Refuse);
                    // `SPLIT AT` — contextual words for the reason the conflict
                    // phrase gives: `split` and `at` are ordinary English a
                    // stored script may already use as names.
                    } else if split.is_none() && self.eat_word("split") {
                        self.expect_word("at", "`AT` and the identity a shard begins at")?;
                        split = Some(self.split_points()?);
                    // `PARTITION BY` — contextual words again, for the same
                    // reason (ADR-0096).
                    } else if partition.is_none() && self.eat_word("partition") {
                        self.expect_word("by", "`BY` and the field a record's identity begins with")?;
                        partition = Some(self.name()?);
                    // `EXPIRE [AFTER 7d]` — contextual for the reason the rest
                    // are: `expire` is a field a session table already has
                    // (ADR-0122 A1).
                    } else if expire.is_none() && self.eat_word("expire") {
                        expire = Some(self.table_expiry()?);
                    // `STORE ROW|COLUMN` — the clause head is the reserved
                    // `STORE` keyword (ADR-0118 reserved it for
                    // `ALTER STORE FINALIZE FORMAT`), so this arm eats the
                    // keyword where its neighbours eat a contextual word;
                    // `ROW` and `COLUMN` stay contextual idents .
                    } else if store.is_none() && self.eat_keyword(Keyword::Store) {
                        store = Some(if self.eat_word("row") {
                            crate::ast::StoreKind::Row
                        } else if self.eat_word("column") {
                            crate::ast::StoreKind::Column
                        } else {
                            return Err(self.error_here(
                                "`ROW` or `COLUMN` after `STORE` — the layout the records are kept in",
                            ));
                        });
                    // `PARTITIONS n BY field` — the hash partitioning of a
                    // columnar projection . `BY` is required here,
                    // unlike on a topic where a missing key reads as round
                    // robin: a columnar table's lanes are keyed or refused,
                    // never implicit.
                    } else if partitions.is_none() && self.eat_word("partitions") {
                        let count = self.positive_count(
                            "a whole number from 1 to 1048576 — the lanes the projection routes across",
                        )?;
                        self.expect_word("by", "`BY` and the field a record's lane is picked by")?;
                        partitions = Some((count, self.name()?.text));
                    } else {
                        break;
                    }
                }
                // A table with no columns has nothing to be strict about, and
                // the reader who wrote it wanted the other word. Refused rather
                // than quietly read as lenient, and the refusal names the word,
                // because "not allowed" without "write this instead" turns a
                // one-word fix into a search through the specification.
                //
                // An edge table is the exception, and it is not a special case
                // so much as the rule read properly: it declares no columns
                // because nobody writes `out` and `in` by hand, so it is not a
                // declaration with nothing in it — it is one whose fields the
                // store supplies. It keeps the lenient reading it had, because
                // an edge carries properties and none of them were ever
                // declared here.
                if columns.is_empty() && strictness.is_none() && edge.is_none() && graph.is_none() {
                    return Err(Error::TableWithoutColumns {
                        name: name.text.clone(),
                        span: name.span,
                    });
                }
                let schemafull = strictness.unwrap_or(!columns.is_empty());
                Ok(StatementKind::DefineTable {
                    name,
                    columns,
                    schemafull,
                    edge,
                    identity: identity.unwrap_or_default(),
                    graph,
                    split: split.unwrap_or_default(),
                    partition,
                    spread,
                    conflict,
                    expire,
                    store,
                    partitions,
                    if_not_exists,
                })
            }
            Some(Keyword::Space) => {
                self.advance();
                let if_not_exists = self.eat_if_not_exists()?;
                let name = self.name()?;
                // The clauses compose in any order, as the table's adjectives
                // do: each arm eats only when its word stands here, and the
                // loop ends at the first position none of them claimed.
                let mut memory = false;
                let mut limit = None;
                let mut split = None;
                loop {
                    let mut claimed = false;
                    if !memory && self.peek_keyword() == Some(Keyword::In) {
                        memory = self.space_in_memory()?;
                        claimed = true;
                    }
                    if limit.is_none() && self.peek_word("max") {
                        limit = self.space_bound()?;
                        claimed = true;
                    }
                    // `SPLIT AT` — contextual words for the reason the table's
                    // clause gives: `split` and `at` are ordinary English a
                    // stored script may already use as names.
                    if split.is_none() && self.eat_word("split") {
                        self.expect_word("at", "`AT` and the identity a shard begins at")?;
                        split = Some(self.split_points()?);
                        claimed = true;
                    }
                    if !claimed {
                        break;
                    }
                }
                if memory && limit.is_none() {
                    return Err(self.error_here(
                        "`MAX n` — a space held in memory states how many keys it may hold",
                    ));
                }
                Ok(StatementKind::DefineSpace {
                    name,
                    if_not_exists,
                    memory,
                    limit,
                    split: split.unwrap_or_default(),
                })
            }
            Some(Keyword::Graph) => {
                self.advance();
                let if_not_exists = self.eat_if_not_exists()?;
                Ok(StatementKind::DefineGraph {
                    name: self.name()?,
                    if_not_exists,
                })
            }
            Some(Keyword::Edge) => {
                self.advance();
                let if_not_exists = self.eat_if_not_exists()?;
                let name = self.name()?;
                // The graph is required and comes first, because an edge kind
                // that did not name one would be an edge table under a different
                // word — and the adjacency it writes has nowhere to live without
                // a graph id above the node.
                self.expect_keyword(Keyword::In, "`IN` and the graph the edge belongs to")?;
                let graph = self.name()?;
                self.expect_keyword(Keyword::From, "`FROM` and the table the edge leaves")?;
                let from = self.name()?;
                self.expect_keyword(Keyword::To, "`TO` and the table the edge reaches")?;
                let to = self.name()?;
                Ok(StatementKind::DefineEdge {
                    name,
                    graph,
                    from,
                    to,
                    if_not_exists,
                })
            }
            Some(Keyword::Bucket) => {
                self.advance();
                let if_not_exists = self.eat_if_not_exists()?;
                let name = self.name()?;
                Ok(StatementKind::DefineBucket {
                    name,
                    max: self.byte_ceiling()?,
                    if_not_exists,
                })
            }
            Some(Keyword::Collection) => {
                self.advance();
                let if_not_exists = self.eat_if_not_exists()?;
                let name = self.name()?;
                // After the name, where the table spelling also takes it. A
                // collection has no strictness word and no columns, so this is
                // the whole of what follows one.
                let identity = if self.eat_word("identity") {
                    self.identity_kind()?
                } else {
                    IdentityKind::default()
                };
                let expire = if self.eat_word("expire") {
                    Some(self.table_expiry()?)
                } else {
                    None
                };
                Ok(StatementKind::DefineCollection {
                    name,
                    identity,
                    expire,
                    if_not_exists,
                })
            }
            Some(Keyword::Index) => self.define_index(),
            Some(Keyword::Field) => self.define_field(),
            Some(Keyword::Analyzer) => self.define_analyzer(),
            Some(Keyword::Search) => self.define_search(),
            // Contextual, like `NODE` below: a table called `synonyms` keeps
            // its name everywhere but directly after `DEFINE`.
            _ if self.eat_word("synonyms") => self.define_synonyms(),
            _ if self.eat_word("stopwords") => self.define_stopwords(),
            Some(Keyword::User) => self.define_user(),
            // `NODE` and `REPLICA` are read as contextual words, for the reason
            // `INFO FOR STORE` gives: reserving a word takes a perfectly good
            // table and field name away from data that already exists, and
            // `DEFINE TABLE node` is not a name to spend. Nothing but a subject
            // can stand after `DEFINE`, so nothing here is ambiguous — and both
            // arms consume their word, so neither may `advance` again.
            _ if self.eat_word("node") => self.define_node(),
            _ if self.eat_word("replica") => self.define_replica(),
            // The cross-cluster half : `REPLICATION` is read whole, so the
            // arm above never eats it, and contextual like every subject here —
            // a table called `replication` keeps its name everywhere but
            // directly after `DEFINE`.
            _ if self.eat_word("replication") => self.define_replication(),
            // And a third contextual subject, on the same reasoning: `failover`
            // is a perfectly good name for a table somebody's data already uses.
            _ if self.eat_word("failover") => self.define_failover(),
            // And a fourth, on the same reasoning: `topology` is a name data
            // already uses, and nothing but a subject can stand after `DEFINE`.
            _ if self.eat_word("topology") => self.define_topology(),
            // And a fifth, on the same reasoning: `balancer` is a name data
            // already uses, and nothing but a subject can stand after `DEFINE`.
            _ if self.eat_word("balancer") => self.define_balancer(),
            // And a sixth, on the same reasoning: `pool` is a name data
            // already uses, and nothing but a subject can stand after `DEFINE`.
            _ if self.eat_word("pool") => self.define_pool(),
            // `KAFKA` qualifies the word rather than replacing it, and it is
            // contextual like every other subject here — special after `DEFINE`
            // and an ordinary identifier everywhere else, so a table called
            // `kafka` is still spellable.
            _ if self.eat_word("kafka") => {
                self.expect_word("consumer", "`CONSUMER` after `KAFKA`")?;
                self.define_consumer()
            }
            // The bare spelling is REFUSED rather than accepted, because the
            // word is being given to the queue: a `DEFINE CONSUMER` that kept
            // working would mean broker ingestion today and a claimant identity
            // later, and nothing in the statement would say which was meant.
            _ if self.peek_word("consumer") => Err(self.error_here(
                "`DEFINE KAFKA CONSUMER` — broker ingestion names its broker, \
                 and `CONSUMER` alone now belongs to a queue's readers",
            )),
            // Contextual for the reason `DEFINE INDEX … VECTOR` already gives:
            // a field called `vector` in a database of embeddings is not a name
            // to take away, and taking it away here would take it away
            // everywhere, since a reserved word is reserved in every position.
            _ if self.eat_word("vector") => self.define_vector(),
            // Contextual for the same reason, and with more at stake: `geo` is a
            // perfectly ordinary column name, and reserving it here would
            // reserve it everywhere.
            _ if self.eat_word("geo") => self.define_geo(),
            // Contextual for the same reason again, and here the reason is
            // strongest: `vault` is a perfectly ordinary table name in a
            // password manager's own schema, which is precisely the kind of
            // application this word exists for.
            _ if self.eat_word("vault") => self.define_vault(),
            // Contextual for the same reason as the rest of this run: `queue` is
            // an ordinary table name, and a store that had one before this word
            // existed keeps it.
            _ if self.eat_word("queue") => self.define_queue(),
            // `DEFINE TOPIC CONSUMER …` is told apart from a topic named
            // `consumer` by what follows the word (ADR-0087 §1).
            _ if self.eat_word("topic") => {
                if self.topic_consumer_follows(true) {
                    self.eat_word("consumer");
                    self.define_topic_consumer()
                } else {
                    self.define_topic()
                }
            }
            _ if self.eat_word("group") => self.define_group(),
            _ if self.eat_word("series") => self.define_series(),
            _ if self.eat_word("rollup") => self.define_rollup(),
            // Contextual for the same reason as the rest of this run: `view` is
            // an ordinary table name, and a store that had one before this word
            // existed keeps it.
            _ if self.eat_word("view") => self.define_view(),
            // Contextual, as `view` is: `event` is an ordinary table name.
            _ if self.eat_word("event") => self.define_event(),
            // Contextual, as `event` is: `feed` is an ordinary table name.
            _ if self.eat_word("feed") => self.define_feed(),
            // Contextual, as `event` is.
            _ if self.eat_word("param") => self.define_param(),
            _ => Err(self.error_here(
                "`NAMESPACE`, `DATABASE`, `TABLE`, `SPACE`, `BUCKET`, `INDEX`, `FIELD`, `ANALYZER`, `USER`, `NODE`, `REPLICA`, `KAFKA CONSUMER`, `VECTOR`, `GEO`, `VAULT`, `QUEUE`, `TOPIC`, `VIEW`, `EVENT` or `PARAM`",
            )),
        }
    }

    /// `DEFINE GEO places`
    ///
    /// A name and nothing else. Where `DEFINE VECTOR` requires two clauses
    /// because a store without them is not one, a geo store is complete as soon
    /// as it exists — so there is no clause to read, and adding an optional one
    /// later leaves every store written today parsing (Q-324).
    pub(super) fn define_geo(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        Ok(StatementKind::DefineGeo {
            name: self.name()?,
            if_not_exists,
        })
    }

    /// What follows `EXPIRE` in a table's declaration: nothing, or `AFTER` and
    /// the lifetime a created record gets (ADR-0122 A1).
    pub(super) fn table_expiry(&mut self) -> Result<TableExpiry> {
        let after = if self.eat_word("after") {
            Some(self.positive_duration(
                "a duration above zero, like `7d` — how long a new record lives",
            )?)
        } else {
            None
        };
        Ok(TableExpiry { after })
    }
}
