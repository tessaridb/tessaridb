//! What can go wrong between a script and the store.
//!
//! Every variant that can name a place does. A script is written by hand, and a
//! failure that cannot point at the words that caused it makes its author read
//! the whole thing again.

use tessari_ql::{Function, Span};
use tessari_types::article;

/// Result alias for every fallible operation in this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// A failure running a script.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The script could not be read.
    #[error(transparent)]
    Script(#[from] tessari_ql::Error),

    /// The store refused the work.
    #[error(transparent)]
    Store(#[from] tessari_storage::Error),

    /// A `SCHEMAFULL` table was written a field it does not declare, with the
    /// statement that would make the same write succeed.
    ///
    /// The store raises the refusal; the suggestion is added here, because
    /// writing a declaration needs the language and *proving* the text is a
    /// declaration needs the parser — neither of which the store has, and both
    /// of which are the difference between a remedy and a plausible-looking
    /// string. It is the same rule `describe` follows for a rendered
    /// declaration: offered only when it re-reads as what it claims to be, and
    /// omitted rather than approximated otherwise, in which case the plain
    /// [`Store`](Self::Store) refusal is what a caller sees.
    #[error("{refusal}; declare it with `{suggestion}`")]
    UndeclaredField {
        /// The store's refusal, unchanged.
        ///
        /// Boxed because this is the only variant that carries a whole store
        /// refusal **beside** something else, and the two together are wider
        /// than every `Result` in this crate should be asked to reserve.
        refusal: Box<tessari_storage::Error>,
        /// A statement that would accept the write, proven to parse.
        suggestion: String,
    },

    /// A script refused itself with `THROW`.
    ///
    /// Carried as its own variant rather than folded into a generic failure, so
    /// that a caller can tell a refusal the script *chose* from one the store
    /// raised — the two mean different things to whatever is handling them.
    #[error("{message} (at {span})")]
    Thrown {
        /// What the script said.
        message: String,
        /// Where it said it.
        span: tessari_ql::Span,
    },

    /// A `DEFINE FAILOVER` names periods that do not hold together.
    ///
    /// The store owns the relations and the direction each one fails in, so the
    /// reason is carried through verbatim rather than restated: a second copy of
    /// *too large means the leader steps over the moment it should have stood*
    /// would be a second answer that drifts from the one place that enforces it.
    ///
    /// The span is added because the store cannot have it. A refusal that named
    /// the relation but not the statement would send an operator looking through
    /// a script for which of five durations it meant.
    #[error("{reason} (at {span})")]
    FailoverRefused {
        /// The store's own words for which relation broke and in which direction.
        reason: String,
        /// Where the statement is.
        span: tessari_ql::Span,
    },

    /// A `USING` naming a word that is not an access path.
    ///
    /// Almost always a typo, and refused before the read rather than after it:
    /// running a scan to then report that `inedx` is not a word would be the
    /// worst of both answers.
    #[error("`USING {named}` names no access path — this store reports {known} (at {span})")]
    NoSuchAccessPath {
        /// The word as written.
        named: String,
        /// The words that exist, comma-separated.
        known: String,
        /// Where it was written.
        span: tessari_ql::Span,
    },

    /// A read passed the ceiling its statement set.
    ///
    /// Refused rather than answered in part. The records it had produced are
    /// dropped, and the count is reported here — which is the same thing a
    /// truncated answer would have told the caller, in the one place a caller
    /// cannot mistake for the result.
    #[error(
        "the read passed its ceiling of {after} after {produced} records (at {span}); \
         it is refused rather than answered in part"
    )]
    TimedOut {
        /// The ceiling, as the statement wrote it.
        after: String,
        /// How many records the read had produced when the ceiling passed.
        produced: u64,
        /// Where the clause is.
        span: tessari_ql::Span,
    },

    /// A read standing in an expression grew past the ceiling this node applies
    /// when the read named none.
    ///
    /// Refused rather than truncated, and the refusal names the one word that
    /// lifts it. A default that quietly kept a prefix would turn a read that is
    /// expensive and right into one that is cheap and wrong.
    #[error(
        "the read standing here passed {most} records without a bound of its own \
         (at {span}); give it a `LIMIT` to say how many of them the question is about"
    )]
    Unbounded {
        /// The ceiling this node applies to a read that named none.
        most: u64,
        /// Where the held read is.
        span: tessari_ql::Span,
    },

    /// The same ceiling reached by a read whose fold holds its whole group.
    ///
    /// A separate refusal because it names a different escape, and naming the
    /// wrong one is worse than refusing without advice: a `LIMIT` bounds what a
    /// fold **answers with** and not what it reads, so the sentence
    /// [`Self::Unbounded`] tells the author to write would lift the ceiling here
    /// and change nothing about the memory it was protecting. The escape that
    /// does work is bounding what the fold reads (Q-227).
    #[error(
        "the read standing here folded `{fold}` over more than {most} records \
         (at {span}); `{fold}` keeps every value it is given, and a `LIMIT` bounds \
         what a fold answers with rather than what it reads — bound its source \
         instead, as in `FROM (SELECT … LIMIT {most})`"
    )]
    UnboundedCollection {
        /// The fold that holds its group, as it is written.
        fold: &'static str,
        /// The ceiling this node applies to a read that named none.
        most: u64,
        /// Where the held read is.
        span: tessari_ql::Span,
    },

    /// An `ONLY` read that more than one record answered.
    ///
    /// Refused rather than answered with the first, for the reason a timeout
    /// refuses rather than truncating: the records are already correct, so
    /// handing back one of them costs nothing and looks like success. The count
    /// is reported because it is what tells a mistaken assertion from a mistaken
    /// condition — two is a duplicate, four thousand is the wrong `WHERE`.
    ///
    /// None is not this error. `ONLY` asserts at most one, and an absence
    /// answers `NONE`.
    #[error(
        "`ONLY` says one record answers this read and {found} did (at {span}); \
         narrow it, or drop the word and take the list"
    )]
    NotAlone {
        /// How many answered.
        found: usize,
        /// Where the word is.
        span: tessari_ql::Span,
    },

    /// An `AFTER` anchor names a record that is not there, on a read that needs
    /// its key.
    ///
    /// A cursor resumes an order, and an order this read named is a value the
    /// anchor held — so with the record gone there is no position to resume
    /// from. Every guess at one picks a page: the page after where the record
    /// used to be, the page after the next record, or the whole table again.
    ///
    /// A read that named **no** order never raises this. Its order is the
    /// store's own, the identity is the whole key, and the position outlives the
    /// record standing on it.
    #[error(
        "`AFTER` names a record `{table}` no longer holds (at {span}), \
         and this read's order needs the value it held; \
         resume from a record that is there, or drop the `ORDER BY`"
    )]
    AnchorGone {
        /// The table the anchor named.
        table: String,
        /// Where the anchor is.
        span: tessari_ql::Span,
    },

    /// A `USING <path>` the read did not satisfy.
    ///
    /// The whole point of the clause. It is checked against what the read
    /// **did**, so an ordered index that could not fill the bound and handed the
    /// read to the scan is caught here — which is precisely the case an
    /// assertion checked against the planner's *intention* would have passed.
    #[error("this read was asked to take the {expected} path and took the {took} path (at {span})")]
    PathNotTaken {
        /// The path the statement named.
        expected: String,
        /// The path the read reported.
        took: String,
        /// Where the assertion was written.
        span: tessari_ql::Span,
    },

    /// A `USING INDEX <name>` the read did not satisfy.
    ///
    /// Separate from [`Self::PathNotTaken`] because it is a different question:
    /// `USING index` asks whether *an* index answered and this asks *which*, and
    /// a read served by the wrong index is a plan regression that the path word
    /// alone cannot see.
    #[error("this read was asked to use the index {expected} and used {took} (at {span})")]
    IndexNotUsed {
        /// The index the statement named.
        expected: String,
        /// What served it instead — an index by name, or `no index`.
        took: String,
        /// Where the assertion was written.
        span: tessari_ql::Span,
    },

    /// A join whose two sides hold different kinds of value at their keys.
    ///
    /// Equality across two kinds is false, so such a join can only ever answer
    /// no rows — and *no rows* is exactly what a correct join over data that
    /// happens not to match answers too. The two are indistinguishable to
    /// whoever reads the answer, and only one of them is a mistake. A join is
    /// written to be trusted, so the store says which one it is rather than
    /// handing back an empty list.
    ///
    /// The commonest shape is `record` against `string`: an identity stored as
    /// text on one side and as a reference on the other.
    #[error(
        "the join matched {left_key} ({left_kinds}) against {right_key} ({right_kinds}), \
         and no value of one kind equals a value of the other, so this could only \
         answer no rows (at {span})"
    )]
    JoinKeysDiffer {
        /// The route into the left record.
        left_key: String,
        /// The kinds the left side held there, in order.
        left_kinds: String,
        /// The route into the right record.
        right_key: String,
        /// The kinds the right side held there, in order.
        right_kinds: String,
        /// Where the read was written.
        span: tessari_ql::Span,
    },

    /// A `MERGE` whose right-hand side is not an object.
    ///
    /// The verb folds one object into another, so a scalar or an array there has
    /// no reading: `MERGE 3` could only mean "replace the record with 3", and
    /// `UPDATE t:1 = 3` already says that.
    #[error("MERGE takes an object to fold in, and this is {found} (at {span})")]
    MergeIsNotAnObject {
        /// What stood there instead.
        found: &'static str,
        /// Where it was written.
        span: tessari_ql::Span,
    },

    /// An assignment into a route the record does not have.
    ///
    /// `SET a.b.c = 1` on a record with no `a`. Creating the objects on the way
    /// would be the store writing structure nobody asked for, which is the same
    /// call it makes about zero-filling a hole in a file — so the route is named
    /// and the write is refused.
    #[error("{route} is not a route this record has, so nothing can be assigned to it (at {span})")]
    NoSuchRouteToAssign {
        /// The route as written.
        route: String,
        /// Where it was written.
        span: tessari_ql::Span,
    },

    /// A ranged write that would leave a gap in a file.
    ///
    /// Zero-filling it would be the store inventing bytes nobody wrote, and a
    /// real hole is a sparse-file feature nobody has asked for — so the write is
    /// refused and says where the file actually ends.
    #[error("writing {path} at {at} would leave a hole: the file is {size} bytes (at {span})")]
    WriteWouldLeaveAHole {
        /// The file's path.
        path: String,
        /// The offset the write asked for.
        at: usize,
        /// How long the file is.
        size: usize,
        /// Where the statement is.
        span: tessari_ql::Span,
    },

    /// A write that would leave a file larger than its bucket accepts.
    ///
    /// Reported against the size the file **would end up** being rather than
    /// the bytes the statement carried, because a ranged write reaches the
    /// ceiling by splicing — and a message naming the splice would be naming
    /// the smaller of the two numbers the caller needs.
    #[error(
        "writing {path} would leave {size} bytes: the bucket takes at most {ceiling} (at {span})"
    )]
    FileAboveBucketCeiling {
        /// The file's path.
        path: String,
        /// How long the file would have been.
        size: u64,
        /// The largest file the bucket accepts.
        ceiling: u64,
        /// Where the statement is.
        span: tessari_ql::Span,
    },

    /// A backup could not be written.
    ///
    /// Its own variant rather than a wrapped store error, because the failures
    /// differ in kind: a backup writes into a buffer and reads the log, so what
    /// goes wrong is the log or the buffer, and a caller reading "the store
    /// refused the work" would look in the wrong place.
    #[error("the backup could not be written: {reason}")]
    BackupFailed {
        /// What the backup writer said.
        reason: String,
    },

    /// `BACKUP … TO` on a node started without a backup folder.
    ///
    /// A node writes a file only where its operator said it may, so with no
    /// folder there is nowhere, rather than a default somebody did not choose.
    #[error(
        "this node has no backup folder, so `BACKUP … TO` has nowhere it may write; \
         start it with --backup-dir (TESSARIDB_BACKUP_DIR in the image)"
    )]
    NoBackupFolder,

    /// A `BACKUP … TO` name that would not stay inside the backup folder.
    ///
    /// Refused before anything is written: a name is a relative path of plain
    /// parts, and a symlink inside the folder is never followed out of it.
    #[error("the backup cannot be written to '{name}': {reason}")]
    BackupNameRefused {
        /// The name as the statement gave it.
        name: String,
        /// Why it would not stay inside the folder.
        reason: String,
    },

    /// A `RESTORE` whose script would create a database that already exists.
    ///
    /// A restore only creates, so it lands beside what the store holds and never
    /// over it; nothing of the script is written.
    #[error("the restore was not run, because {place} already exists; a restore only creates")]
    RestoreTargetExists {
        /// The namespace and database, as `ns.db`.
        place: String,
    },

    /// A `RESTORE` refused before anything ran: the script does something other
    /// than create databases and fill them, or cannot be read as a script.
    #[error("the restore was not run: {reason}")]
    RestoreRefused {
        /// Why, naming the statement by its first words.
        reason: String,
    },

    /// A `BACKUP … TO` whose file is already there.
    ///
    /// A backup never replaces a file: the one it would replace is most likely
    /// an earlier backup, and losing it to a mistyped name is not recoverable.
    #[error(
        "the backup was not written, because {path} already exists; a backup never replaces a file"
    )]
    BackupExists {
        /// The file that is already there.
        path: String,
    },

    /// A stored value could not be read back.
    #[error(transparent)]
    Encoding(#[from] tessari_encoding::Error),

    /// A shape the store will not hold.
    ///
    /// Raised on the way in, never on the way out, and the coordinates it names
    /// are the **snapped** ones — the store's version of the position rather
    /// than the caller's. A caller comparing them against what it sent can see
    /// that quantisation was the cause; quoting the submitted coordinates back
    /// would describe a shape that was never in question.
    #[error("a shape was refused: {refused} (at {span})")]
    GeometryRefused {
        /// What was wrong with it.
        refused: tessari_geo::Refused,
        /// Where the statement is.
        span: Span,
    },

    /// A statement needs a namespace and the session has not selected one.
    #[error("no namespace selected (at {span}) — say `USE NAMESPACE …` first")]
    NoNamespaceSelected {
        /// Where the statement is.
        span: Span,
    },

    /// A statement needs a database and the session has not selected one.
    ///
    /// An error rather than a default, because guessing which database a write
    /// belongs to is the one mistake reading the result cannot undo.
    #[error("no database selected (at {span}) — say `USE DATABASE …` first")]
    NoDatabaseSelected {
        /// Where the statement is.
        span: Span,
    },

    /// A name the catalog does not hold.
    #[error("no {entity} named {name:?} (at {span})")]
    Unknown {
        /// What kind of thing was looked for.
        entity: &'static str,
        /// The name as written.
        name: String,
        /// Where it was written.
        span: Span,
    },

    /// A catalog object was asked to go while something still points at it.
    ///
    /// Refused rather than cascaded, and the choice is the same one
    /// [`Error::Unbounded`] makes about a read: a statement that removes an
    /// unbounded amount on the strength of one name is the widest thing this
    /// language can be asked to run, and the person writing it is thinking
    /// about the one name. So the refusal counts what it found and names the
    /// first of them, because a message saying only *not empty* leaves the
    /// reader to go and run the query this statement already ran.
    ///
    /// There is deliberately no `CASCADE`: it is the unbounded form under
    /// another spelling.
    #[error(
        "{} `{name}` still {} {count} {} (at {span}), the first being `{first}` \
         — remove {} first; there is no `CASCADE`, because a statement that \
         removes an unbounded amount from one name is the mistake this refusal \
         exists to catch",
        .depended.entity(),
        .depended.relation(),
        .depended.dependants(*count),
        .depended.dependants(2),
    )]
    StillDepended {
        /// Which of the three dependencies this is.
        depended: Depended,
        /// The name that was asked to go, as written.
        name: String,
        /// How many dependants were found.
        count: usize,
        /// One of them, named so the reader can act without a second query.
        first: String,
        /// Where the statement is.
        span: Span,
    },

    /// A `LET` produced something that is not a single value.
    ///
    /// Unreachable while the executor answers a binding with a value, and named
    /// rather than unwrapped: an executor change that made a binding answer with
    /// records would otherwise become a panic in a running node, and here it is
    /// a compile-time conversation followed by an honest refusal.
    #[error("a binding must produce one value (at {span})")]
    BindingIsNotAValue {
        /// Where the binding is.
        span: Span,
    },

    /// `BEGIN` inside a transaction that is already open.
    #[error("a transaction is already open (at {span})")]
    NestedTransaction {
        /// Where the second `BEGIN` is.
        span: Span,
    },

    /// A transaction verb in a script run through [`crate::Atomic`], whose
    /// transaction the caller commits (ADR-0087).
    #[error(
        "this script runs inside a transaction its caller commits (at {span}) — it may not \
         contain `BEGIN`, `COMMIT`, `CANCEL` or `VERIFY`"
    )]
    TransactionVerbInAtomic {
        /// Where the verb is.
        span: Span,
    },

    /// `COMMIT` or `CANCEL` with nothing open.
    #[error("no transaction is open (at {span})")]
    NoOpenTransaction {
        /// Where the statement is.
        span: Span,
    },

    /// `VERSION` inside an open transaction.
    ///
    /// A transaction *is* a point in the store's history — one snapshot, held
    /// for as long as it runs, which is what makes its reads agree with each
    /// other. A statement inside it asking for a different point is asking for
    /// something a transaction cannot be.
    ///
    /// Refused rather than answered at the transaction's own snapshot, which
    /// would run the statement, return rows, and leave the clause reading as
    /// though it had been honoured.
    #[error(
        "`VERSION` cannot be used inside a transaction — a transaction already \
         reads at one point in history (at {span})"
    )]
    VersionInsideTransaction {
        /// Where the clause is.
        span: Span,
    },

    /// A script that opened a transaction and never closed it.
    ///
    /// The work is discarded and this is raised, rather than committed: a script
    /// that does not say `COMMIT` has not said its work is finished, and
    /// discarding it silently would hide that it ran at all.
    #[error("the script ended with a transaction still open (at {span}); its work was discarded")]
    UnclosedTransaction {
        /// Where the `BEGIN` was.
        span: Span,
    },

    /// A view was named where a table has to be.
    ///
    /// Reading a view happens by rewriting the statement before anything is
    /// resolved, so a view name that reaches a resolution is a view in a
    /// position that has no records to act on — a write, a keyspace address, an
    /// index, or a read the rewrite does not cover.
    #[error(
        "`{name}` is a view (at {span}) — a view holds no records, so it can only be read from"
    )]
    ViewIsNotATable {
        /// The view named.
        name: String,
        /// Where it was written.
        span: Span,
    },
    /// A materialized view's read depends on something its source table's
    /// changes do not cover (ADR-0109 D4).
    ///
    /// Refused where the view is declared: kept current from one table's
    /// changes, the stored rows would go stale with no change in the feed to say
    /// so, and nothing would be in an error state.
    #[error(
        "a materialized view cannot {what} (at {span}): it is kept current from one table's changes"
    )]
    MaterializedShape {
        /// What the read does that a kept view cannot.
        what: &'static str,
        /// Where the view was declared.
        span: Span,
    },
    /// A materialized view read by somebody who may read only some of its
    /// source table's fields (ADR-0109 D7).
    ///
    /// A plain view re-runs its read with the caller's grants, so a hidden field
    /// is simply absent. A kept view's rows were computed already and cannot be
    /// redacted after the fact, so the read is refused rather than answered from
    /// fields the caller may not see.
    #[error(
        "`{view}` is kept from `{table}`, which this user may read only in part (at {span}) — \
         read `{table}` instead, or ask for its whole read"
    )]
    MaterializedFromHidden {
        /// The view named.
        view: String,
        /// Its source table.
        table: String,
        /// Where the view was named.
        span: Span,
    },
    /// A chain of views was expanded as far as the store will follow it.
    ///
    /// A view naming a view naming a view, past the depth this build accepts —
    /// which is also what a cycle looks like from here, and the chain is printed
    /// so the cycle is legible in the message.
    #[error("views nested more than {depth} deep (at {span}): {}", chain.join(" -> "))]
    ViewsTooDeep {
        /// The chain followed, in the order it was followed.
        chain: Vec<String>,
        /// How far the store will follow one.
        depth: usize,
        /// Where the read that started it was written.
        span: Span,
    },
    /// A write would store a value nested deeper than the store keeps.
    ///
    /// Refused at the write because the decoder refuses the same depth on the
    /// way back: storing it would make a record that exists and cannot be read.
    #[error("the value written nests containers more than {limit} deep (at {span})")]
    NestedTooDeep {
        /// The deepest nesting a stored value may have.
        limit: usize,
        /// Where the write was written.
        span: Span,
    },
    /// A stored view could not be read back as a read.
    ///
    /// The text was parsed when it was declared, so this is a catalog whose
    /// contents have moved under a build that no longer accepts them, not a
    /// statement somebody has just mistyped.
    #[error("the stored read of view `{name}` (at {span}) no longer parses: {detail}")]
    ViewUnreadable {
        /// The view whose stored read would not parse.
        name: String,
        /// What the parser said about it.
        detail: String,
        /// Where the view was named.
        span: Span,
    },
    /// `CREATE` over a record that is already there.
    ///
    /// Refused rather than replaced: a silent overwrite loses a record with
    /// nothing anywhere to notice, and `UPDATE` and `SET` both say replacement
    /// out loud.
    #[error("record {id} already exists (at {span}) — say `UPDATE` to replace it")]
    RecordExists {
        /// The identity as written.
        id: String,
        /// Where it was written.
        span: Span,
    },

    /// A record written to an event-time series without its time field, or
    /// with one that is not a `datetime` (ADR-0088 §1).
    #[error(
        "series `{table}` is ordered by `{field}`, and this record's `{field}` is {found} rather than a datetime (at {span})"
    )]
    SeriesTimeMissing {
        /// The series.
        table: String,
        /// The declared time field.
        field: String,
        /// What the record held instead.
        found: &'static str,
        /// Where the write was written.
        span: Span,
    },

    /// An event time a UUID version 7 cannot carry: before 1970, or past its
    /// 48-bit millisecond range.
    #[error(
        "series `{table}` cannot order an event at {instant} — a series holds instants from 1970 on (at {span})"
    )]
    SeriesTimeOutOfRange {
        /// The series.
        table: String,
        /// The instant, as written back.
        instant: String,
        /// Where the write was written.
        span: Span,
    },

    /// A write whose event time is already below the series' floor — it would
    /// be accepted and never answered, which is a loss with nothing in an error
    /// state.
    #[error(
        "series `{table}` no longer answers for {instant}, which is past its retention — the record would be written and never read (at {span})"
    )]
    BelowSeriesFloor {
        /// The series.
        table: String,
        /// The instant, as written back.
        instant: String,
        /// Where the write was written.
        span: Span,
    },

    /// An identity named by hand in an event-time series that does not carry
    /// the record's own time — the store mints these from the time field.
    #[error(
        "series `{table}` names its records from `{field}`; leave the identity out and the store mints it (at {span})"
    )]
    SeriesIdentityDerived {
        /// The series.
        table: String,
        /// The declared time field.
        field: String,
        /// Where the write was written.
        span: Span,
    },

    /// A change to a record's time field in an event-time series: its identity
    /// is minted from that field and would stop describing it.
    #[error(
        "`{field}` fixes where a record of series `{table}` sits, so it cannot change — delete the record and write it again (at {span})"
    )]
    SeriesTimeFixed {
        /// The series.
        table: String,
        /// The declared time field.
        field: String,
        /// Where the write was written.
        span: Span,
    },

    /// `FILL` without exactly one `time::bucket` key of a constant width.
    #[error(
        "`FILL` completes one window key — `GROUP BY time::bucket(<instant>, <width>)` with a written width (at {span})"
    )]
    FillNeedsWindow {
        /// Where the clause was written.
        span: Span,
    },

    /// `FILL` whose range does not evaluate to two instants.
    #[error("`FILL … FROM <instant> TO <instant>` needs both ends to be datetimes (at {span})")]
    FillNeedsRange {
        /// Where the clause was written.
        span: Span,
    },

    /// A fill that would answer more windows than one read may.
    #[error(
        "this `FILL` would answer {windows} windows, and a read fills at most {most} — narrow the range or widen the window (at {span})"
    )]
    FillTooWide {
        /// How many windows it would answer.
        windows: u64,
        /// The most one read may.
        most: u64,
        /// Where the clause was written.
        span: Span,
    },

    /// `LATEST BY` over something whose identity is not its time.
    #[error(
        "`LATEST BY` keeps the newest record per key, and only a series' identity says which is newest (at {span})"
    )]
    LatestNeedsSeries {
        /// Where the clause was written.
        span: Span,
    },

    /// `LATEST BY` beside `GROUP BY`: one keeps records, the other folds them.
    #[error("`LATEST BY` keeps records and `GROUP BY` folds them — say one (at {span})")]
    LatestBesideGroup {
        /// Where `LATEST BY` was written.
        span: Span,
    },

    /// `ASOF JOIN` with a side that is not a series ordered by event time:
    /// "at or before" needs both sides to carry the moment in their key.
    #[error(
        "`ASOF JOIN` pairs records by time, and `{side}` is not a series declared with `TIME` (at {span})"
    )]
    AsofNeedsTime {
        /// The side's name.
        side: String,
        /// Where the read was written.
        span: Span,
    },

    /// `DEFINE ROLLUP` over something that is not an event-time series.
    #[error("a rollup folds a series declared with `TIME`, and `{name}` is not one (at {span})")]
    RollupNeedsSeries {
        /// The name written.
        name: String,
        /// Where it was written.
        span: Span,
    },

    /// A `COMPUTE` a rollup cannot keep exactly from its row alone.
    #[error(
        "a rollup keeps `count`, `sum`, `min` and `max`, each once and under its own name — `{fold}` is not one; for a mean keep `sum` and `count` and divide (at {span})"
    )]
    RollupFold {
        /// What was written.
        fold: String,
        /// Where it was written.
        span: Span,
    },

    /// A rollup window that is not a whole number of seconds.
    #[error("a rollup's window is a whole number of seconds, longer than nothing (at {span})")]
    RollupWindow {
        /// Where the statement was written.
        span: Span,
    },

    /// A caller's write or delete on a rollup's own table.
    #[error(
        "`{table}` is a rollup, kept by the writes to its series — write the series instead (at {span})"
    )]
    RollupIsDerived {
        /// The rollup.
        table: String,
        /// Where the write was written.
        span: Span,
    },

    /// `DEFINE ROLLUP` inside `BEGIN … COMMIT`: its backfill commits after the
    /// declaration, which an enclosing transaction would hold back.
    #[error(
        "`DEFINE ROLLUP` commits its declaration and then fills the rollup in, so it runs outside `BEGIN … COMMIT` (at {span})"
    )]
    RollupInTransaction {
        /// Where the statement was written.
        span: Span,
    },

    /// Two keys of one window whose rows would share an identity.
    #[error(
        "two keys of one rollup window share a row identity; the write is refused rather than merged (at {span})"
    )]
    RollupKeyCollision {
        /// Where the write was written.
        span: Span,
    },

    /// `DROP SERIES` while rollups are kept of it.
    #[error("rollups are kept of series `{series}` — drop them first (at {span})")]
    RollupsDependOn {
        /// The series.
        series: String,
        /// Where the statement was written.
        span: Span,
    },

    /// A recipient name that did not evaluate to text.
    ///
    /// Reports the **type** and never the value. Every neighbouring variant
    /// renders what it found; this one cannot, because a caller who wrote a
    /// field reference here would have the store quote whatever that field
    /// holds — and on a vault's record that is the one thing this feature
    /// exists to keep unquoted.
    #[error("a recipient is named by text, and this is {found} (at {span})")]
    RecipientIsNotAName {
        /// The type that was supplied.
        found: &'static str,
        /// Where it was written.
        span: Span,
    },

    /// `UPDATE` over a record that is not there.
    #[error("no record {id} (at {span}) — say `CREATE` to write a new one")]
    NoSuchRecord {
        /// The identity as written.
        id: String,
        /// Where it was written.
        span: Span,
    },

    /// `UPDATE … WHERE` over a record that does not say what the caller asserted.
    ///
    /// A **refusal** and not a count, because that is what this verb already is:
    /// [`Error::NoSuchRecord`] above refuses an `UPDATE` over a record that is
    /// not there, so refusing one over a record that is not in the asserted
    /// state is the same assertion one step further in. The failure discards the
    /// work above it in the transaction, which is what makes the clause a
    /// compare-and-set rather than a filter — a caller can act on a lost race
    /// without having remembered to read a number.
    #[error(
        "record {id} does not say what the condition asserts (at {span}) — \
         the `WHERE` on an `UPDATE` is a compare-and-set, so nothing was written"
    )]
    ConditionNotMet {
        /// The identity as written.
        id: String,
        /// Where the condition was written.
        span: Span,
    },

    /// A table used as an edge table that was not declared as one.
    ///
    /// Refused rather than accommodated: an edge table carries an index on each
    /// endpoint, and without them a relation would be written that traversal
    /// could not find. A write nothing can read back is worse than a refusal.
    #[error("{table} is not an edge table — define it with `EDGE` (at {span})")]
    NotAnEdgeTable {
        /// The table as written.
        table: String,
        /// Where it was written.
        span: Span,
    },

    /// A `RELATE` between a pair the edge table does not declare.
    ///
    /// Only an edge table declared `EDGE FROM a TO b` refuses this; the bare
    /// `EDGE` accepts any pair, which is the difference the clause buys. The
    /// refusal is the point of declaring the pair at all: a link into a table
    /// the graph was never told about traverses out of the structure the caller
    /// thought they had, and nothing downstream would be in an error state.
    #[error(
        "{table} does not join these two tables — it was declared with `EDGE FROM … TO …` (at {span})"
    )]
    EndpointsNotDeclared {
        /// The edge table as written.
        table: String,
        /// Where the relation was written.
        span: Span,
    },

    /// A `DROP TABLE` naming a graph's own node collection.
    ///
    /// Refused rather than allowed, and the refusal is what makes the node
    /// collection part of the graph rather than something the graph depends on.
    /// `DEFINE GRAPH g` creates it, under the graph's own name, so the caller
    /// never declared it; dropping it alone would leave a graph that is still
    /// declared, still answers `INFO FOR GRAPH`, and can hold no record — the
    /// state the collection exists to remove, reachable in one statement with
    /// nothing anywhere in an error state. There is no statement that puts it
    /// back, because `DEFINE GRAPH g` would refuse: the graph is already there.
    ///
    /// The refusal names `DROP GRAPH` rather than only saying no, which is what
    /// makes it a signpost. A table the caller attached with `IN` is a different
    /// thing and still drops freely: that clause is one the caller wrote and may
    /// withdraw.
    #[error(
        "`{table}` is graph `{graph}`'s own node collection, not a table of its \
         own (at {span}) — write `DROP GRAPH {graph}` to remove the graph and \
         everything it holds"
    )]
    TableBelongsToGraph {
        /// The table as written.
        table: String,
        /// The graph whose collection it is.
        graph: String,
        /// Where the statement is.
        span: Span,
    },

    /// An edge kind named an endpoint table that does not belong to its graph.
    ///
    /// Refused rather than allowed, because this refusal is what **bounds** a
    /// walk. A kind whose far side sat outside the graph would let a traversal
    /// leave the structure it was told to stay inside, and the walk would still
    /// answer — with records the graph does not contain.
    #[error("table `{table}` does not belong to graph `{graph}`")]
    EndpointOutsideGraph {
        /// The endpoint table as written.
        table: String,
        /// The graph the edge kind was declared in.
        graph: String,
        /// Where the declaration was written.
        span: Span,
    },

    /// A graph traversal was asked for inside a read of the past.
    ///
    /// Edges are followed through the edge table's direction indexes, and an
    /// index entry carries no version: it describes the committed tail. Every
    /// other index-served read answers this by falling back to a scan, but a
    /// traversal has nothing to fall back to — the indexes are the mechanism,
    /// not a shortcut past it.
    ///
    /// So the choice is a refusal or a set of records reached through today's
    /// edges and read at yesterday's snapshot. The second is a wrong answer with
    /// nothing to distinguish it from a right one, in the query shape whose
    /// working nobody can see.
    #[error(
        "a graph traversal cannot be read at an earlier version: edges{} are \
         indexed at the present (at {span})",
        if table.is_empty() { String::new() } else { format!(" in {table}") }
    )]
    NoHistoricalTraversal {
        /// The first edge table in the traversal, when one was named.
        table: String,
        /// Where the traversal was written.
        span: Span,
    },

    /// A graph traversal met an edge table where a transaction across leaders
    /// is part-way on this node (Q-919).
    ///
    /// The traversal follows the table's direction indexes, which hold that
    /// transaction's resolution while readers do not see it yet, or miss an
    /// edge readers already see. A traversal has no scan to fall back to, so
    /// it is refused rather than answered through edges the read itself would
    /// not show. Retriable: it lasts until the transaction's last part lands
    /// here, and the store reports it rather than waiting.
    #[error(
        "edges in `{table}` are settling a transaction across leaders on this node — \
         read again in a moment (at {span})"
    )]
    AcrossSettling {
        /// The edge table.
        table: String,
        /// Where the traversal was written.
        span: Span,
    },

    /// A weighted path met an edge whose weight is not a cost (G055 W6).
    ///
    /// A weight that is absent drops the edge — absence narrows — but one that is
    /// not a number, or is below zero, is a mistake in the data the path would
    /// otherwise answer around: a negative step makes "the cheapest path" a walk
    /// that loops, and every shortest-path method refuses it.
    #[error(
        "`WEIGHT {field}` met an edge whose `{field}` is {found}, and a step's cost is a number of zero or more (at {span})"
    )]
    PathWeight {
        /// The weight field.
        field: String,
        /// What it held.
        found: String,
        /// Where the path was written.
        span: Span,
    },

    /// `PATH TO` over an edge table rather than a declared edge kind (G055 W6).
    ///
    /// A path search walks backwards as well as forwards, which is the adjacency
    /// a declared graph keeps beside each node in both directions; an edge table
    /// is a set of records found through an index.
    #[error(
        "`PATH TO` walks a declared edge kind — `{table}` is an edge table; declare the graph with `DEFINE EDGE … IN <graph>` (at {span})"
    )]
    PathOverEdgeTable {
        /// The edge table named.
        table: String,
        /// Where it was named.
        span: Span,
    },

    /// A read needs records this node does not hold (G031 S3.3, ADR-0081).
    ///
    /// The node was served part of what its catalog describes — one shard of a
    /// split table, or one shard of a database whose other tables it therefore
    /// holds none of. Answering from what it has would present a part as the
    /// whole with nothing in an error state, so the read is refused and the
    /// shards it lacks are named. An empty list means it holds none of the
    /// table at all.
    ///
    /// When this node knows a peer holding the whole table it names it, and a
    /// surface that can redirect turns the refusal into one (G051 C4,
    /// ADR-0101) — to a client that greeted minor 0 it stays this refusal.
    #[error(
        "this node does not hold all of `{table}`{} — read it on a node that holds \
         the whole table{}, or name a span of identities inside what this one holds",
        if shards.is_empty() {
            String::new()
        } else {
            format!(
                ": shard{} {} {} held elsewhere",
                if shards.len() == 1 { "" } else { "s" },
                shards.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "),
                if shards.len() == 1 { "is" } else { "are" }
            )
        },
        holder.as_ref().map_or_else(String::new, |peer| format!(" ({} does)", peer.endpoint))
    )]
    NotHeldHere {
        /// The table asked for.
        table: String,
        /// The shards the read needs and this node does not hold.
        shards: Vec<u32>,
        /// A peer holding the whole table that this node has heard serving, when
        /// it knows one.
        holder: Option<crate::Peer>,
    },

    /// A shard this node lacks could not be fetched from its leader (G033).
    ///
    /// The whole read is refused, because an answer missing one shard's records
    /// is a partial answer that looks whole.
    #[error(
        "shard {shard} of `{table}` could not be fetched from its leader, so the read is \
         refused rather than answered without it: {why}"
    )]
    NotGathered {
        /// The table asked for.
        table: String,
        /// The shard nobody answered for.
        shard: u32,
        /// What refused, in its own words.
        why: String,
    },

    /// The leader asked for a shard holds a different map of the table: the
    /// shard was retired there, or minted by a change this node has not applied
    /// yet (ADR-0095 D4).
    ///
    /// Retriable, and not the same as a leader that did not answer: the repair
    /// is to read the map again once this node has applied the change.
    #[error(
        "shard {shard} of `{table}` is not in the map the node leading it holds — one \
         of the two maps has moved; read again once this node has applied the change"
    )]
    ShardMapMoved {
        /// The table asked for.
        table: String,
        /// The shard asked for.
        shard: u32,
        /// A peer holding the whole table that this node has heard serving, when
        /// it knows one — the node a surface redirects the read to (G051 SG3).
        holder: Option<crate::Peer>,
    },

    /// A gathered read would hold more records than a node holds in memory (G033).
    #[error(
        "reading `{table}` here would gather more than {most} records, and this build \
         refuses rather than shorten the answer — read a span of identities inside the \
         shards this node holds, or read on a node that holds the whole table"
    )]
    GatheredTooMuch {
        /// The table asked for.
        table: String,
        /// The ceiling.
        most: usize,
    },

    /// An edge was given properties that are not a set of named fields.
    ///
    /// An edge record already carries `out` and `in`; anything else it holds has
    /// to be named, so there is nowhere for a bare value to go. Refused where it
    /// is written rather than dropped on the way to the store.
    #[error("an edge's properties must be an object, not {found} (at {span})")]
    EdgePropertiesNotAnObject {
        /// The type that was given instead.
        found: &'static str,
        /// Where it was written.
        span: Span,
    },

    /// A range bound that no record identity can be.
    #[error("a key range is bounded by record identities (at {span})")]
    InvalidKeyBound {
        /// Where the bound was written.
        span: Span,
    },

    /// A condition that is not a boolean.
    ///
    /// Every operator that composes a condition answers with one, so this only
    /// happens when a bare path or literal stands where a question was meant.
    /// `WHERE tags` is not a question with a false answer; it is a question that
    /// was not finished, and an empty result would hide that.
    #[error("a condition must be a boolean, not {found} (at {span})")]
    ConditionNotBoolean {
        /// The type that stood there instead.
        found: &'static str,
        /// Where it was written.
        span: Span,
    },

    /// An arithmetic operator applied to something that is not a number.
    #[error("`{operator}` needs numbers, not {left} and {right} (at {span})")]
    NotArithmetic {
        /// The operator as written.
        operator: &'static str,
        /// The left operand's type.
        left: &'static str,
        /// The right operand's type.
        right: &'static str,
        /// Where the operator is.
        span: Span,
    },

    /// Arithmetic that has no answer: an overflow, or a division by zero.
    ///
    /// A failure rather than a value, because a wrapped integer or an infinity
    /// written into a record is a number nobody meant, and by the time anyone
    /// notices it is stored.
    #[error("`{operator}` has no answer here: {reason} (at {span})")]
    ArithmeticFailed {
        /// The operator as written.
        operator: &'static str,
        /// Why there is no answer.
        reason: &'static str,
        /// Where the operator is.
        span: Span,
    },

    /// A function argument holding the wrong kind of value.
    #[error("{function} wants {expected} as argument {at}, not {found} (at {span})")]
    WrongArgument {
        /// The function called.
        function: Function,
        /// Which argument, counting from one.
        at: usize,
        /// What it wanted.
        expected: &'static str,
        /// What it found.
        found: &'static str,
        /// Where the call is.
        span: Span,
    },

    /// A function that could not answer for a reason of its own.
    #[error("{function} has no answer here: {reason} (at {span})")]
    CallFailed {
        /// The function called.
        function: Function,
        /// Why there is no answer.
        reason: &'static str,
        /// Where the call is.
        span: Span,
    },

    /// An expiry a key-value write cannot take (G035): not a duration or a
    /// datetime, a `SET … EXPIRE` that is not in the future, or an instant
    /// past what a millisecond clock can hold.
    #[error("{reason} (at {span})")]
    InvalidExpiry {
        /// Why the expiry cannot be taken.
        reason: &'static str,
        /// Where the expiry is.
        span: Span,
    },

    /// The store could not produce a record identity.
    ///
    /// Its own refusal rather than [`Error::CallFailed`], which names the
    /// function that failed: the caller of an `INSERT` called nothing. Reporting
    /// this as `rand::uuid() has no answer here` would send them looking through
    /// a statement that contains no such call.
    #[error("the store cannot produce a record identity: {reason} (at {span})")]
    IdentityUnavailable {
        /// Why there is no identity.
        reason: &'static str,
        /// Where the statement is.
        span: Span,
    },

    /// A join token could not be made (ADR-0108 D9).
    ///
    /// No fallback, for [`Error::IdentityUnavailable`]'s reason: a token from a
    /// weaker source is a credential somebody else can produce.
    #[error("the store cannot make a join token: {reason} (at {span})")]
    TokenUnavailable {
        /// Why there is no token.
        reason: &'static str,
        /// Where the statement is.
        span: Span,
    },

    /// A read asked to be answered by a node fresher than this cluster can know.
    ///
    /// A write, or a `COMMIT`, asked to be acknowledged at a level its namespace
    /// does not let a request lower to (ADR-0106 D2).
    ///
    /// Refused before anything is written. A namespace's level is a default
    /// somebody relies on, and one any caller can silently lower is not one —
    /// so lowering it takes the namespace's own `OR WEAKER`.
    #[error(
        "this write asks for `{asked}` (at {span}), and namespace `{namespace}` \
         waits for `{stated}`: write the namespace's level or a stronger one, or \
         declare `ALTER NAMESPACE {namespace} {stated} OR WEAKER` to let a request \
         ask for less"
    )]
    AcknowledgeBelowNamespace {
        /// The level the request asked for.
        asked: tessari_types::Acknowledge,
        /// The namespace's own.
        stated: tessari_types::Acknowledgement,
        /// The namespace the write is in.
        namespace: String,
        /// Where the request is.
        span: Span,
    },

    /// A `MAJORITY` write the declared voters cannot acknowledge, refused
    /// before it commits (ADR-0106 D7).
    ///
    /// A voter whose subscription does not cover the write can never hold it.
    /// When the ones that can do not make up a majority, accepting the write and
    /// timing it out would commit something the caller is then told failed.
    #[error(
        "this write waits for {needed} of {voters} voter(s) to hold it (at \
         {span}), and only {holders:?} besides this node subscribe to where it \
         is written: nothing was written. Declare a subscription that covers it \
         on enough voters, or write it with `ACKNOWLEDGE LEADER` if the namespace \
         allows that"
    )]
    MajorityUnreachable {
        /// The voters, other than this node, that do subscribe to it.
        holders: Vec<String>,
        /// How many voters the range has, this node included.
        voters: usize,
        /// How many of them a majority is.
        needed: usize,
        /// Where the write is.
        span: Span,
    },

    /// A `MAJORITY` write committed here that not enough voters acknowledged in
    /// time (ADR-0106 D4).
    ///
    /// **The write IS committed on this node**, and this refusal says so rather
    /// than answering success: whether it survives a failover depends on copies
    /// this node could not confirm. Retrying is safe only for a write that
    /// says the same thing twice.
    #[error(
        "this write (at {span}) is committed on this node at sequence {sequence}, \
         and only {held_by:?} besides it acknowledged holding it in time, where \
         {needed} copies were needed: it may not survive a failover"
    )]
    NotAcknowledgedInTime {
        /// Where it was committed in the log.
        sequence: u64,
        /// The voters, other than this node, that acknowledged it.
        held_by: Vec<String>,
        /// How many copies were needed, this node's included.
        needed: usize,
        /// Where the write is.
        span: Span,
    },

    /// A transaction across leaders was not committed, and nothing of it was
    /// applied anywhere (ADR-0112): a participant refused its prepare, or the
    /// record was aborted before the decision landed. The reason is the
    /// refusal as the participant gave it.
    #[error(
        "the transaction across leaders (at {span}) was not committed, and nothing of it applies: {reason}"
    )]
    AcrossAborted {
        /// Why, in the words of the node that refused.
        reason: String,
        /// Where the commit is.
        span: Span,
    },

    /// A transaction across leaders whose decision was sent and not confirmed
    /// (ADR-0112 D4, D7). Its record decides it: committed if the decision
    /// landed, aborted once the record's liveness lapses if it did not. The
    /// caller is told it does not know rather than told either.
    #[error(
        "the transaction across leaders (at {span}) is in doubt — its record decides it: {reason}"
    )]
    AcrossInDoubt {
        /// What could not be confirmed.
        reason: String,
        /// Where the commit is.
        span: Span,
    },

    /// A transaction across leaders asked of a node that knows no peers to
    /// carry its records to.
    #[error(
        "this node carries nothing to other nodes, so the commit at {span} cannot reach across leaders"
    )]
    AcrossUnavailable {
        /// Where the commit is.
        span: Span,
    },

    /// A staleness bound says how far behind an answering node may be. A bound
    /// tighter than the interval at which a node learns anything about its peers
    /// is a promise nothing can check — it would be enforced against a picture
    /// whose own age exceeds the tolerance being compared to it.
    ///
    /// **The floor is named in the refusal, and that is the half that matters.**
    /// A caller told only that their bound was too tight cannot write a
    /// statement that would be accepted; a caller told the floor can.
    #[error(
        "a staleness bound of {written} (at {span}) is tighter than this cluster \
         can know about itself: the floor is {floor}s"
    )]
    StalenessBelowFloor {
        /// The bound as the statement wrote it.
        written: String,
        /// The tightest bound that would have been accepted, in seconds.
        floor: u64,
        /// Where the clause is.
        span: Span,
    },

    /// A read named a tolerance for staleness that no copy in reach satisfies.
    ///
    /// `05_blocking-decisions.md` §C-05 decided both halves of this. Routing
    /// **excludes** a node beyond the bound rather than serving it with a
    /// marker, because a marker nobody is obliged to read is not a guarantee —
    /// so a node outside the bound does not answer. And a read no node can
    /// satisfy is **refused**, not sent to the leader: a silent promotion turns
    /// a latency feature into a leader stampede exactly when the cluster is
    /// already struggling, which is when every replica is behind at once.
    ///
    /// **It is the cluster that is short, not the statement.** The bound cleared
    /// the floor, so it is a bound this cluster could in principle honour; what
    /// is missing is a copy young enough to honour it with. A refusal that read
    /// as a grammar complaint would send the caller to rewrite a statement that
    /// was never wrong.
    #[error(
        "a staleness bound of {written} (at {span}) admits no copy in reach: \
         neither this node's own copy nor any peer it has heard from is within \
         it, and a read no node can satisfy is refused rather than sent to the \
         leader"
    )]
    NoCopyWithinStaleness {
        /// The bound as the statement wrote it.
        written: String,
        /// Where the clause is.
        span: Span,
    },

    /// A read carrying a staleness bound belongs on another node, and this is
    /// which one.
    ///
    /// The third answer, and it is a *success* wearing an error's clothes. C-07
    /// settled the shape: **any node answers any request by serving it or by
    /// returning a redirect carrying the node that should, and no node proxies
    /// on a client's behalf.** A client holding no routing state at all is
    /// therefore always correct, which is what makes a minimal third-party
    /// client possible; caching the map and refreshing it on a redirect is the
    /// optimisation and never the contract.
    ///
    /// **The node id travels beside the address**, because a redirect naming
    /// only a place cannot be checked on arrival: a client that dialled it and
    /// met a different node would have no way to notice.
    ///
    /// **It says what it did not do.** A caller who cannot tell a redirect from
    /// a silent proxy has no way to know whether this node is now holding their
    /// read open against a peer, which is the failure mode that made *no node
    /// proxies* worth deciding.
    #[error(
        "{because} (at {span}) is not satisfied by this node, and {endpoint} \
         satisfies it: read it there. This node redirects rather than fetching \
         on your behalf. The node to expect is {}",
        tessari_types::RecordId::Uuid(*node)
    )]
    ReadIsElsewhere {
        /// What the read asked for that this node could not give it, in the
        /// statement's own words.
        ///
        /// It carries the REASON rather than a bound, because there are now two
        /// of them — a staleness bound this node's copy is outside, and an
        /// `ANSWERED BY LEADER` on a node that does not lead. One variant and
        /// not two deliberately: the wire turns this into frame kind 13 and
        /// HTTP into a 307, and a second variant would be a second arm in each
        /// of those, where forgetting one answers a redirect as a plain refusal
        /// and nothing anywhere is in an error state.
        because: String,
        /// The address to dial — the same string the declaration carried.
        endpoint: String,
        /// Who was last heard there, so the redirect is checkable on arrival.
        node: [u8; tessari_encoding::NODE_ID_LEN],
        /// The leadership that node itself last claimed was current.
        ///
        /// Carried so the surface rendering this can date the redirect. It is
        /// the named peer's own claim and not this node's: a node redirecting a
        /// bounded read is one whose own copy failed the bound, and it may hold
        /// no leadership at all.
        epoch: tessari_types::Epoch,
        /// Where the clause is.
        span: Span,
    },

    /// A read asked to be answered by the leader, and this node knows of none.
    ///
    /// Its own roles say it does not lead, and no peer it has greeted claims to
    /// either. Refused rather than answered here, for the reason
    /// [`Self::NoCopyWithinStaleness`] is refused rather than promoted: a read
    /// that said where it had to come from is not served by coming from
    /// somewhere else and saying nothing.
    ///
    /// **The remedy is different from a staleness refusal's**, which is why it
    /// is a different class. A bound that nothing satisfies is widened or waited
    /// out; this one is a cluster with no writable member, and it is answered by
    /// declaring one — `DEFINE REPLICA … ROLES writable`.
    #[error(
        "`ANSWERED BY LEADER` (at {span}) cannot be satisfied: this node does \
         not lead, and no peer it has greeted says it does. Declare one with \
         `DEFINE REPLICA … ROLES writable`"
    )]
    NoLeaderKnown {
        /// Where the clause is.
        span: Span,
    },

    /// A namespace was defined without saying how many copies of it to keep, on
    /// a store that has somewhere to keep them.
    ///
    /// ADR-0060 requires the clause so that a single-copy namespace is a
    /// decision somebody took rather than a default nobody saw. W211 shipped it
    /// optional, because with no peers declared the only clause an operator
    /// could write was `REPLICATION NONE`, and a grammar that forces everybody
    /// to type a refusal teaches them to decline without reading — which is the
    /// inherited default the ADR abolishes, in a costume.
    ///
    /// **So the obligation is conditional on the risk existing.** A store that
    /// declares no peers has nowhere to put a second copy, and there the bare
    /// form is accepted and stored as *never stated*. A store that declares one
    /// is a cluster, and there declining a copy is a choice — so it has to be
    /// written down.
    ///
    /// **It is refused at execute and not at parse**, because how many peers
    /// this store declares is a fact about the cluster and the parser has no
    /// cluster. That is the placement `StalenessBelowFloor` already settled.
    ///
    /// **The refusal names both accepted clauses**, for the reason the staleness
    /// floor is named in its own: a caller told only that something is missing
    /// cannot write a statement that would be accepted.
    ///
    /// It does not fire for `IF NOT EXISTS` against a namespace that is already
    /// there. That branch creates nothing, so there is no unstated namespace to
    /// prevent, and refusing it would break every idempotent bootstrap script.
    #[error(
        "`{namespace}` (at {span}) does not say how many copies to keep, and this \
         store declares {peers} peer(s) that could hold one: write \
         `REPLICATION NONE` to keep a single copy deliberately, or \
         `REPLICATION FACTOR <n>` to keep more"
    )]
    ReplicationUnstated {
        /// The namespace as the statement named it.
        namespace: String,
        /// How many peers this store declares — why the clause is now required.
        peers: usize,
        /// Where the name is.
        span: Span,
    },

    /// A `SELECT` named a vault as its source.
    ///
    /// Refused rather than answered, and what it would have answered is worth
    /// saying: **ciphertext**, because the sealed envelope is the stored value.
    /// So this refusal protects nothing — the confidentiality is already closed
    /// by where the sealing happens — and exists because a read that quietly
    /// returns opaque bytes teaches a caller that the vault is broken, while a
    /// refusal naming `REVEAL` teaches them the language.
    #[error("`{table}` is a vault: read it with `REVEAL … FROM {table}:…` (at {span})")]
    NotReadBySelect {
        /// The vault named.
        table: String,
        /// Where it was named.
        span: Span,
    },

    /// An index was declared over a field the vault seals.
    ///
    /// This one **does** protect something. An index over a secret field is a
    /// searchable copy of it: the term dictionary holds the analysed value, the
    /// postings say which records share it, and a caller who may not read the
    /// field can still ask whether any record holds a given one, a term at a
    /// time. That is the whole plaintext, obtained through a structure nobody
    /// thinks of as a read.
    #[error("`{field}` is a secret field of vault `{table}` and cannot be indexed (at {span})")]
    NotIndexable {
        /// The field named.
        field: String,
        /// The vault it is on.
        table: String,
        /// Where the declaration is.
        span: Span,
    },

    /// `SECRET` was declared on a field of something that is not a vault.
    ///
    /// Refused rather than honoured, because there would be no key to honour it
    /// with: the hierarchy that seals a field hangs off the vault's own key, and
    /// an ordinary table has none. Accepting the word would produce a field the
    /// store believes is sealed and writes in the clear — which is the exact
    /// failure the marker exists to prevent, wearing the marker's own name.
    #[error("`SECRET` needs a vault: `{table}` is not one (at {span})")]
    SecretNeedsVault {
        /// The table named.
        table: String,
        /// Where the declaration is.
        span: Span,
    },

    /// A vault was asked to become schemaless.
    ///
    /// Refused, because strictness is the only thing that makes *declared* and
    /// *sealed* the same set. The marker that seals a field is `SECRET` on its
    /// declaration; a field nobody declared carries no marker, so a schemaless
    /// vault writes it in the clear beside the sealed ones — inside the store
    /// whose promise is that it holds nothing readable.
    ///
    /// Refusing the *transition* rather than only choosing the right default is
    /// the point: a default is one statement deep, and this is the statement.
    #[error(
        "vault `{table}` cannot be made schemaless: a field nobody declared is a field nothing seals (at {span})"
    )]
    VaultIsStrict {
        /// The vault named.
        table: String,
        /// Where the statement is.
        span: Span,
    },

    /// `REVEAL` was asked for a field that is not declared `SECRET`.
    ///
    /// Refused rather than answered in the clear. `REVEAL` returns plaintext, so
    /// a caller reading its answer cannot tell which entries were ever sealed —
    /// and a verb that sometimes returns a secret and sometimes returns whatever
    /// happened to be beside it is one whose output nobody can reason about.
    ///
    /// The field name is named and **the value is not**, which is the rule every
    /// refusal from a vault statement keeps: a name is what the caller already
    /// typed, and a value is what they were asking to be told.
    #[error("`{field}` is not a secret field of vault `{vault}` (at {span})")]
    NotASecret {
        /// The field asked for.
        field: String,
        /// The vault it was asked of.
        vault: String,
        /// Where the statement is.
        span: Span,
    },

    /// A value the cast asked for cannot hold.
    ///
    /// Distinct from [`Error::WrongArgument`], because the argument's *type* is
    /// not the complaint: `type::int` takes a string happily and answers `42`
    /// for `'42'`. What failed is this particular value, so this is the one
    /// refusal in the function surface that names the value rather than its
    /// type — a message saying only "wants a number, found string" would be
    /// describing an argument the function accepts.
    ///
    /// **Why it refuses instead of answering `none`.** An absent argument
    /// already answers `none`, and a store built on the distinction between
    /// "the field is not there" and "the field is there and holds nothing"
    /// cannot then use `none` for a third thing — "the field is there, holds
    /// something, and that something is not what you asked for". A caller who
    /// wants the lenient reading can say so with `IF`; a caller who gets it by
    /// default has no way back.
    ///
    /// # The one refusal in this file that quotes a value, and what that costs
    ///
    /// Every other value-bearing variant here carries a `found: &'static str` —
    /// a type name. This one carries the value, deliberately and for the reason
    /// above. That makes it the single place where a **sealed** value could
    /// reach a message, which is criterion W4 of the vault goal.
    ///
    /// It cannot happen through `REVEAL`, which returns plaintext to its caller
    /// and casts nothing. It cannot happen through any generic read, because
    /// what those read is ciphertext. The route that would exist is a cast
    /// applied to a plaintext a script had already revealed into a binding — at
    /// which point the caller holds the value and the message tells them nothing
    /// they did not have. **That is the argument, and an argument is not a
    /// test**; the assertion that submits a known secret and greps the whole
    /// refusal for it is a W122 row, and until it runs W4 is unproven rather
    /// than passing.
    #[error("{function} cannot read {value} as {target} (at {span})")]
    NotCastable {
        /// The cast called.
        function: Function,
        /// The value as it was written, quoted when it is text.
        value: String,
        /// The kind that was asked for.
        target: &'static str,
        /// Where the call is.
        span: Span,
    },

    /// A default that cannot satisfy the type its own field declares.
    ///
    /// Checked when the declaration is made rather than when it first bites: by
    /// the time a write failed on it, the declaration would be in the catalog
    /// and the failure would look like the write's fault.
    #[error("the default for {field} is {found}, and the field is declared {declared} (at {span})")]
    DefaultDoesNotMatch {
        /// The field being declared.
        field: String,
        /// The type it declares.
        ///
        /// Owned rather than `&'static str`: a literal union spells itself as
        /// its members, so not every declared type is a word known at compile
        /// time.
        declared: String,
        /// The type its default evaluated to.
        found: &'static str,
        /// Where the field was named.
        span: Span,
    },

    /// A fold given something it cannot fold.
    ///
    /// A silent skip would make a wrong total look like a right one, which is
    /// the same reason arithmetic refuses a non-number rather than ignoring it.
    #[error("`{fold}` cannot fold {found} (at {span})")]
    NotSummable {
        /// The fold as written.
        fold: &'static str,
        /// What it was given, or why it could not answer.
        found: &'static str,
        /// Where the fold is.
        span: Span,
    },

    /// A statement was run against a closed store with nobody signed in.
    ///
    /// "I do not know you" — a different answer from "I know you and no", and a
    /// client needs to tell them apart to know whether signing in would help.
    #[error("this store requires a signed-in user (at {span})")]
    NotSignedIn {
        /// Where the statement is.
        span: Span,
    },

    /// An anonymous append to a `PUBLIC` topic went past the rate it declares.
    ///
    /// Retriable after a wait: the allowance is earned back over `per`. The rate
    /// is this node's, so a cluster admits it once per node.
    #[error(
        "topic {topic} takes at most {rate} anonymous messages per {per} on this node (at {span})"
    )]
    TopicRateExceeded {
        /// The topic, as the statement named it.
        topic: String,
        /// How many messages the topic takes per window.
        rate: u64,
        /// The window, as a duration literal.
        per: String,
        /// Where the statement is.
        span: Span,
    },

    /// The signed-in user's role does not allow the statement.
    #[error("{} {role} may not {needs} (at {span})", article(role))]
    RoleForbids {
        /// The role the user holds.
        role: &'static str,
        /// What the statement needed.
        needs: &'static str,
        /// Where the statement is.
        span: Span,
    },

    /// The statement writes, and this node does not accept writes.
    ///
    /// Not a permission failure: the caller may well be allowed to write, and
    /// on the leader the same script would run. It says *where*, not *who* —
    /// which is why it is separate from [`Self::RoleForbids`] and why the
    /// message says the node rather than the user.
    #[error("this node does not accept writes (at {span})")]
    NotWritable {
        /// Where the statement is.
        span: Span,
    },

    /// Two peers are declared writable, so a forward has no single destination.
    ///
    /// A configuration fault rather than a statement fault, which is why it
    /// carries names instead of a span: nothing about where the statement sits
    /// would help, and the two names are what an operator has to go and fix.
    /// Refused rather than resolved by picking one, because choosing between
    /// them is choosing a leader, and two leaders accepting writes is the split
    /// brain replication exists to prevent.
    #[error("two peers are declared writable, `{named}` and `{also}`")]
    ManyWritablePeers {
        /// One of them.
        named: String,
        /// The other.
        also: String,
    },

    /// A signin that did not match.
    ///
    /// One message for a wrong name and a wrong password alike: telling them
    /// apart tells an attacker which half to keep guessing at.
    #[error("no user of that name and password")]
    SignInRefused,

    /// A signin this node declined to attempt at all.
    ///
    /// Either the identity has missed too many times in a row and is waiting, or
    /// this node is already running as many password verifications as it will.
    /// One answer for both, because both mean the same thing to a client — come
    /// back — and separating them would tell an attacker which of the two limits
    /// they had reached and therefore which one to work around.
    ///
    /// Distinct from [`Error::SignInRefused`], and that distinction is
    /// deliberate: this one says nothing about whether the credential was right,
    /// so a client can tell "wait" from "wrong" and stop retrying a password that
    /// will never work. An operator reading the log gets the two apart there,
    /// where an attacker is not.
    #[error("this node is not taking a sign-in for that user right now")]
    SignInThrottled,

    /// A statement that changes authority or membership, or reads the whole
    /// store, arrived through another node acting for its caller (ADR-0108 D2).
    ///
    /// Such a statement is taken only from a caller who proved a credential to
    /// the node that judges it, so a member cannot widen a user, add a member or
    /// take the store's backup by asking another node to act for somebody.
    #[error(
        "`{statement}` is not taken through another node; send it to the node that \
         leads, signed in there"
    )]
    MayNotTravel {
        /// The statement, as the language spells it.
        statement: &'static str,
    },

    /// A passphrase was presented while guesses at it are being made to wait
    /// (ADR-0092 D2).
    ///
    /// Its own refusal rather than [`Error::SignInThrottled`], whose words name
    /// a user: what is being guessed here is the store's passphrase, and the
    /// caller is told to wait without being told whether this try was right.
    #[error("this node is not taking a vault passphrase right now")]
    PassphraseThrottled,

    /// A passphrase change on a store that has never been unsealed.
    ///
    /// There is no root to re-wrap. Creating one is what the first
    /// `UNSEAL VAULT` does, and it says so; a change that quietly initialised
    /// instead would make its `FROM` passphrase mean nothing.
    #[error("this store has no vault passphrase yet: the first `UNSEAL VAULT` sets one")]
    NoVaultRoot,

    /// A statement naming a vault that opens with the store's passphrase
    /// (ADR-0093 D3).
    ///
    /// Refused rather than taken as the store-wide statement: unsealing the
    /// store by naming one vault would open every other vault in the store's
    /// custody too, which the caller did not name.
    #[error(
        "vault {vault} opens with the store's passphrase: use `UNSEAL VAULT`, `SEAL VAULT` or \
         `CHANGE VAULT PASSPHRASE` without a name (at {span})"
    )]
    VaultUsesStorePassphrase {
        /// The vault as written.
        vault: String,
        /// Where the statement is.
        span: Span,
    },

    /// A score was asked for where there is no collection to measure against.
    ///
    /// Not answered with zero, and not answered against whatever records
    /// happened to be read: both produce an ordering that looks exactly like a
    /// ranking and is not one. See `crate::rank` for the argument in full.
    #[error(
        "cannot rank by {field:?}: a score measures a record against its collection, \
         and that needs a search index on the field (at {span})"
    )]
    NoSearchIndex {
        /// The path as written.
        field: String,
        /// Where it was written.
        span: Span,
    },

    /// A `MATCHES PREFIX` whose prefix is shorter than the store will serve.
    ///
    /// Refused rather than answered slowly, and refused **before any access path
    /// is chosen**, so an index cannot change whether the query runs. The cost
    /// of a prefix is the size of its expansion, and a one- or two-character
    /// prefix expands to a large fraction of the vocabulary — an answer nobody
    /// can use, paid for in full, on the query a frustrated reader retries.
    ///
    /// The limit is stated in the message because a refusal that does not say
    /// what would have worked leaves the caller guessing at it.
    #[error(
        "the prefix {prefix:?} is shorter than {minimum} characters, \
         which is the shortest this store will expand (at {span})"
    )]
    PrefixTooShort {
        /// The prefix as it was analysed, not as it was typed.
        prefix: String,
        /// The shortest prefix that would have been served.
        minimum: usize,
        /// Where the query was written.
        span: Span,
    },

    /// A quoted phrase whose trailing slop marker does not parse.
    ///
    /// Refused rather than answered, and refused **before any access path is
    /// chosen**, for the same reason [`Error::PrefixTooShort`] is: whether a
    /// query runs must not depend on whether an index happens to exist.
    ///
    /// The alternative is what this store did before phrases existed, and it is
    /// the failure the whole operator was built to remove. `~x` is not a slop
    /// marker, so the characters fall through to the analyzer, `x` becomes a
    /// term of its own, no record holds it, and the query answers `[]` — an
    /// empty answer that looks exactly like "nothing matched" and is really
    /// "you typed something I did not understand". A caller cannot tell those
    /// apart, and nothing in the answer invites them to look.
    ///
    /// Reading it as slop 0 would be the same mistake wearing a helpful face: it
    /// answers a question the caller did not ask, and it does so silently.
    #[error(
        "{marker:?} is not a slop marker; write `~` followed by a whole number, \
         as in \"a phrase\"~2, or leave it off for an exact phrase (at {span})"
    )]
    MalformedSlop {
        /// The tail as written, after the phrase's closing quote.
        marker: String,
        /// Where the query was written.
        span: Span,
    },

    /// A search query that excludes terms and requires none.
    ///
    /// An inverted index enumerates **presence**, so `NOT babbage` names the
    /// complement of a posting list — every record in the table, which the index
    /// cannot produce. The two honest answers are a full scan and a refusal, and
    /// this store refuses, exactly as it refuses a score over a field with no
    /// search index: a statement that did not run beats one that quietly read the
    /// whole table because a word was spelled `NOT`.
    ///
    /// Raised **before any access path is chosen**, and before the catalog is
    /// read at all, so the refusal cannot come to depend on whether an index
    /// exists (ADR-0046, extended to errors).
    #[error(
        "a search query cannot exclude terms without requiring one; write at \
         least one word to match, as in `ada NOT babbage` (at {span})"
    )]
    NegationWithoutTerm {
        /// Where the query was written.
        span: Span,
    },

    /// `FROM SEARCH` was asked something that is not text.
    #[error("a search is asked text, and this is {found} (at {span})")]
    SearchNeedsText {
        /// What it was.
        found: &'static str,
        /// Where.
        span: Span,
    },

    /// An `ORDER BY` on an ungrouped `FROM SEARCH`, whose order is its ranking.
    #[error(
        "a search answers in the order it ranks its records; write the ordering \
         over a grouped read, or rank by `search::score()` (at {span})"
    )]
    SearchIsItsOwnOrder {
        /// The ordering.
        span: Span,
    },

    /// `search::score()`, `search::table_name()` or `search::snippet()` outside a
    /// `FROM SEARCH`, where there is no ranked record to answer about.
    #[error(
        "this function answers about a record a `FROM SEARCH` ranked, and this \
         read is not one (at {span})"
    )]
    NotSearched {
        /// The call.
        span: Span,
    },

    /// An event of this name is already defined on the table (ADR-0110).
    #[error("an event named `{event}` is already defined on `{table}` (at {span})")]
    EventExists {
        /// The event.
        event: String,
        /// The table.
        table: String,
        /// Where it was written.
        span: Span,
    },

    /// An event's body refused, so the write that ran it is refused (ADR-0110
    /// D4). The cause is the body's own refusal, unchanged.
    #[error("event `{event}` on `{table}` refused the write: {cause}")]
    EventFailed {
        /// The event whose body refused.
        event: String,
        /// The table it is defined on.
        table: String,
        /// What the body refused with.
        cause: Box<Error>,
    },

    /// A chain of events ran deeper than the limit (ADR-0110 D5).
    #[error(
        "event `{event}` would run {limit} events deep — a chain of events that writes back into itself; give the body a `WHEN` that excludes its own change"
    )]
    EventDepth {
        /// The event that would have run past the limit.
        event: String,
        /// The limit.
        limit: u8,
    },

    /// An event on a table kind that cannot carry one (ADR-0110 D9).
    #[error(
        "`{table}` is a {kind}, and only tables, collections and edge tables carry events (at {span})"
    )]
    EventOnKind {
        /// The table.
        table: String,
        /// Its kind.
        kind: &'static str,
        /// Where it was named.
        span: Span,
    },

    /// A search of this name already exists in the database.
    #[error("a search named `{name}` already exists (at {span})")]
    SearchExists {
        /// The name.
        name: String,
        /// Where it was written.
        span: Span,
    },

    /// A search named one table twice: its fields are listed once, together.
    #[error("the search names `{table}` twice; list its fields once, after one `ON` (at {span})")]
    SearchNamesTableTwice {
        /// The table.
        table: String,
        /// The second naming.
        span: Span,
    },

    /// A search named one field of a table twice.
    #[error("the search names the field `{field}` twice (at {span})")]
    SearchNamesFieldTwice {
        /// The field.
        field: String,
        /// The second naming.
        span: Span,
    },

    /// A synonym or stop word that is not one word once tokenised.
    #[error("`{word}` is not one word; a set holds single words (at {span})")]
    NotOneWord {
        /// The entry.
        word: String,
        /// The set.
        span: Span,
    },

    /// A field weight at or below zero, or above a thousand.
    #[error("a field's weight is above zero and at most 1000, to three places (at {span})")]
    WeightOutOfRange {
        /// The field.
        span: Span,
    },

    /// A vector distance this store does not have.
    ///
    /// The distance is declared rather than defaulted, because a default would
    /// silently decide which queries the index can serve — so a name nobody
    /// recognises is refused rather than replaced with a guess.
    #[error("there is no vector distance called {name:?}; try `cosine` or `euclidean` (at {span})")]
    NoSuchDistance {
        /// The name as written.
        name: String,
        /// Where it was written.
        span: Span,
    },

    /// An owner reached a user outside the tenancy they own.
    ///
    /// Distinct from [`Error::RoleForbids`], which says the caller's role is too
    /// small: here the role is exactly right and the *reach* is not, and the two
    /// send an operator to different fixes — one to a role change, the other to
    /// somebody further up.
    ///
    /// It names the user rather than answering "no such user", which would be
    /// the other way to keep the boundary. A store where `ALTER USER root …`
    /// says the name is unknown while `DEFINE USER root …` says it is taken is a
    /// store that lies to the person trying to fix something, and a name is not
    /// the secret here — the password hash and the grants are, and neither is in
    /// this message. The disclosure is also bounded: reaching this at all means
    /// the caller already owns a tenancy of their own.
    #[error("{user:?} is not in a tenancy you administer (at {span})")]
    NotYours {
        /// The user they named.
        user: String,
        /// Where the statement is.
        span: Span,
    },

    /// An edit that computes from the record, applied to a vault's record.
    ///
    /// `SET` and `MERGE` start from the record as it stands, and a vault's
    /// record as it stands is ciphertext plus the store's own key map. Building
    /// on it would mean opening the sealed fields the edit does not name — and
    /// opening a secret is `REVEAL`, which records itself before it answers. An
    /// `UPDATE` that opened three secrets to re-seal them would materialise
    /// plaintext with nothing to say it had been there, which is the one thing
    /// the audit ordering exists to prevent.
    ///
    /// So the message names the form that works rather than only refusing. The
    /// whole record is the unit of a vault write, and it is not a limitation
    /// dressed up: opening a field the edit never named is `REVEAL`, and `REVEAL`
    /// records itself before it answers — so an edit that quietly opened three
    /// secrets in order to re-seal them would put plaintext in this process with
    /// nothing anywhere saying it had been there.
    ///
    /// **Narrowed in W135 from "written whole" to this.** A field-by-field edit
    /// no longer needs the whole record, because the fields it names are the
    /// fields it supplies and every other envelope is carried through untouched.
    /// What is still refused is the part that was always the real problem: an
    /// assignment whose *expression* reads the record, which cannot be answered
    /// without opening a sealed value.
    #[error(
        "an edit of a vault's record may not compute from it — write the value, or replace the record with `UPDATE … = {{ … }}` (at {span})"
    )]
    VaultEditComputesFromTheRecord {
        /// Where the statement is.
        span: Span,
    },

    /// Declaring somebody who would reach further than the declarer.
    ///
    /// Separate from [`Error::NotYours`], which is about a user who already
    /// exists: this one is about a reach being *asked for*, and the two send an
    /// operator to different places — one to somebody further up, the other to
    /// the `ON` clause they left off.
    #[error("{user:?} would reach further than you do; name a tenancy inside your own (at {span})")]
    WiderThanYou {
        /// The name they tried to declare.
        user: String,
        /// Where the statement is.
        span: Span,
    },

    /// Granting an authority over a reach the subject is confined outside of.
    ///
    /// The refusal exists because the alternative is worse than either obvious
    /// answer. A declared tenancy is a second, independent confinement asked
    /// before the held set is consulted, so an authority granted outside it can
    /// never be used — which meant `GRANT read ON NAMESPACE staging TO nina`,
    /// where `nina` was declared `ON NAMESPACE prod`, **succeeded and did
    /// nothing**. A statement that returns `ok` and has no effect is the worst
    /// of the three available designs, because the operator's only evidence that
    /// they did the thing *is* the `ok`; the next person reads the holding in
    /// `INFO FOR USER` and believes it.
    ///
    /// It is the subject-side twin of [`Error::WiderThanYou`], which refuses the
    /// same escalation on the declaring side. Without it the escalation simply
    /// moves: declare somebody narrow, then grant them out of their own `ON`.
    ///
    /// `REVOKE` is deliberately **not** refused this way. Taking away a holding
    /// that could never be used is harmless, and the store may already hold such
    /// a holding from before this refusal existed — a revocation is how that is
    /// cleaned up, so refusing it would trap the very rows this rule is about.
    #[error(
        "{user:?} is confined to a tenancy that does not contain that reach, so the authority could never be used (at {span})"
    )]
    OutsideTheirTenancy {
        /// The subject of the grant.
        user: String,
        /// Where the statement is.
        span: Span,
    },

    /// Handing out an authority the caller does not hold at that reach.
    ///
    /// Two refusals wear this one message because they are the same rule read
    /// from both ends: you may not hand out an authority you were never given,
    /// and you may not hand out anything at all somewhere you do not govern.
    /// Together they are the property the whole model exists for — **no
    /// statement can mint an identity above the one running it**.
    ///
    /// Separate from [`Error::WiderThanYou`], which is about the `ON` clause of
    /// a declaration and sends an operator to a smaller tenancy. This one is
    /// about an authority, and sends them to whoever holds it.
    #[error("you do not hold {kind} at that reach yourself (at {span})")]
    CannotHandOut {
        /// The authority that is missing — the one being granted, or `govern`.
        kind: &'static str,
        /// Where the statement is.
        span: Span,
    },

    /// An authority named at a reach its kind cannot be held at.
    ///
    /// One kind is store-only — `replicate`, because the log it hands over
    /// carries the users, credentials and grants of every tenancy — so naming it
    /// over a namespace or a database asks for something that does not exist at
    /// that size.
    ///
    /// # Refused rather than accepted and quietly dropped
    ///
    /// The alternative was to store what can be held and discard the rest, which
    /// is what a **role** does here: `ROLE owner` at a namespace is a name for a
    /// set, and a set may narrow. An explicitly named kind is not a name for a
    /// set — it is a request, and the operator's only evidence that a request
    /// landed is the statement not complaining. Answering it with silence
    /// produces a grant that reads as present in the script and is absent from
    /// the store.
    ///
    /// Distinct from [`Self::CannotHandOut`], which is about the caller: that
    /// one says *you do not hold this*, and is fixed by somebody granting it.
    /// This one says *nobody holds this here*, and is fixed by asking at the
    /// store. Distinct from [`Self::NotTheWholeStore`], which refuses a
    /// subscription already authorized rather than the grant behind it.
    #[error("{kind} is held over the whole store or not at all (at {span})")]
    NotAtThatReach {
        /// The authority that was named.
        kind: &'static str,
        /// Where it was named.
        span: Span,
    },

    /// No user carries that id.
    ///
    /// Carries an id and no span because nothing typed it: it is reached when a
    /// stored declaration names a user who has since been deleted, and a caret
    /// under a character nobody wrote would point at the wrong thing. For a
    /// consumer this is not a fault — it is how deleting a declarer stops the
    /// background writer they declared.
    #[error("no user has id {id}")]
    UnknownUser {
        /// The id the stored declaration named.
        id: u32,
    },

    /// A role this language does not have.
    #[error("there is no role called {name:?} (at {span})")]
    NoSuchRole {
        /// The name as written.
        name: String,
        /// Where it was written.
        span: Span,
    },

    /// An authority kind this store does not have.
    ///
    /// Its own error rather than [`Error::NoSuchRole`], because the two sets do
    /// not overlap and being told there is no role called `manage` sends the
    /// reader looking for the wrong thing entirely.
    #[error("there is no authority called {name:?} — the kinds are {known} (at {span})")]
    NoSuchAuthority {
        /// The name as written.
        name: String,
        /// Every kind there is, so the answer is in the refusal.
        known: String,
        /// Where it was written.
        span: Span,
    },

    /// A password that is not one.
    ///
    /// An empty password is not a weak credential, it is the absence of one
    /// wearing the shape of a credential — and the account it belongs to is open
    /// to anybody who types the name. Refused where a password is *set* rather
    /// than where it is checked, because by the time it is checked the account
    /// already exists.
    #[error("a password cannot be empty (at {span})")]
    PasswordEmpty {
        /// Where it was written.
        span: Span,
    },

    /// A caller changing their own password who did not prove the current one.
    ///
    /// Distinct from [`Error::SignInRefused`] so a client can tell "your
    /// password is wrong" from "you are not signed in": here the caller *is*
    /// signed in, and what failed is the second proof this statement asks for.
    #[error("the current password does not match")]
    CurrentPasswordRefused,

    /// An owner of one tenancy attempting something whose subject is the store.
    ///
    /// Deliberately not [`Error::RoleForbids`]. The two have different fixes —
    /// *be made an owner* against *be made an owner of the store* — and an owner
    /// told they are not an owner goes looking for the wrong thing.
    #[error(
        "{user:?} holds one database, and this statement's subject is the whole store (at {span})"
    )]
    NotTheWholeStore {
        /// Who asked.
        user: String,
        /// Where they asked.
        span: Span,
    },
    /// Two message fields mapped onto one record field.
    ///
    /// Refused rather than resolved by order, because there is no order here
    /// that is not arbitrary: the mapping is a set of pairs, and whichever one
    /// happened to be applied last would win silently on every message.
    #[error(
        "{field:?} is mapped more than once, so which message field wins is undefined (at {span})"
    )]
    DuplicateMapping {
        /// The record field named twice.
        field: String,
        /// Where the second one was written.
        span: Span,
    },

    /// A token whose user has since been changed or removed.
    ///
    /// Deliberately one refusal for four different events — a rotated password,
    /// a corrected role, a moved tenancy, a dropped user. Which one it was is
    /// the holder's business only insofar as they must sign in again, and
    /// naming it would tell somebody holding a stolen token what happened to the
    /// account they stole it from.
    ///
    /// Carries no span, because no statement produced it.
    #[error("this session's token is no longer current; sign in again")]
    TicketStale,

    /// A verb this language does not have.
    #[error("there is no verb called {name:?}; a grant carries `read` or `write` (at {span})")]
    NoSuchVerb {
        /// The name as written.
        name: String,
        /// Where it was written.
        span: Span,
    },

    /// A transaction across leaders wrote into a table whose engine keeps
    /// something beside its records — a vault, a bucket, a space, a topic, a
    /// queue, a series or a vector or geo store — which a prepared write would
    /// bypass (ADR-0112). Only tables, collections and edges commit across
    /// leaders.
    #[error(
        "{table:?} cannot be written across leaders: only tables, collections and edges can (at {span})"
    )]
    AcrossKind {
        /// The table.
        table: String,
        /// Where the write was.
        span: Span,
    },

    /// A grant-governed user reached a table nobody granted them.
    ///
    /// Named separately from [`Error::RoleForbids`] because the two send the
    /// reader to different places: a role is changed by re-declaring the user,
    /// and a grant by running one more `GRANT`.
    #[error("{user:?} has not been granted {needs} on {table:?} (at {span})")]
    NotGranted {
        /// Who was asking.
        user: String,
        /// The table they named.
        table: String,
        /// What they needed on it.
        needs: &'static str,
        /// Where the statement is.
        span: Span,
    },

    /// A grant-governed user asking for a backup.
    ///
    /// A grant names a table, and a backup names none because it reaches every
    /// one — so no grant could permit it, and an emptiness that read as
    /// permission would be worse than a refusal that says why.
    #[error("{user} holds grants, and a backup is every table at once (at {span})")]
    GrantedUserCannotBackUp {
        /// The user who asked.
        user: String,
        /// Where the statement is.
        span: tessari_ql::Span,
    },

    /// A grant-governed user tried to declare structure.
    ///
    /// `DEFINE TABLE x` names a table that does not exist, so no grant for it
    /// can exist either — the statement is unreachable rather than refused by a
    /// rule, and saying so is better than a refusal that reads like a bug.
    #[error(
        "{user:?} is governed by grants, and a grant names a table that already exists — \
         declaring one is not a scoped activity (at {span})"
    )]
    GrantedUserCannotDeclare {
        /// Who was asking.
        user: String,
        /// Where the statement is.
        span: Span,
    },

    /// A field list on a grant that also carries `write`.
    ///
    /// A user who cannot see a field but may write the record would overwrite it
    /// whole and destroy what they cannot see — a data-loss hole created by the
    /// permission system rather than closed by it.
    #[error(
        "`FIELDS` narrows what may be read, and a `write` grant replaces whole records — \
         so the two together would let somebody destroy what they cannot see (at {span})"
    )]
    FieldsOnAWrite {
        /// Where the statement is.
        span: Span,
    },

    /// A revocation that would have removed a user's last grant.
    ///
    /// Which would **widen** them from a named table to every table their role
    /// allows — the opposite of what somebody running a `REVOKE` is thinking
    /// about. Widening is done by granting, which is a statement whose name says
    /// what it does.
    #[error(
        "that is {user:?}'s last grant, and taking it away would widen them to every table \
         their role allows; grant what they should reach instead (at {span})"
    )]
    LastGrant {
        /// Who the grant is for.
        user: String,
        /// Where the statement is.
        span: Span,
    },

    /// A `PASSHASH` this store would not have produced.
    #[error(
        "that hash is not one this store would store: it takes an Argon2id \
         hash, version 19, at parameters no weaker than its own (at {span})"
    )]
    PasshashRefused {
        /// Where the statement is.
        span: Span,
    },

    /// A password the hasher will not take.
    #[error("that password cannot be stored (at {span})")]
    PasswordUnusable {
        /// Where the declaration is.
        span: Span,
    },

    /// A statement reaching outside the tenancy its user belongs to.
    ///
    /// The refusal names the tenancy and not the record: one that says whether
    /// a record exists has answered the question it declined.
    #[error("{name} is outside this user's namespace and database (at {span})")]
    OutsideTenancy {
        /// The tenancy or object as written.
        name: String,
        /// Where it was written.
        span: Span,
    },

    /// A fold reached the evaluator instead of being replaced by its value.
    ///
    /// **Unreachable through the language today**, for the same reason
    /// [`Error::NoRecordInScope`] is: the parser refuses a fold everywhere
    /// except a projection, and a projection holding one is answered by the
    /// grouped path, which computes each fold once per group and substitutes it
    /// as a literal before anything is evaluated. Kept because the alternative
    /// arm would be a wildcard answering `none` — a wrong number where a fold
    /// was asked for, which is the failure this store spends most of its rules
    /// avoiding.
    #[error("a fold has no value outside a group (at {span})")]
    FoldOutsideAGroup {
        /// Where the fold was written.
        span: Span,
    },

    /// A route into a record, written where there is no record.
    ///
    /// **Unreachable through the language today**, and kept anyway. Two separate
    /// mechanisms hold the invariant — the parser only reads a bare name as a
    /// route inside a condition, and `seekable` refuses to use a right-hand side
    /// that reads the record as an index bound — and neither is expressed in a
    /// type.
    ///
    /// A vault edit evaluates its assignments against no record deliberately, to
    /// use the evaluator as its own detector for "this expression reads the
    /// record". That path catches this variant and answers
    /// [`Self::VaultEditComputesFromTheRecord`] instead, so the invariant above
    /// still holds for everything a caller can actually see. The alternative to this failure is answering `none`, which would be
    /// a wrong answer rather than a refusal, and a wrong answer from a filter is
    /// the failure mode this store spends most of its rules avoiding.
    #[error("there is no record here to read a path from (at {span})")]
    NoRecordInScope {
        /// Where the path was written.
        span: Span,
    },

    /// `search::ranks()` was called where no fused order ranked the record.
    ///
    /// A rank is the fusion's, not the record's, so outside the projection of a
    /// read ordered by `FUSE (…)` there is no answer — and `none` would read as
    /// "no branch placed it", which is a different claim.
    #[error(
        "search::ranks() answers only in the projection of a read ordered by FUSE (…) (at {span})"
    )]
    NotFused {
        /// Where the call was written.
        span: Span,
    },

    /// A statement that only a bucket answers, aimed at an ordinary table.
    ///
    /// Refused rather than writing a record that looks like a file: the two are
    /// the same shape on disk, so a table that gained file semantics because
    /// somebody used the wrong verb is a state nothing could later tell apart.
    #[error("{table} is not a bucket (at {span}) — define it with `DEFINE BUCKET`")]
    NotABucket {
        /// The table as written.
        table: String,
        /// Where it was written.
        span: Span,
    },

    /// A statement that only a queue answers, aimed at another kind of table.
    ///
    /// Refused rather than answered as an ordinary read, because `CLAIM` writes:
    /// a table that gained holds because somebody used the wrong verb would
    /// carry two fields nothing maintains and nothing would ever notice.
    #[error("{table} is not a queue (at {span}) — define it with `DEFINE QUEUE`")]
    NotAQueue {
        /// The table as written.
        table: String,
        /// Where it was written.
        span: Span,
    },

    /// A statement that only a topic answers, aimed at another kind of table
    /// (G037).
    #[error("{table} is not a topic (at {span}) — define it with `DEFINE TOPIC`")]
    NotATopic {
        /// The table as written.
        table: String,
        /// Where it was written.
        span: Span,
    },

    /// A group of that name already reads the topic (G042).
    #[error(
        "{topic} already has a group named '{group}' (at {span}) — `IF NOT EXISTS` accepts \
         it as it stands, and `DROP GROUP` removes it"
    )]
    GroupExists {
        /// The group.
        group: String,
        /// The topic as written.
        topic: String,
        /// Where it was written.
        span: Span,
    },

    /// No group of that name reads the topic (G042).
    #[error("{topic} has no group named '{group}' (at {span}) — declare one with `DEFINE GROUP`")]
    NoSuchGroup {
        /// The group.
        group: String,
        /// The topic as written.
        topic: String,
        /// Where it was written.
        span: Span,
    },

    /// `ACK` or `NACK` for a reader that is not a group, so nothing is held to
    /// settle (G042).
    ///
    /// Refused rather than answered with a zero: a reader acknowledging by hand
    /// under a name that commits its reads would be told its acknowledgements
    /// work while they do nothing at all.
    #[error(
        "'{consumer}' reads {topic} without a group, so nothing is held to acknowledge \
         (at {span}) — its reads are acknowledged by committing them; `DEFINE GROUP` makes \
         acknowledgement explicit"
    )]
    NotAGroup {
        /// The reader.
        consumer: String,
        /// The topic as written.
        topic: String,
        /// Where it was written.
        span: Span,
    },

    /// `AFTER` on a read by a group member (G042).
    #[error(
        "'{group}' is a group, and a member cannot skip the group's work with `AFTER` \
         (at {span}) — `ALTER GROUP … START AT n` moves the whole group"
    )]
    AfterOnGroup {
        /// The group.
        group: String,
        /// Where it was written.
        span: Span,
    },

    /// A group whose dead letter is the topic it reads (G042).
    #[error(
        "{topic} cannot be its own group's dead letter (at {span}) — a message given up \
         on would be handed out again"
    )]
    DeadLetterIsTheTopic {
        /// The topic as written.
        topic: String,
        /// Where it was written.
        span: Span,
    },

    /// A topic consumer whose topic and destination are in two databases
    /// (ADR-0087).
    ///
    /// Refused because the guarantee is the transaction: the group read, the
    /// writes and the acknowledgement commit together, and a transaction is one
    /// database's.
    #[error(
        "{topic} and {destination} are in different databases (at {span}) — a topic consumer \
         reads and writes in one transaction, and a transaction is one database's"
    )]
    TopicConsumerSpansDatabases {
        /// The topic as written.
        topic: String,
        /// The destination as written.
        destination: String,
        /// Where it was written.
        span: Span,
    },

    /// `ON FAILURE quarantine` over a group that cannot park a message
    /// (ADR-0087).
    ///
    /// Quarantine hands a message back to the group, and only a group with a
    /// delivery limit and a dead letter ever stops handing it out again.
    #[error(
        "'{group}' has no {missing}, so a message handed back would be handed out forever \
         (at {span}) — declare the group with `DELIVERIES n DEAD LETTER TO <topic>`, or use \
         `ON FAILURE stop`"
    )]
    QuarantineNeedsDeadLetter {
        /// The group.
        group: String,
        /// Which clause it lacks.
        missing: &'static str,
        /// Where it was written.
        span: Span,
    },

    /// A consumer named with the other kind's statement (ADR-0087).
    #[error("`{name}` is a {kind} consumer (at {span}) — use `{instead}`")]
    WrongConsumerKind {
        /// The consumer.
        name: String,
        /// What it is: `kafka` or `topic`.
        kind: &'static str,
        /// The statement that names it.
        instead: String,
        /// Where it was written.
        span: Span,
    },

    /// A position or a count on `READ FROM` that is not a whole number at or
    /// above zero (G037).
    #[error("{reason} (at {span})")]
    InvalidPosition {
        /// Why the value cannot be taken.
        reason: &'static str,
        /// Where it is.
        span: Span,
    },

    /// A caller's write setting a field only the queue engine may set.
    ///
    /// The bucket's rule in a second place and for the identical reason: engine
    /// metadata a caller can write is metadata that can lie, and a hold whose
    /// deadline the holder chose is not a hold. Named rather than silently
    /// dropped, so a payload that happens to use the name is told what happened.
    #[error(
        "`{field}` on a queue is written by the store (at {span}) — `CLAIM` and `RELEASE` set it"
    )]
    QueueFieldIsTheEngines {
        /// The field the write named.
        field: &'static str,
        /// Where the write was written.
        span: Span,
    },

    /// A release of a hold that belongs to somebody else.
    ///
    /// Refused rather than performed, because taking another claimant's work
    /// away is a different act from letting go of your own — and until there
    /// was a claimant to compare, any caller who could write the table could do
    /// it with nothing anywhere saying so.
    ///
    /// It names the **consumer** and not the instance: the consumer is the name
    /// a person chose and can recognise, while the instance is a value the
    /// engine minted and means nothing to anybody reading the message.
    #[error("{consumer} is holding that record (at {span})")]
    HeldByAnother {
        /// The consumer whose hold it is.
        consumer: String,
        /// Where the release was written.
        span: Span,
    },

    /// `RELEASE ALL` from a session that never said who it is.
    ///
    /// The bare form means *everything mine*, and a session with no declared
    /// consumer has no instance for *mine* to point at. The two silent readings
    /// are both wrong in ways that look like success — succeeding on nothing
    /// tells a worker its work was freed when it was not, and freeing every
    /// unsigned hold takes work from claimants who never asked this session for
    /// anything — so it is refused, naming the statement that would fix it.
    #[error("`RELEASE ALL` needs `USE CONSUMER` first, or a named consumer (at {span})")]
    NoConsumerDeclared {
        /// Where the release was written.
        span: Span,
    },

    /// A claim for more records than one statement may take.
    ///
    /// A bound rather than a tuning knob: without one, a single statement holds
    /// the whole queue for the whole timeout and every other worker waits, with
    /// nothing anywhere in an error state.
    #[error("a claim takes at most {ceiling} records, not {asked} (at {span})")]
    ClaimAboveCeiling {
        /// How many were asked for.
        asked: u64,
        /// How many one statement may take.
        ceiling: u64,
        /// Where the claim was written.
        span: Span,
    },

    /// A claim whose deadline falls outside the range an instant can hold.
    ///
    /// Only reachable from a timeout so long that adding it to now overflows,
    /// which the declaration allows because refusing a long timeout would need a
    /// ceiling nobody has a reason for. Refused here rather than saturated: a
    /// deadline clamped to the end of time is a hold that never lapses, which is
    /// the one thing this engine exists to prevent.
    #[error("this queue\'s timeout puts the claim past the end of time (at {span})")]
    ClaimDeadlineUnreachable {
        /// Where the claim was written.
        span: Span,
    },

    /// A record written by hand into a bucket.
    ///
    /// A bucket's records describe bytes the store holds. One a caller can write
    /// is one that can lie — a size that disagrees with the file, a chunk count
    /// pointing at chunks nobody wrote — and nothing would ever catch it,
    /// because there is nothing to catch it against.
    #[error("{table} is a bucket (at {span}) — write a file with `PUT`")]
    NotWrittenByHand {
        /// The bucket as written.
        table: String,
        /// Where it was written.
        span: Span,
    },

    /// A `PUT` whose value is neither bytes nor text.
    #[error("a file is bytes, not {found} (at {span})")]
    FileIsNotBytes {
        /// The type that stood there instead.
        found: &'static str,
        /// Where the statement is.
        span: Span,
    },

    /// A file addressed by something other than a path.
    ///
    /// A file's identity is text, because a chunk's identity is the path
    /// followed by its ordinal — and an integer identity and the text of that
    /// integer would produce the same chunk key, which is two files sharing
    /// bytes.
    #[error("a file is named by a path, so its identity is text (at {span})")]
    FileNeedsAPath {
        /// Where the identity was written.
        span: Span,
    },

    /// Metadata promising a chunk the store does not hold.
    ///
    /// Unreachable through the statements that write files — the metadata and
    /// the chunks land in one commit — so this says the store is inconsistent
    /// rather than answering a file that is quietly short.
    #[error("{path:?} is missing chunk {ordinal} (at {span})")]
    FileIsIncomplete {
        /// The file's path.
        path: String,
        /// Which chunk is absent.
        ordinal: u32,
        /// Where the statement is.
        span: Span,
    },

    /// A parameter in an expression that belongs to no call.
    ///
    /// Every parameter in a script is replaced by its value before the first
    /// statement runs, so this is not reachable from a script. What is reachable
    /// is a **stored** expression — a field's `DEFAULT` — which is evaluated on
    /// every write that omits the field and therefore belongs to no particular
    /// caller. There is nobody to bind it, so it is refused where it is declared
    /// rather than surprising a write months later.
    #[error(
        "the parameter `${name}` at {span} has no value here — a stored expression belongs to no call"
    )]
    ParameterHasNoValue {
        /// The parameter's name, without its marker.
        name: String,
        /// Where it was written.
        span: Span,
    },
}

/// Which catalog dependency a [`Error::StillDepended`] refusal is about.
///
/// An enum rather than the three `&'static str` fields this began as, for two
/// reasons and only one of them is size. The other is that the strings had to
/// agree — `analyzer` with `is named by` with `fields` — and nothing made them:
/// the first draft shipped *"still is named by 1 fields"*, which is what a free
/// pairing of a verb and a plural noun produces the first time somebody writes
/// the third one. Here the three are one value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depended {
    /// An analyzer a field still names. The reference is by name, so nothing in
    /// the catalog enforces it and a dangling one is a search that quietly stops
    /// matching.
    AnalyzerByField,
    /// A database that still holds tables.
    DatabaseByTable,
    /// A namespace that still holds databases.
    NamespaceByDatabase,
    /// A graph that tables still belong to.
    GraphByTable,
    /// A graph that edge kinds still belong to.
    GraphByEdgeKind,
    /// An analyzer a search still reads with (ADR-0105).
    AnalyzerBySearch,
    /// A synonym set a search's field still names.
    SynonymsBySearch,
    /// A stop-word set a search still names.
    StopwordsBySearch,
}

impl Depended {
    /// What was asked to go.
    pub(crate) const fn entity(self) -> &'static str {
        match self {
            Self::AnalyzerByField | Self::AnalyzerBySearch => "analyzer",
            Self::SynonymsBySearch => "synonym set",
            Self::StopwordsBySearch => "stop-word set",
            Self::DatabaseByTable => "database",
            Self::NamespaceByDatabase => "namespace",
            Self::GraphByTable | Self::GraphByEdgeKind => "graph",
        }
    }

    /// How the dependants stand to it, as the sentence needs it.
    pub(crate) const fn relation(self) -> &'static str {
        match self {
            Self::AnalyzerByField
            | Self::AnalyzerBySearch
            | Self::SynonymsBySearch
            | Self::StopwordsBySearch => "is named by",
            Self::DatabaseByTable | Self::NamespaceByDatabase => "holds",
            // Not "holds": a graph does not contain its tables the way a
            // database contains them — they belong to it while living in the
            // database, and the sentence has to say which relation is in the way.
            Self::GraphByTable | Self::GraphByEdgeKind => "is joined by",
        }
    }

    /// What they are, agreeing with how many there are.
    pub(crate) const fn dependants(self, count: usize) -> &'static str {
        match (self, count) {
            (Self::AnalyzerByField, 1) => "field",
            (Self::AnalyzerByField, _) => "fields",
            (Self::DatabaseByTable, 1) => "table",
            (Self::DatabaseByTable, _) => "tables",
            (Self::NamespaceByDatabase, 1) => "database",
            (Self::NamespaceByDatabase, _) => "databases",
            (Self::GraphByTable, 1) => "table",
            (Self::GraphByTable, _) => "tables",
            (Self::GraphByEdgeKind, 1) => "edge kind",
            (Self::GraphByEdgeKind, _) => "edge kinds",
            (Self::AnalyzerBySearch | Self::SynonymsBySearch | Self::StopwordsBySearch, 1) => {
                "search"
            }
            (Self::AnalyzerBySearch | Self::SynonymsBySearch | Self::StopwordsBySearch, _) => {
                "searches"
            }
        }
    }
}

#[cfg(test)]
mod article_tests {
    use tessari_ql::Span;

    use super::Error;

    /// Every word that reaches the role refusal reads as English. Three of the
    /// four begin with a vowel — `owner`, `editor` and the `authorities`
    /// fallback for a user whose set no role summarises — and only `viewer` got
    /// the article right by luck, which is why *"a editor may not operate"* was
    /// reported from a documentation wave rather than from the engine's own
    /// tests. Q-366.
    #[test]
    fn the_role_refusal_reads_as_english_for_every_role_it_can_name() {
        for (role, wanted) in [
            ("owner", "an owner may not"),
            ("editor", "an editor may not"),
            ("authorities", "an authorities may not"),
            ("viewer", "a viewer may not"),
        ] {
            let said = Error::RoleForbids {
                role,
                needs: "operate",
                span: Span::new(0, 6),
            }
            .to_string();
            assert!(said.starts_with(wanted), "{said}");
        }
    }
}
