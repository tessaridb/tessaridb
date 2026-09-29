//! DEFINE, and the kinds of table and index it declares.

mod declared;
use super::Parser;
use tessari_types::{ConflictPolicy, IdentityKind, RecordId};

use crate::ast::{EdgeClause, Name, StatementKind};
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
                Ok(StatementKind::DefineNamespace {
                    name,
                    if_not_exists,
                    replication,
                    class,
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
                loop {
                    if strictness.is_none() && self.eat_keyword(Keyword::Schemafull) {
                        strictness = Some(true);
                    } else if strictness.is_none() && self.eat_keyword(Keyword::Schemaless) {
                        strictness = Some(false);
                    } else if edge.is_none() && self.eat_keyword(Keyword::Edge) {
                        edge = Some(self.edge_clause()?);
                    } else if identity.is_none() && self.eat_word("identity") {
                        identity = Some(self.identity_kind()?);
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
                    conflict,
                    if_not_exists,
                })
            }
            Some(Keyword::Space) => {
                self.advance();
                let if_not_exists = self.eat_if_not_exists()?;
                let name = self.name()?;
                Ok(StatementKind::DefineSpace {
                    name,
                    if_not_exists,
                    limit: self.space_bound()?,
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
                Ok(StatementKind::DefineCollection {
                    name,
                    identity,
                    if_not_exists,
                })
            }
            Some(Keyword::Index) => self.define_index(),
            Some(Keyword::Field) => self.define_field(),
            Some(Keyword::Analyzer) => self.define_analyzer(),
            Some(Keyword::User) => self.define_user(),
            // `NODE` and `REPLICA` are read as contextual words, for the reason
            // `INFO FOR STORE` gives: reserving a word takes a perfectly good
            // table and field name away from data that already exists, and
            // `DEFINE TABLE node` is not a name to spend. Nothing but a subject
            // can stand after `DEFINE`, so nothing here is ambiguous — and both
            // arms consume their word, so neither may `advance` again.
            _ if self.eat_word("node") => self.define_node(),
            _ if self.eat_word("replica") => self.define_replica(),
            // And a third contextual subject, on the same reasoning: `failover`
            // is a perfectly good name for a table somebody's data already uses.
            _ if self.eat_word("failover") => self.define_failover(),
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
            _ => Err(self.error_here(
                "`NAMESPACE`, `DATABASE`, `TABLE`, `SPACE`, `BUCKET`, `INDEX`, `FIELD`, `ANALYZER`, `USER`, `NODE`, `REPLICA`, `KAFKA CONSUMER`, `VECTOR`, `GEO`, `VAULT`, `QUEUE`, `TOPIC` or `VIEW`",
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
}
