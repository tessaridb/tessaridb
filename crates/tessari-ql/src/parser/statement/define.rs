//! DEFINE, and the kinds of table and index it declares.

use super::Parser;
use tessari_types::{ConflictPolicy, Filter, IdentityKind, RecordId};

use crate::ast::{EdgeClause, Name, StatementKind};
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct, Token};

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
            _ if self.eat_word("topic") => self.define_topic(),
            _ if self.eat_word("series") => self.define_series(),
            // Contextual for the same reason as the rest of this run: `view` is
            // an ordinary table name, and a store that had one before this word
            // existed keeps it.
            _ if self.eat_word("view") => self.define_view(),
            _ => Err(self.error_here(
                "`NAMESPACE`, `DATABASE`, `TABLE`, `SPACE`, `BUCKET`, `INDEX`, `FIELD`, `ANALYZER`, `USER`, `NODE`, `REPLICA`, `KAFKA CONSUMER`, `VECTOR`, `GEO`, `VAULT`, `QUEUE`, `TOPIC` or `VIEW`",
            )),
        }
    }

    /// `DEFINE VECTOR embeddings DIMENSION 768 DISTANCE cosine`
    ///
    /// Both clauses are required and neither has a default. The width, because
    /// declaring it is the whole capability the word adds. The distance, for the
    /// reason the index already records: a default would silently decide which
    /// queries the store can serve, and a graph built for one distance
    /// approximates that distance and no other.
    ///
    /// They are read in a fixed order rather than in any order. Two clauses is
    /// too few to be worth an order-free reader, and a fixed order is what makes
    /// the statement read the same way in every store that has one.
    pub(super) fn define_vector(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        if !self.eat_word("dimension") {
            return Err(self.error_here("`DIMENSION` and how wide every vector here is"));
        }
        let dimension = self.vector_dimension()?;
        if !self.eat_word("distance") {
            return Err(self.error_here("`DISTANCE` and the distance its index is built with"));
        }
        Ok(StatementKind::DefineVector {
            name,
            dimension,
            distance: self.name()?,
            if_not_exists,
        })
    }

    /// `DEFINE SERIES readings RETAIN 30d`
    ///
    /// The retention is a literal duration rather than an expression, on
    /// [`Self::define_queue`]'s rule and for its reason: a floor a bound value
    /// could set is a floor a caller could move, and this one is meant to be
    /// readable in the statement that declared it.
    pub(super) fn define_series(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        if !self.eat_word("retain") {
            return Err(self.error_here("`RETAIN` and how far back the table answers"));
        }
        let expected = "a duration, like `30d` or `12h`";
        let Some(Token::Duration(written)) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let retain = *written;
        let at = self.span_here();
        self.advance();
        // Refused here for the reason a queue's zero timeout is: a retention of
        // no length is not a retention, it is a table that answers with nothing,
        // and that is a mistake in the statement rather than a configuration.
        if retain.seconds() < 0 || (retain.seconds() == 0 && retain.nanos() == 0) {
            return Err(Error::EmptyRetention {
                written: retain.to_literal(),
                span: at,
            });
        }
        Ok(StatementKind::DefineSeries {
            name,
            retain,
            if_not_exists,
        })
    }

    /// `DEFINE VIEW active AS SELECT * FROM users WHERE active = true`
    ///
    /// The read runs to the end of the statement. No parentheses, because there
    /// is nothing to disambiguate: a view holds exactly one `SELECT` and it is
    /// everything after `AS`.
    ///
    /// # Parsed to validate, kept as text to store
    ///
    /// The read is parsed here so that a view which is not one `SELECT` is
    /// refused where somebody wrote it rather than on whatever read first names
    /// it, and then the **source text** is what the statement carries — sliced
    /// by the read's own span. Rendering the parsed tree back would store a
    /// different statement that happens to mean the same thing, and a reader
    /// comparing what they wrote against what `INFO` reports should find them
    /// equal.
    ///
    /// Nothing is resolved: the tables the read names need not exist yet, the
    /// same rule a field's `DEFAULT` follows. The alternative would make the
    /// order of a provisioning script load-bearing.
    pub(super) fn define_view(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        if !self.eat_keyword(Keyword::As) {
            return Err(self.error_here("`AS` and the read this name means"));
        }
        if self.peek_keyword() != Some(Keyword::Select) {
            return Err(self.error_here("`SELECT` — a view is a read"));
        }
        let read = self.select_statement()?;
        Ok(StatementKind::DefineView {
            name,
            read: self.source[read.span.start..read.span.end].to_owned(),
            if_not_exists,
        })
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

    /// `DEFINE INDEX by_email ON users FIELDS email, name UNIQUE`
    pub(super) fn define_index(&mut self) -> Result<StatementKind> {
        self.advance();
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        self.expect_keyword(Keyword::On, "`ON` and the table the index reads")?;
        let table = self.table_ref()?;
        self.expect_keyword(Keyword::Fields, "`FIELDS` and the fields to index")?;

        let mut fields = vec![self.field_path()?];
        while self.eat_punct(Punct::Comma) {
            fields.push(self.field_path()?);
        }
        // **One** marker. `UNIQUE` says how entries collide, `SEARCH` says the
        // entries are terms, `VECTOR` says they are a graph, `SPATIAL` says they
        // are cells — four different index kinds wearing four flags, of
        // which at most one can be true. Accepting two used to be possible and
        // the first one checked simply won, so `UNIQUE SEARCH` was an index
        // whose uniqueness was silently ignored. Adding a third made that
        // inconsistency a thing to answer rather than inherit.
        /// Which of the four an index is.
        enum Marker {
            Unique,
            Search,
            Spatial,
            /// With the distance its graph is built for, which is required —
            /// a default would silently decide which queries the index serves.
            Vector(crate::Name),
        }
        let mut kind: Option<Marker> = None;
        loop {
            let held = if self.eat_keyword(Keyword::Unique) {
                Marker::Unique
            } else if self.eat_keyword(Keyword::Search) {
                Marker::Search
            } else if self.eat_word("spatial") {
                // Contextual for the same reason `vector` is: a field called
                // `spatial` is not a name to take away from a caller.
                Marker::Spatial
            } else if self.eat_word("vector") {
                // Contextual, like `order` and `fetch`: a field called `vector`
                // in a database of embeddings is not a name to take away.
                Marker::Vector(self.name()?)
            } else {
                break;
            };
            if kind.is_some() {
                return Err(self.error_here("one index kind, not two"));
            }
            kind = Some(held);
        }
        // An index over `tags[*]` is a **multikey** index: one entry per element
        // rather than one per record. Three shapes are refused, each naming its
        // own reason — a caller told "unexpected token" would go looking for a
        // typo in a statement that has none.
        let mut several = fields.iter().filter(|field| field.path.is_several());
        if let Some(field) = several.next() {
            match &kind {
                Some(Marker::Unique) => {
                    return Err(Error::SeveralInAUniqueIndex { span: field.span });
                }
                Some(Marker::Search | Marker::Vector(_) | Marker::Spatial) => {
                    return Err(Error::SeveralInAnAnalysedIndex { span: field.span });
                }
                None => {}
            }
            if let Some(second) = several.next() {
                return Err(Error::SeveralRoutesInOneIndex { span: second.span });
            }
        }
        Ok(StatementKind::DefineIndex {
            name,
            table,
            fields,
            unique: matches!(kind, Some(Marker::Unique)),
            search: matches!(kind, Some(Marker::Search)),
            spatial: matches!(kind, Some(Marker::Spatial)),
            vector: match kind {
                Some(Marker::Vector(distance)) => Some(distance),
                _ => None,
            },
            if_not_exists,
        })
    }

    /// `DEFINE ANALYZER simple FILTERS lowercase, ascii`
    ///
    /// The tokenizer is not named because there is one: splitting on
    /// non-alphanumeric boundaries is what every filter chain assumes
    /// underneath it, and a knob with one setting is a knob nobody should have
    /// to read about.
    pub(super) fn define_analyzer(&mut self) -> Result<StatementKind> {
        self.advance();
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        let mut filters = Vec::new();
        if self.eat_keyword(Keyword::Filters) {
            filters.push(self.filter()?);
            while self.eat_punct(Punct::Comma) {
                filters.push(self.filter()?);
            }
        }
        Ok(StatementKind::DefineAnalyzer {
            name,
            filters,
            if_not_exists,
        })
    }

    /// One named filter.
    pub(super) fn filter(&mut self) -> Result<Filter> {
        let Some(Token::Ident(word)) = self.peek() else {
            return Err(self.error_here("a filter name"));
        };
        let Some(filter) = Filter::parse(word) else {
            return Err(self.error_here("a filter name"));
        };
        self.advance();
        Ok(filter)
    }
}
