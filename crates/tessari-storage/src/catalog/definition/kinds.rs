//! The kinds a table can be, and how a kind is stored.

use super::{
    EdgeDeclaration, QueueDeclaration, SeriesDeclaration, VaultDeclaration, VectorDeclaration,
    ViewDeclaration,
};
use crate::error::{Error, Result};

/// Which engine's rules a table plays by.
///
/// The four are exclusive by construction, which is the whole point of the type:
/// the booleans it replaces described eight states, four of them meaningless,
/// and only the grammar kept them apart because each kind is reached by a
/// different statement. A parser is the wrong place for an invariant about what
/// a stored table *is* — nothing stops a later caller building the definition by
/// hand, and a table that is both a bucket and an edge would take the bucket's
/// refusal of `CREATE` and the edge's endpoint indexes into one record with
/// nothing anywhere in an error state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TableKind {
    /// Records with named fields — `DEFINE TABLE`.
    #[default]
    Table,
    /// Records that are one value rather than named fields — `DEFINE
    /// COLLECTION`.
    ///
    /// Kept distinct from a schemaless table, which behaves alike, because
    /// `INFO FOR TABLE` must answer with the word that created the thing: a
    /// round trip emitting `DEFINE TABLE … SCHEMALESS` for a collection would
    /// re-execute happily while losing the word.
    Collection,
    /// Files: records holding metadata the store fills in from bytes it holds,
    /// with the bytes in a companion table nothing can name — `DEFINE BUCKET`.
    ///
    /// `CREATE`, `UPDATE` and `SET` against one are refused, because metadata a
    /// caller writes by hand is metadata that can lie, and a size disagreeing
    /// with the bytes is a lie nothing would ever catch. Reading is not
    /// restricted: listing a bucket is `SELECT * FROM media`, a query rather
    /// than an API call, which is the point of a bucket being a table at all
    /// (ADR-0011).
    ///
    /// The `u64` is the largest file the bucket accepts, in bytes, when one was
    /// declared. It rides **on** the kind rather than beside it for the reason
    /// [`TableKind::Edge`]'s pair does: a pair of fields would make "carries a
    /// ceiling but is not a bucket" representable, and the kind exists to
    /// abolish exactly that state. Optional inside the variant because a bucket
    /// with no ceiling is still a bucket — unlike a vector store, which without
    /// a width is not a vector store.
    ///
    /// It is a count of bytes and not a size literal because the language has
    /// no size literal: digits touching a letter are a duration whatever the
    /// letter is, so `5MB` is a duration with an unrecognised unit. The grammar
    /// therefore reads `MAX 5242880`.
    Bucket(Option<u64>),
    /// Edges: records carrying `out` and `in` record references, each with an
    /// index — `DEFINE TABLE … EDGE`.
    ///
    /// The indexes are what make traversal a range read rather than a scan,
    /// without the caller having had to know to declare them. `RELATE` checks
    /// the kind before writing, because an edge nothing can traverse to is
    /// worse than a refusal.
    ///
    /// `Some` is `DEFINE TABLE … EDGE FROM a TO b`, which refuses a link whose
    /// endpoints it does not declare; `None` is the bare `EDGE`, which accepts a
    /// link between any two records. The clause is optional so that a store
    /// discovering its shape as it goes still has a spelling for that, and so
    /// that every edge table declared before the clause existed keeps its
    /// meaning (Q-297).
    ///
    /// The declaration rides **on** the kind rather than sitting beside it in a
    /// second field, because a pair would be two places holding one fact and
    /// would make "declares a pair but is not an edge table" representable — the
    /// state this type was introduced one change earlier to abolish (C1).
    Edge(Option<EdgeDeclaration>),
    /// Vectors: records holding one vector of a declared width, with the index
    /// that searches them built by the declaration — `DEFINE VECTOR`.
    ///
    /// The fifth kind, and it is a kind rather than three separate statements
    /// for the reason [`TableKind::Collection`] is one: `INFO` must answer with
    /// the word that created the thing. A store reported as a collection with a
    /// field and an index re-executes happily and loses the fact that the three
    /// belong together — which is the whole of what the word promises, since a
    /// width with no index searches nothing, an index with no width admits a row
    /// of the wrong shape, and neither without `REQUIRED` admits a record with
    /// no vector at all.
    Vector(VectorDeclaration),
    /// Places: records holding one geometry, with the spatial index that finds
    /// them built by the declaration — `DEFINE GEO`.
    ///
    /// The sixth kind, on the same test the fifth passed: `INFO` must answer
    /// with the word that created the thing, and a store reported as a
    /// collection carrying a geometry field and a spatial index re-executes
    /// happily while losing the fact that the three belong together. A field
    /// with no index makes every place query a scan, an index with no declared
    /// field indexes nothing, and neither without `REQUIRED` admits a record
    /// with no geometry at all — which is a record a place store has no way to
    /// answer for.
    ///
    /// Unlike [`TableKind::Vector`] it carries no declaration, because it has
    /// nothing to declare. A vector store without a width and a distance is not
    /// a vector store; a geo store is complete as soon as it exists. Whether it
    /// should narrow the shape it holds is decided against it (Q-324): the shape
    /// a `Closest` read needs is already enforced where that read happens, and a
    /// store narrowed to points could not express a table of regions — which
    /// `records_in_region` serves correctly today.
    Geo,
    /// Secrets: records whose declared `SECRET` fields are stored sealed, and
    /// which no generic read can reach — `DEFINE VAULT`.
    ///
    /// The seventh kind, and the first whose reason is not "`INFO` must answer
    /// with the word that created the thing". That test is passed here too, but
    /// it is not why the kind exists: a vault is the one store where the
    /// *absence* of a capability is the capability. `SELECT` is refused, an
    /// index on a secret field is refused, a filter and an ordering on one are
    /// refused, and each refusal is reachable only because the kind is on the
    /// definition where every path can see it.
    ///
    /// It carries the vault's key, sealed under the store's master key, for the
    /// reason [`TableKind::Vector`] carries its declaration: a store that has
    /// one is not the same object as a store that does not, and a key sitting
    /// in a field beside the kind would make "carries a key but is not a vault"
    /// representable — which is a table whose records nothing can ever open.
    ///
    /// Dropping the definition therefore destroys the key, and destroying the
    /// key is the deletion. Every record in the vault becomes unopenable in
    /// every backup, snapshot and replica that will ever be restored — which is
    /// the only deletion claim a store like this can honestly make, since a row
    /// delete is a statement about the live table and not about the data.
    Vault(VaultDeclaration),
    /// Work waiting to be done, handed out under a hold that lapses — `DEFINE
    /// QUEUE`.
    ///
    /// The hold is not a lease and there is no lease manager, deliberately. A
    /// claim is an ordinary **write**, so it is sequenced into the log and
    /// replicated by the mechanism every other write uses; the instant it lapses
    /// is computed once by the session that takes it and **written into the
    /// record**, the same rule `time::now()` already follows so that a replica
    /// applies what was written rather than asking its own clock; and expiry is
    /// a comparison a later reader performs rather than an event anything
    /// raises. Those three together are why the queue holds no state outside the
    /// log and therefore asks nothing of a cluster that an ordinary write does
    /// not already ask.
    ///
    /// It carries its declaration for the reason [`TableKind::Vector`] does: a
    /// timeout in a field beside the kind would make "carries a timeout but is
    /// not a queue" representable, which is the state this type abolishes.
    Queue(QueueDeclaration),
    /// A name for a read, holding no records of its own — `DEFINE VIEW`.
    ///
    /// The ninth kind, and the first that is not a store at all. Every kind
    /// before it answers *what may be done to these records*; this one has no
    /// records, so what it changes is where the records come from: a statement
    /// naming a view is rewritten to carry the view's read before anything
    /// resolves a name, and the read then runs as an ordinary materialised
    /// source.
    ///
    /// That rewrite happens **before the grant check**, which is the whole of
    /// why this is a kind and not a catalog entity of its own. A grant names a
    /// [`tessari_types::TableId`], so a view outside the table namespace would
    /// need a second permission system; inside it, a view cannot shadow a table
    /// (one name reservation answers both) and a caller reading through one is
    /// checked against the tables the view actually reads.
    ///
    /// It carries its read for the reason [`TableKind::Vector`] carries its
    /// declaration: a read in a field beside the kind would make "carries a read
    /// but is not a view" representable, which is the state this type abolishes.
    View(ViewDeclaration),
    /// Records that age out — `DEFINE SERIES`.
    ///
    /// The tenth kind, and the one that makes Time an engine rather than a
    /// convention. What it adds is not a way to store an instant — every table
    /// could already do that — but a **floor**: past it a record is not in the
    /// answer, whether or not its bytes have been removed yet.
    ///
    /// The floor is a position in the key rather than a predicate over a field,
    /// and that is the whole construction. A series table's identity is
    /// [`tessari_types::IdentityKind::Uuid`], fixed by the kind, because UUID
    /// version 7 carries the millisecond in its leading six bytes big-endian —
    /// so a read under a retention does not filter, it **starts later**. An
    /// ordinary table cannot offer that: its counter identity carries no time at
    /// all, and an age rule over one of its datetime fields is re-tested per
    /// record.
    ///
    /// The comparison is performed by the reader, not raised as an event, which
    /// is the rule [`TableKind::Queue`]'s hold already follows. One consequence
    /// is worth stating where it cannot be missed: **the removal is a separate
    /// act from the hiding.** Correctness comes from the read, so a removal pass
    /// that lags, is throttled or never runs costs storage and never an answer.
    Series(SeriesDeclaration),
    /// A key-value space, with the limit it declared if any — `DEFINE SPACE`
    /// (G036). Its own kind so `INFO` writes it back with its own word; see
    /// [`super::space`].
    Space(crate::catalog::space::SpaceDeclaration),
    /// An append-only order of messages — `DEFINE TOPIC`. See
    /// [`super::topic`].
    Topic(crate::catalog::topic::TopicDeclaration),
}

/// The parts of a stored table entry that together name its kind.
///
/// Grouped rather than passed as eight arguments, for the reason `TableShape`
/// already exists a few types above: four of them are `bool`, so the compiler
/// cannot tell one from another and a transposition produces a table of the
/// wrong kind with nothing anywhere in an error state. Reading these out of a
/// catalog record is the one place they all appear together.
#[derive(Debug, Clone, Default)]
pub struct StoredKind {
    /// The `edge` flag.
    pub edge: bool,
    /// The `bucket` flag.
    pub bucket: bool,
    /// The `collection` flag.
    pub collection: bool,
    /// The `geo` flag.
    pub geo: bool,
    /// An edge table's declared endpoints.
    pub endpoints: Option<EdgeDeclaration>,
    /// A vector store's declaration.
    pub vector: Option<VectorDeclaration>,
    /// A vault's wrapped key.
    pub vault: Option<VaultDeclaration>,
    /// A queue's timeout and attempt ceiling.
    pub queue: Option<QueueDeclaration>,
    /// A view's read.
    pub view: Option<ViewDeclaration>,
    /// A series table's retention.
    pub series: Option<SeriesDeclaration>,
    /// A space's declaration, when the table is one (G036).
    pub space: Option<crate::catalog::space::SpaceDeclaration>,
    /// A topic's declaration, when the table is one (G037).
    pub topic: Option<crate::catalog::topic::TopicDeclaration>,
    /// A bucket's size ceiling.
    pub ceiling: Option<u64>,
}

impl TableKind {
    /// The kind a stored definition's three flags describe.
    ///
    /// A catalog entry written before the kind existed carries the flags, and a
    /// build without the kind still writes them, so this is the only direction
    /// that needs a decision — and the decision is to **refuse** a combination
    /// rather than to prefer one of them. A stored table claiming to be both a
    /// bucket and an edge is not a table this build can serve correctly under
    /// either reading, and picking one would put a store into the state the
    /// kind exists to make unrepresentable. It is the same contract
    /// `identity_kind` already keeps for a word it does not recognise.
    ///
    /// An entry with no flags set is a plain table, which is what every entry
    /// written before any of these flags existed is.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when more than one kind is claimed.
    pub fn from_parts(stored: StoredKind) -> Result<Self> {
        let StoredKind {
            edge,
            bucket,
            collection,
            geo,
            endpoints,
            vector,
            vault,
            queue,
            view,
            series,
            space,
            topic,
            ceiling,
        } = stored;
        // A topic is pulled out first for the reason a space is below.
        if let Some(declared) = topic {
            return match (
                edge, bucket, collection, geo, endpoints, vector, &vault, &queue, &view, &series,
                space, ceiling,
            ) {
                (false, false, false, false, None, None, None, None, None, None, None, None) => {
                    Ok(Self::Topic(declared))
                }
                _ => Err(Error::CatalogMalformed {
                    entity: "table",
                    field: "kind",
                    found: "more than one kind",
                }),
            };
        }
        // A space sets no flag, and is pulled out first among the declarations
        // for the reason each of them is: the arms below stay what they were.
        if let Some(declared) = space {
            return match (
                edge, bucket, collection, geo, endpoints, vector, &vault, &queue, &view, &series,
                ceiling,
            ) {
                (false, false, false, false, None, None, None, None, None, None, None) => {
                    Ok(Self::Space(declared))
                }
                _ => Err(Error::CatalogMalformed {
                    entity: "table",
                    field: "kind",
                    found: "more than one kind",
                }),
            };
        }
        // A vault is read first and alone. Every other arm below distinguishes
        // kinds that differ in what a caller may do; this one differs in
        // whether the records can be read at all, so a definition that both
        // carries a vault key and claims another kind is not a puzzle to
        // resolve by precedence — it is a catalog entry that must not be
        // honoured in either direction.
        if let Some(declared) = vault {
            return match (
                edge, bucket, collection, geo, endpoints, vector, &queue, ceiling,
            ) {
                (false, false, false, false, None, None, None, None) => Ok(Self::Vault(declared)),
                _ => Err(Error::CatalogMalformed {
                    entity: "table",
                    field: "kind",
                    found: "more than one kind",
                }),
            };
        }
        // A series sets no flag either, and is pulled out here for the reason
        // the queue below it is: the arms already written stay the exhaustive
        // statement they are.
        if let Some(declared) = series {
            return match (
                edge, bucket, collection, geo, endpoints, vector, &queue, &view, ceiling,
            ) {
                (false, false, false, false, None, None, None, None, None) => {
                    Ok(Self::Series(declared))
                }
                _ => Err(Error::CatalogMalformed {
                    entity: "table",
                    field: "kind",
                    found: "more than one kind",
                }),
            };
        }
        // A queue sets no flag either, so it reaches the match below as a plain
        // table carrying a declaration — and it is pulled out here rather than
        // added as a ninth tuple element so that the arms already written keep
        // reading as the exhaustive statement they are.
        if let Some(declared) = queue {
            return match (
                edge, bucket, collection, geo, endpoints, vector, &view, ceiling,
            ) {
                (false, false, false, false, None, None, None, None) => Ok(Self::Queue(declared)),
                _ => Err(Error::CatalogMalformed {
                    entity: "table",
                    field: "kind",
                    found: "more than one kind",
                }),
            };
        }
        // A view sets no flag either, and is pulled out here for the reason the
        // queue is: the tuple match below is an exhaustive statement about the
        // kinds that *are* flags, and growing it by one element per declaration
        // would make every arm harder to read to say nothing new.
        if let Some(declared) = view {
            return match (edge, bucket, collection, geo, endpoints, vector, ceiling) {
                (false, false, false, false, None, None, None) => Ok(Self::View(declared)),
                _ => Err(Error::CatalogMalformed {
                    entity: "table",
                    field: "kind",
                    found: "more than one kind",
                }),
            };
        }
        match (edge, bucket, collection, geo, endpoints, vector, ceiling) {
            (false, false, false, false, None, None, None) => Ok(Self::Table),
            (true, false, false, false, endpoints, None, None) => Ok(Self::Edge(endpoints)),
            // The ceiling rides through with the flag, so a bucket declared
            // before the clause existed arrives with `None` and is the
            // unbounded bucket it has always been. A ceiling on any other kind
            // falls to the refusal below, because nothing else has a file to
            // measure it against.
            (false, true, false, false, None, None, ceiling) => Ok(Self::Bucket(ceiling)),
            (false, false, true, false, None, None, None) => Ok(Self::Collection),
            (false, false, false, true, None, None, None) => Ok(Self::Geo),
            // A vector store sets no flag, so it arrives here as a plain table
            // carrying a declaration. Any flag beside that declaration is two
            // kinds claimed at once and is refused with the rest.
            (false, false, false, false, None, Some(declared), None) => Ok(Self::Vector(declared)),
            _ => Err(Error::CatalogMalformed {
                entity: "table",
                field: "kind",
                found: "more than one kind",
            }),
        }
    }
}
