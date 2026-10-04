use super::*;

/// `PATH TO <record> DEPTH n [WEIGHT field]` on a one-hop walk (G055 W6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathTo {
    /// The record the path ends at.
    pub to: RecordTarget,
    /// The edge field a step costs, when the path is weighted; each step costs
    /// one otherwise.
    pub weight: Option<Name>,
    /// Where `PATH` is.
    pub span: Span,
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
        /// `PATH TO <record> … [WEIGHT field]` — the shortest path to one record
        /// within `depth` steps rather than everything within them (G055 W6).
        /// Boxed: rare, and as large as the rest of the walk.
        path: Option<Box<PathTo>>,
    },
    /// `FROM SEARCH knowledge MATCHES 'ada lovelace'` — the records of every
    /// member of a declared search, ranked as one collection (ADR-0105).
    Search {
        /// The search.
        name: Name,
        /// What is asked of it.
        ask: super::super::SearchAsk,
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
