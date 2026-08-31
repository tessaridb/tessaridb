//! What can go wrong between a script and the store.
//!
//! Every variant that can name a place does. A script is written by hand, and a
//! failure that cannot point at the words that caused it makes its author read
//! the whole thing again.

use tessari_ql::{Function, Span};

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

    /// `UPDATE` over a record that is not there.
    #[error("no record {id} (at {span}) — say `CREATE` to write a new one")]
    NoSuchRecord {
        /// The identity as written.
        id: String,
        /// Where it was written.
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

    /// The signed-in user's role does not allow the statement.
    #[error("a {role} may not {needs} (at {span})")]
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
    /// type. The alternative to this failure is answering `none`, which would be
    /// a wrong answer rather than a refusal, and a wrong answer from a filter is
    /// the failure mode this store spends most of its rules avoiding.
    #[error("there is no record here to read a path from (at {span})")]
    NoRecordInScope {
        /// Where the path was written.
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
}

impl Depended {
    /// What was asked to go.
    pub(crate) const fn entity(self) -> &'static str {
        match self {
            Self::AnalyzerByField => "analyzer",
            Self::DatabaseByTable => "database",
            Self::NamespaceByDatabase => "namespace",
        }
    }

    /// How the dependants stand to it, as the sentence needs it.
    pub(crate) const fn relation(self) -> &'static str {
        match self {
            Self::AnalyzerByField => "is named by",
            Self::DatabaseByTable | Self::NamespaceByDatabase => "holds",
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
        }
    }
}
