//! A read: what it selects, from where, in what order, and under which guards.

use super::{Direction, Expr, FieldPath, Identity, Name, RecordTarget, TableRef};
use crate::token::Span;
use tessari_types::{Duration, Number};

/// A read of records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Select {
    /// Which values each record answers with.
    pub projection: Projection,
    /// The routes `*` must not contribute, when `OMIT` named any.
    ///
    /// Empty means the clause was not written. It subtracts from what the star
    /// put there and from nothing else: a value written out by name was asked
    /// for explicitly, so removing it would be answering a question nobody
    /// asked, and the parser refuses the clause where there is no star to
    /// subtract from rather than accepting one that does nothing.
    ///
    /// A route rather than a name, because the field to leave out may be inside
    /// the record: `OMIT address.postcode` keeps the address.
    pub omit: Vec<FieldPath>,
    /// Which access path the statement resolves to.
    pub from: Source,
    /// Whether `WITHOUT SCAN GUARD` was written, lifting the planner's veto.
    ///
    /// The guard it lifts is a **policy** and not a measurement: an index is
    /// served when it can produce at most half the table, and half is a
    /// threshold this store chose rather than a number a cost model produced.
    /// `plan::worth_serving` counts with a bounded probe, so what can be wrong
    /// here is the threshold and never the count — which is why the clause is
    /// spelled as lifting a guard rather than as overriding an estimate.
    ///
    /// It lifts the veto and does **not** choose the path. An override naming an
    /// index would be a router, and a router owes answers to every question a
    /// router raises: what an inapplicable named index does to the predicate,
    /// how it composes with ranking, what `EXPLAIN` then reports. This is one
    /// flag reaching one function — the ranking still chooses, an inapplicable
    /// index still changes nothing, and the worst case of misuse is the
    /// behaviour that shipped before the guard existed.
    ///
    /// A separate clause rather than a word on `USING INDEX`, deliberately: that
    /// clause is an assertion about what the read did, and a modifier turning it
    /// into an instruction would be a pun a reader can miss. This one cannot be
    /// missed.
    ///
    /// **Its removal condition, recorded at birth** (Q-494): it exists because
    /// the threshold is a policy, and it is retired when the planner acquires a
    /// cost model or statistics that make the policy unnecessary. A hint with no
    /// recorded removal condition is scar tissue — it freezes plans against a
    /// planner that has since improved, and nobody dares remove it because
    /// nobody remembers why it is there.
    pub lift_scan_guard: bool,
    /// Where `ONLY` was written, when it was.
    ///
    /// The clause is an **assertion by the author** that at most one record
    /// answers, and the answer is shaped to match: the record itself rather than
    /// a list holding it, so that a caller reading one thing does not unwrap a
    /// list of one everywhere.
    ///
    /// Its span rather than a bare `bool` because the refusal points at the word
    /// — the author either meant a different read or did not mean the word, and
    /// both are decided by looking at it.
    ///
    /// Nothing about it is checked before the read runs. A parse-time rule would
    /// have to say which sources can answer with one, and it cannot know:
    /// `FROM ONLY users WHERE email = $e` is the commonest correct use of the
    /// clause and carries no bound the parser can see, because the uniqueness it
    /// rests on lives in the schema and in the data.
    pub only: Option<Span>,
    /// The routes whose record references are followed before anything else
    /// looks at the record.
    ///
    /// Empty means the clause was not written. It is applied **before** the
    /// projection and the ordering, so `SELECT author.name … FETCH author` and
    /// `ORDER BY author.name` both see the record rather than the reference —
    /// which is the only ordering that makes the clause useful for the
    /// statements that want it.
    pub fetch: Vec<FieldPath>,
    /// The route whose array is opened into one record per element, when
    /// `SPLIT ON` named one.
    ///
    /// A route rather than a name, like `OMIT` and `FETCH`, so the array may be
    /// inside the record: `SPLIT ON address.tags`.
    ///
    /// Applied **after** `FETCH` and before everything that groups, projects or
    /// sorts — after the fetch because a reference resolved once and then opened
    /// is the same answer as one opened and then resolved n times, and before
    /// the rest because every one of them counts records and the split is what
    /// decides how many there are.
    ///
    /// One route and not a list. Two would be a cartesian product, which is a
    /// different question and should have to say so.
    pub split: Option<FieldPath>,
    /// The keys the records are grouped by, when the read groups.
    ///
    /// Empty means no grouping — which is not the same as no aggregate:
    /// `SELECT count(*) FROM users` folds every record into one group without
    /// naming a key, because the commonest question the language can be asked
    /// should not need a clause that means nothing.
    ///
    /// An **expression**, not a path, so a window is sayable:
    /// `GROUP BY time::bucket(at, 1h)`. A bare name still reads as a route into
    /// the record — the same reading `WHERE` and `ORDER BY` give it — so
    /// `GROUP BY city` means what it always did.
    pub group: Vec<Expr>,
    /// `FILL <mode> FROM <start> TO <end>` after a windowed `GROUP BY` — one row
    /// per window of the stated range (ADR-0088 §2).
    pub fill: Option<Fill>,
    /// `LATEST BY <field>` — one record per value of the field, the newest,
    /// on a series (ADR-0088 §3).
    pub latest: Option<FieldPath>,
    /// The keys the answer is sorted by, in order of significance.
    ///
    /// Under [`Select::fusion`] these are the fused read's branches rather than
    /// sort keys in order of significance: each ranks the records on its own and
    /// the ranks are what is combined.
    pub order: Vec<Ordering>,
    /// `ORDER BY FUSE (…)`: the branches in `order` are fused by rank.
    ///
    /// Beside `order` rather than a variant of it, so everything that walks the
    /// keys — binding, reach, views, rendering — reads a fused read's branches
    /// exactly as it reads sort keys. What must NOT treat them as sort keys is
    /// every path that serves or bounds an order by its first key, and each of
    /// those declines when this is set.
    pub fusion: Option<Fusion>,
    /// The record the answer resumes after, when `AFTER` named one.
    ///
    /// A cursor: the page begins at the first record that sorts **strictly
    /// after** this one in the answer's own order. A record identity rather than
    /// an opaque token because the caller already holds it — the answer carries
    /// the identity of every record in it — so the clause needs no new return
    /// channel, no token format, and no version of one.
    ///
    /// **It supplies the order it resumes.** With an `ORDER BY` that is the
    /// order written; with none, it is the store's own key order, which is why a
    /// cursor read that names no order still answers identity-ascending rather
    /// than in whatever order the source happened to produce. A cursor without
    /// an order to resume would be a filter on a sequence nobody promised.
    ///
    /// A `START` beside it is refused where the statement is read: an offset and
    /// a cursor are two answers to the same question, and accepting both would
    /// make one of them silently lose.
    ///
    /// Boxed where every other field of this struct is inline, because a record
    /// target is one of the larger things the grammar holds and a cursor is
    /// absent from very nearly every statement ever parsed. Inline it made
    /// `Select` the outlier variant of [`Statement`] — every statement of every
    /// kind paying for a clause almost none of them write.
    pub after: Option<Box<RecordTarget>>,
    /// Whether the caller will accept an approximate ordering.
    ///
    /// **Permission, not a demand.** Every index in this store may change what a
    /// read costs and none may change what it answers — except a vector index,
    /// whose graph returns the neighbours a walk found and cannot show it missed
    /// none. So the exception is written in the statement: a read that does not
    /// say this gets the exact scan, and one that does may be served by the
    /// graph if there is one. With no such index it is still exact, which is
    /// better than what was asked for; the reported access path says which.
    ///
    /// An `Option` and not a `bool` beside a separate budget field, because the
    /// budget is meaningless without the permission: an exact scan has nothing to
    /// spend. Kept as one value so *"an effort with no approximation"* is
    /// unrepresentable rather than merely unreachable — the same reasoning
    /// `TableKind` records, and for the same reason, since a `Select` is built by
    /// the query builder as well as by the parser.
    pub approximate: Option<Approximation>,
    /// How many records to pass over before answering.
    pub start: Option<u64>,
    /// How many to answer with at most.
    pub limit: Option<u64>,
    /// What the author expects the read to have done, when they said.
    ///
    /// `None` is the ordinary case: the statement asks a question and the store
    /// answers it however it can.
    pub using: Option<Using>,
    /// How long the read may take before it is refused.
    ///
    /// `None` is the ordinary case: a read takes as long as it takes.
    pub timeout: Option<Timeout>,
    /// The point in the store's history the read answers from.
    ///
    /// `None` is the ordinary case: the read answers from the committed tail.
    pub version: Option<Version>,
    /// How far behind the node answering this read is allowed to be.
    ///
    /// `None` is the ordinary case: a read has no tolerance because it is
    /// answered here, and a node's own answer is never stale relative to itself.
    pub staleness: Option<Staleness>,
    /// Which nodes this read admits as its answerer.
    ///
    /// `None` is the ordinary case and means what `ANSWERED BY ANY` means: any
    /// copy may answer. See [`AnsweredBy`] for why this is a second axis rather
    /// than a tighter [`Staleness`].
    pub answered_by: Option<AnsweredBy>,
    /// Where the statement sits in the source.
    pub span: Span,
}

/// How far behind the node answering a read is allowed to be.
///
/// **A candidate filter, never a marker.** It does not ask to be told that an
/// answer was stale; it says which nodes may answer at all. A marker nobody is
/// obliged to read is not a guarantee, which is why a read no node can satisfy
/// is refused rather than quietly promoted to the leader.
///
/// The bound is a literal duration and a parameter is not accepted in its place,
/// the rule `TIMEOUT` already keeps: a tolerance a bound value could set is a
/// tolerance a caller could widen, and this one is meant to be readable in the
/// statement that asked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Staleness {
    /// The tolerance, as the statement wrote it.
    pub within: Duration,
    /// Where the clause sits, for the refusal to point at.
    pub span: Span,
}

/// Which nodes a read admits as its answerer.
///
/// # Why this is not a tighter staleness bound
///
/// A follower at zero lag is still not authoritative. Being level a moment ago
/// says nothing about a write committing right now, so a bound of `0s` does not
/// linearise — and `0s` is refused anyway, because it admits no node at all
/// including the one being asked. Answering a read that had to come from the
/// leader out of a freshness bound is a wrong answer that raises no error, which
/// is why the two are separate controls and not one knob.
///
/// The two **compose** and neither widens the other: a read may name both, and
/// it is answered only where both hold.
///
/// # Why the clause is not called `AUTHORITY`
///
/// Because that word is already spoken for, and by the part of the language a
/// mistake would be most expensive in: [`ReachRef`] documents itself as *how far
/// an authority goes*, `DEFINE USER … ON prod.orders` writes one, and the whole
/// grant surface is built on the noun. A read clause wearing the same word would
/// make `AUTHORITY LEADER` look like a permission to every reader who met the
/// grant vocabulary first.
///
/// `ANSWERED BY` says the thing itself — who answers — and collides with
/// nothing. *Strong* and *consistency* were rejected as terms of art: both
/// promise a linearisability this engine does not claim, and the limit below is
/// exactly why.
///
/// # Why `LEADER`
///
/// Leadership is already this product's own vocabulary rather than an
/// implementation detail leaking out: it is a catalog row, `DEFINE REPLICA …
/// ROLES writable` declares it, and `INFO FOR NODE` reports it. A new word would
/// be a second name for something the reader already has one for.
///
/// # The limit that ships with it
///
/// Answered by the leader is not **repeatable**. Two such reads with writes in
/// between legitimately differ, and neither is wrong. The clause says where the
/// answer comes from; it does not hold the store still while the caller reads —
/// that is what a transaction is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnsweredBy {
    /// Which nodes the read admits.
    pub admits: Admitted,
    /// Where the clause sits, for a refusal to point at.
    pub span: Span,
}

/// The two answers `ANSWERED BY` takes.
///
/// Two and not three: a third waits for something that needs
/// it. Named values rather than a `bool`, for the reason every tag in this
/// workspace is named — a `bool` makes an unrecognised spelling silently become
/// one of the two, and here the silent direction is the unsafe one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Admitted {
    /// Any copy may answer. The default, and writable on purpose: a read that
    /// deliberately does not need the leader should be able to say so.
    #[default]
    AnyCopy,
    /// Only the node that decides writes for these records may answer.
    Leader,
}

/// A point in the store's history a read answers from.
///
/// # Why this is a sequence and not a timestamp
///
/// Records are versioned by a suffix on their own key, and that suffix is the
/// log sequence the version was written at. The sequence is the store's only
/// ordering authority: no log record carries a wall clock, and two commits
/// within the same millisecond are ordered by sequence and by nothing else.
///
/// So a timestamp could not name a point *between* those two commits — it would
/// be a spelling that looks more precise than the thing it addresses. The clause
/// names the number the store actually orders by, which is the same number an
/// answer reports and a caller can hand straight back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    /// The sequence to read at.
    pub at: u64,
    /// Where the clause sits, for a refusal to point at.
    pub span: Span,
}

/// What a caller accepted when they wrote `APPROXIMATE`, and what they will
/// spend on it.
///
/// The budget is the walk's speed-against-recall dial: how many candidates it
/// keeps in hand before it stops. Larger explores more and costs more, and the
/// trade belongs to **this read** rather than to the declaration — a caller who
/// needs a better answer for one query should not have to redeclare the store,
/// and one who needs a cheaper answer should not degrade everybody else's.
///
/// It never reaches the index **build**. The build walks the same graph to choose
/// a new node's neighbours, so a read's budget leaking into it would let two
/// replicas replaying one log with different reads interleaved build different
/// graphs — the determinism this index gave up its hierarchical layer to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approximation {
    /// `APPROXIMATE` — the walk spends the budget the engine was built with.
    Default,
    /// `APPROXIMATE EFFORT 200` — the walk keeps this many candidates.
    ///
    /// At least one, refused below that where it is written, as `DEPTH n` and
    /// `vector<n>` are: a walk that may keep no candidates is a search with no
    /// way to answer.
    Effort(usize),
}

/// A ceiling on how long a read may run.
///
/// **Refused, never truncated.** A read that reaches its ceiling fails; it does
/// not answer with the part it had. A partial answer that looks whole is the
/// failure this store spends its rules removing, and a timeout is the easiest
/// place in a language to introduce one — the records are already in hand and
/// returning them costs nothing, which is exactly why it must not be done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeout {
    /// The ceiling, as the statement wrote it.
    pub after: Duration,
    /// Where the clause sits, for the refusal to point at.
    pub span: Span,
}

/// An assertion about how a read was served.
///
/// **A refusal, never a router.** It does not choose a path — nothing here
/// reaches the planner — it fails the statement when the path taken is not the
/// one named. That turns the worst failure mode an indexed store has, the query
/// that quietly stops using its index and starts scanning, from a thing you find
/// out from a latency graph into a thing the statement says out loud.
///
/// It is checked against what the read **did**, not against what the planner
/// chose, and the difference matters: an ordered index that could not fill the
/// bound sends the read to the scan, and an assertion satisfied by the planner's
/// intention would pass exactly where the scan it was written to catch happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Using {
    /// `USING <path>` — the access path the read is expected to report.
    ///
    /// Carried as written rather than as a checked variant, because the set of
    /// path words belongs to the store that reports them and duplicating it in
    /// the grammar would be a second vocabulary of exactly the kind one plan
    /// structure exists to remove. An unrecognised word is refused before the
    /// read runs, naming the ones that exist.
    Path(Name),
    /// `USING INDEX <name>` — the index the read is expected to have used.
    ///
    /// Stronger than a path word and often what is actually meant: `index` says
    /// *an* index answered, this says *which*.
    Index(Name),
}

/// How the branches of `ORDER BY FUSE (…)` are combined (G038).
///
/// A record's fused score is the sum, over the branches where it is within the
/// first `depth` of that branch's own order, of `weight / (K + rank)` — ranks,
/// never the branches' values, because a relevance score and a distance are not
/// on one scale and adding them lets whichever has the larger range decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fusion {
    /// One weight per branch, in the order of `Select::order`; each above zero.
    pub weights: Vec<Number>,
    /// How far down each branch's own order a record may be and still count;
    /// `None` when the statement named no `DEPTH`, so the default is the
    /// executor's and the statement renders back as it was written.
    pub depth: Option<u64>,
    /// Where `FUSE` was written.
    pub span: Span,
}

/// One sort key, and which way it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ordering {
    /// What to sort by.
    ///
    /// Read in the condition position, so a bare name is a route into the
    /// record — or a projected name, since ordering runs after projection. An
    /// **expression** works too, which is what makes a nearest-neighbour query
    /// an ordinary `ORDER BY … LIMIT` rather than an operator of its own.
    pub key: Expr,
    /// Whether the order is reversed.
    pub descending: bool,
}

/// Which values a read answers with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Projection {
    /// `*` — the record as it is stored, with nothing added.
    ///
    /// Kept apart from the composed form below rather than expressed as one of
    /// its cases, because it is the read that copies nothing: the records go to
    /// the answer as they were decoded, and giving it a projection to apply
    /// would give the commonest read in the language a per-record allocation to
    /// pay for a list that is empty.
    All,
    /// A named list, in the order it was written, and whether the record's own
    /// fields join them.
    ///
    /// Order is carried even though the answer is a name-ordered object, because
    /// an error naming the second of two colliding projections should point at
    /// the one the author wrote second. It is also why `*` needs no position of
    /// its own here: the answer is ordered by name whatever order the list was
    /// written in, so *where* the star stands among the values cannot be
    /// observed.
    Values {
        /// `*` written among the values, if it was.
        ///
        /// The span is what a message about a field the record and the list both
        /// name would point at.
        everything: Option<Span>,
        /// The values written out, each with the name it answers under.
        values: Vec<Projected>,
    },
}

impl Projection {
    /// Whether the record's own fields reach the answer.
    ///
    /// The one question two clauses ask of a projection — `OMIT`, which has
    /// nothing to subtract from without it, and the projection stage, which
    /// starts from the record rather than from nothing.
    #[must_use]
    pub const fn stars(&self) -> bool {
        match self {
            Self::All => true,
            Self::Values { everything, .. } => everything.is_some(),
        }
    }

    /// The values written out by name, which is none for a bare `*`.
    #[must_use]
    pub fn written(&self) -> &[Projected] {
        match self {
            Self::All => &[],
            Self::Values { values, .. } => values,
        }
    }
}

/// One projected value, and the name it answers under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projected {
    /// How the value is produced.
    ///
    /// An ordinary expression, which is what makes `mean(age) * 2` writable: a
    /// fold is an [`ExprKind::Fold`] node like any other, so composing over one
    /// needs no second kind of projection. There is exactly one shape a fold
    /// has in this tree, which is the point — two would eventually disagree,
    /// and the disagreement would be a wrong number.
    pub value: Expr,
    /// The name it answers under.
    ///
    /// Resolved at parse rather than left for the executor: whether two
    /// projections collide is a property of the statement, so it is knowable
    /// before anything runs and is refused there.
    pub name: Name,
}

/// One step of a traversal: an edge table, and optionally the table its far
/// endpoint is read from.
///
/// Only the **last** step may leave the target out, and the grammar is what
/// guarantees that rather than a check: continuing a walk needs a node to
/// continue from, so `a->e1->e2` is one step landing on `e2` and never two steps
/// with a gap in the middle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hop {
    /// The edge table this step walks.
    pub edges: TableRef,
    /// The table the far endpoint is read from, when the statement names one.
    pub target: Option<TableRef>,
}

/// The three access paths the store has, named by what the statement targets.
///
/// Which one runs is decided here, by the shape of the statement, and not by a
/// cost model — there is no planner at this milestone and nothing pretends
/// otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// This node's own identity.
    ///
    /// Its own variant and **not** a table, because its data is not records: it
    /// lives in the `META` keyspace so that it does not replicate, which is the
    /// one thing a system table cannot be (ADR-0018 §1, and §3's amendment,
    /// which corrects the sentence that said otherwise).
    ///
    /// It carries no [`TableRef`], and everything that decides permissions from
    /// the tables a statement names has to answer for that separately rather
    /// than reading an empty list as "nothing to check".
    Node,
    /// One record, by its identity.
    Record(RecordTarget),
    /// Every record of a table.
    Table(TableRef),
    /// `SELECT * FROM events:1000..2000` — every record whose identity falls in
    /// a span.
    ///
    /// # Why this is a source and not a condition
    ///
    /// `WHERE id >= 1000 AND id < 2000` asks the same question and is answered
    /// by reading the table and testing every record. This is answered by
    /// **walking the keyspace between two positions**, because a record's key is
    /// its table prefix followed by its identity — so the records outside the
    /// span are not read, not decoded and not tested. The difference is the
    /// whole reason the variant exists, and it is a difference in cost of the
    /// same order as an index.
    ///
    /// # What it is a window over
    ///
    /// Identity order, which for both identity kinds this store issues is also
    /// **write order**: `Int` is a per-table counter, and `Uuid` is UUID v7,
    /// which carries a timestamp in its leading bits. So a span of identities is
    /// a span of time *as the store saw it*. It is not a span of an event time a
    /// record carries in a field — if events arrive out of order, those are two
    /// different questions, and the one this answers is the arrival.
    Range {
        /// The table.
        table: TableRef,
        /// The first identity in the span, which is always inside it.
        lower: Identity,
        /// The last, inside the span only when the bound was written `..=`.
        upper: Identity,
        /// Whether the upper bound is itself included.
        inclusive: bool,
        /// Where the whole source sits.
        span: Span,
    },
    /// A walk along one or more edge tables.
    ///
    /// `users:1->follows` reads the edge records themselves;
    /// `users:1->follows->users` resolves one step further and reads the records
    /// the edges point at; `users:1->follows->users->follows->users` does it
    /// again from there. Every step is an index read, because an edge table
    /// carries an index on each endpoint from the moment it is declared.
    Traverse {
        /// Where the walk starts.
        from: RecordTarget,
        /// Which way the arrows point.
        ///
        /// One direction for the whole walk. A per-hop direction asks a real
        /// question — "who follows somebody ada follows" — and is a separate
        /// design rather than a loosened rule; `docs/tessariql.md` §8 holds it.
        direction: Direction,
        /// The steps, in order. Never empty.
        hops: Vec<Hop>,
        /// `DEPTH n` — how many times the single hop repeats.
        ///
        /// `None` is a walk written out step by step, which is bounded because
        /// the steps are written. `Some(n)` is the only construct in the
        /// language that repeats, and `n` is an integer **literal** for that
        /// reason: a walk whose length comes from a parameter is a walk whose
        /// length is not in the statement, and nothing reading the statement
        /// could tell how far it goes.
        ///
        /// Answers with every distinct record reachable in `1..=n` hops. The
        /// start is marked seen before the first round, so a cycle terminates
        /// and no record is answered twice — which is what makes `n` bound the
        /// *work* and not merely the number written down.
        depth: Option<u64>,
    },
    /// `FROM SEARCH knowledge MATCHES 'ada lovelace'` — the records of every
    /// member of a declared search, ranked as one collection (ADR-0105).
    Search {
        /// The search.
        name: Name,
        /// What is asked of it.
        ask: super::SearchAsk,
        /// `WHERE …` after the ask: what each answered record must also hold.
        condition: Option<Box<Expr>>,
    },
    /// The records a condition holds for.
    ///
    /// Which access path this becomes is decided when it runs, by what exists:
    /// an index read where an index serves one of the condition's conjuncts, a
    /// scan where none does. The statement is the same either way, which is what
    /// lets an index be added later without rewriting a single query — and the
    /// candidates an index offers are still tested against the **whole**
    /// condition, because the index answered one conjunct and the statement
    /// asked for all of them.
    Where {
        /// The table being read.
        table: TableRef,
        /// What each record must satisfy.
        condition: Box<Expr>,
    },
    /// Two tables matched on a value neither of them stores a pointer for.
    ///
    /// The other kind of join from [`Select::fetch`]: a reference *is* an
    /// address, so following one is a point read, and this is for the
    /// relationship nobody wrote an address down for.
    ///
    /// # A row is a record with two named sides
    ///
    /// The answer is `{ users: { … }, orders: { … } }` rather than the two
    /// records merged. That is not a shape decision, it is the *naming* decision:
    /// merged records need a rule for what happens when both carry `name`, and
    /// every candidate rule — an alias syntax, a prefixing convention, last-wins
    /// — is something a reader has to learn. Nested, `users.name` and
    /// `orders.name` were never in danger of colliding, and every path,
    /// projection, `WHERE`, `ORDER BY` and `GROUP BY` works over it unchanged
    /// because it is an ordinary object.
    ///
    /// The row's identity is the **left** record's, so a left record matching
    /// two right records answers as two rows carrying one id. A row is not a
    /// record and this store's answers are keyed by record; a shape for rows is
    /// a change to the wire, the JSON surface and the console, which is a
    /// milestone rather than a clause.
    ///
    /// # Inner, so that `LEFT` stays additive
    ///
    /// A row appears only where both sides match. Decided now because it cannot
    /// be decided later: if a bare `JOIN` meant *outer*, adding `LEFT`
    /// afterwards would change what already-written statements answer.
    Join {
        /// The side that is read and drives.
        ///
        /// Boxed, with the other side, because a side may itself hold a whole
        /// read: inline they make this variant three times the size of every
        /// other one, and every [`Source`] anywhere pays for it.
        left: Box<JoinSide>,
        /// The side that is probed.
        right: Box<JoinSide>,
        /// The route into a left record whose value is matched.
        left_key: FieldPath,
        /// The route into a right record it is matched against.
        right_key: FieldPath,
        /// `ASOF JOIN`: each left record is paired with the newest right record
        /// at or before its time, and kept when there is none (ADR-0088 §4).
        asof: bool,
        /// What each joined row must satisfy, when a `WHERE` was written.
        ///
        /// Over the **composite**, so it reads `users.name` and `orders.total`
        /// like everything else does.
        condition: Option<Box<Expr>>,
    },
    /// A read whose answer is what the outer statement reads.
    ///
    /// The answer is the inner records themselves — not wrapped, not renamed —
    /// so an outer `WHERE`, `ORDER BY` and projection read them exactly as they
    /// would read the table. That is what makes one engine's answer the next
    /// question's source without a shape to learn.
    ///
    /// # It states its own ceiling
    ///
    /// The inner read is **materialised**: unlike a table, there is no index to
    /// walk and no bound to push down, so every record it answers with is held
    /// at once. A source that could grow without limit is therefore refused
    /// without a `LIMIT`, rather than truncated at a number nobody wrote — a
    /// silently truncated source answers a different question from the one that
    /// was asked, and looks exactly like a complete one.
    ///
    /// # It is also the only place some reads can be filtered
    ///
    /// `WHERE` belongs to the table position — `FROM t WHERE c` — so a
    /// traversal and a grouped read have nowhere to put one. Wrapping either in
    /// a materialised read gives it one, which is why the condition lives here
    /// rather than only inside. It is also where a condition over a *projected*
    /// name goes: `SELECT city, count(*) AS n … GROUP BY city` produces `n`, and
    /// nothing inside that read can ask about it.
    Subquery {
        /// The read whose answer this source is.
        read: Box<Select>,
        /// What each of its records must satisfy, when a `WHERE` was written.
        condition: Option<Box<Expr>>,
    },
}

/// One side of a join — what it reads, and the name the row files it under.
///
/// The name is not decoration. A row is `{ <left>: …, <right>: … }`, so the two
/// sides need two names, and `ON`, `WHERE`, `ORDER BY` and the projection are
/// all routes through them. A table brings its own name and may be given
/// another; a read brings none, which is why the two cases are different
/// variants rather than one variant with an optional name that is sometimes
/// required.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinSide {
    /// A table, under its own name unless `AS` gave it another.
    ///
    /// An alias is what makes a self-join sayable: `users AS a JOIN users AS b`
    /// files two records of one table under two names, where `users JOIN users`
    /// has one name for both and no row a reader could address.
    Table {
        /// The table read.
        table: TableRef,
        /// The name `AS` gave it, when one was written.
        alias: Option<Name>,
    },
    /// A read, materialised and then joined, under the name `AS` gave it.
    ///
    /// The name is **mandatory** and typed as such: a read has no name of its
    /// own, and a side with no name has no place in the row.
    Read {
        /// The read whose answer this side joins.
        read: Box<Select>,
        /// The name the row files it under.
        alias: Name,
    },
}

impl JoinSide {
    /// The name this side answers under in the row.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Table {
                alias: Some(alias), ..
            }
            | Self::Read { alias, .. } => &alias.text,
            Self::Table { table, alias: None } => &table.name.text,
        }
    }
}

/// `FILL <mode> FROM <start> TO <end>`: the windows of a range a windowed
/// grouping answers with even where nothing was written (ADR-0088 §2).
///
/// The range is part of the clause and not optional: a window with no records
/// has no row because the statement did not say which windows it meant, and
/// this is where it says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fill {
    /// What an empty window answers with.
    pub mode: FillMode,
    /// The first instant of the range, inclusive.
    pub from: Expr,
    /// The end of the range, exclusive.
    pub to: Expr,
    /// Where the clause was written.
    pub span: Span,
}

/// What a filled window's folds answer with. `count` answers `0` whatever the
/// mode, so a filled row can always be told from a written one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FillMode {
    /// `PREVIOUS` — the nearest written window before it, per group.
    Previous,
    /// `LINEAR` — interpolated between the nearest written windows either side.
    Linear,
    /// A value — `NULL`, `0`, anything an expression answers.
    Value(Expr),
}
