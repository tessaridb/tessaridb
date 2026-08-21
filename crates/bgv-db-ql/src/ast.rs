//! What a script says, once the words are gone.
//!
//! Two properties of this tree are deliberate.
//!
//! **Every node carries a span.** Not for error messages alone — execution
//! raises failures too, and "no index on `email`" is a different message when it
//! can point at the `email` the author wrote.
//!
//! **Nothing here is resolved.** A table is a name, not a [`TableId`]; a name
//! becomes an id only inside a transaction, by reading the catalog, because that
//! is the only place the answer is true. A tree that carried ids would have to
//! be re-parsed whenever a definition changed under it.
//!
//! The enums here are deliberately **not** `#[non_exhaustive]`. They are the
//! contract between the parser and whatever executes the tree, both of which
//! version together in this workspace, and exhaustive matching is what makes
//! adding a statement to the grammar fail to compile until something runs it.
//! Sealing them would buy version tolerance nobody needs and pay for it with a
//! wildcard arm that silently accepts every future statement.
//!
//! [`TableId`]: bgv_db_types::TableId

use bgv_db_types::{FieldKind, Filter, Path, RecordId, Value};

use crate::function::Function;
use crate::token::Span;

/// A parsed script: statements in the order they were written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    /// The statements, in source order.
    pub statements: Vec<Statement>,
    /// The whole source.
    pub span: Span,
}

/// One statement, and where it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    /// What the statement does.
    pub kind: StatementKind,
    /// Where it sits in the source.
    pub span: Span,
}

/// The statement forms this milestone accepts.
///
/// Flat rather than grouped by family: execution matches on it once, and a
/// grouping would only move the match one level down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatementKind {
    /// `USE NAMESPACE prod DATABASE orders` — at least one of the two.
    Use {
        /// The namespace to work in, when the statement names one.
        namespace: Option<Name>,
        /// The database to work in, when the statement names one.
        database: Option<Name>,
    },
    /// `DEFINE NAMESPACE prod`
    DefineNamespace {
        /// The name to create.
        name: Name,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE DATABASE orders`
    DefineDatabase {
        /// The name to create.
        name: Name,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE TABLE users SCHEMAFULL` / `DEFINE TABLE follows EDGE`
    DefineTable {
        /// The name to create.
        name: Name,
        /// Whether the table refuses a field it does not declare.
        schemafull: bool,
        /// Whether the table holds edges, with an index on each endpoint.
        edge: bool,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE SPACE sessions` — a table whose records hold a single value.
    DefineSpace {
        /// The name to create.
        name: Name,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE INDEX by_email ON users FIELDS email UNIQUE`
    DefineIndex {
        /// The index's name, unique within its table.
        name: Name,
        /// The table it indexes.
        table: TableRef,
        /// The values it projects, in order. A path indexes a nested value.
        fields: Vec<FieldPath>,
        /// Whether two records may share one entry.
        unique: bool,
        /// Whether the index holds terms rather than whole values.
        search: bool,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE FIELD email ON users TYPE string`
    DefineField {
        /// The field's name, unique within its table.
        name: Name,
        /// The table it is declared on.
        table: TableRef,
        /// What the field is allowed to hold.
        kind: FieldKind,
        /// Whether the field must hold a value: present, and not `null`.
        required: bool,
        /// The analyzer this field's text becomes terms by, when it has one.
        analyzer: Option<Name>,
        /// What a write supplying no value uses instead.
        ///
        /// A **value-position** expression, so it cannot read the record it is
        /// filling in — which would be a rule about evaluation order nobody
        /// would guess.
        default: Option<Written>,
        /// Whether re-declaring an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE ANALYZER simple FILTERS lowercase, ascii`
    DefineAnalyzer {
        /// The name to create.
        name: Name,
        /// The filters it applies, in order.
        filters: Vec<Filter>,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DROP FIELD email ON users` — removes the declaration, not the data.
    DropField {
        /// The field's name.
        name: Name,
        /// The table it was declared on.
        table: TableRef,
    },
    /// `RELATE users:1->follows->users:2` / `… = { since: … }`
    Relate {
        /// The edge's source.
        from: RecordTarget,
        /// The edge table the relation is recorded in.
        edges: TableRef,
        /// The edge's target.
        to: RecordTarget,
        /// The edge's own properties, when the statement gives any.
        value: Option<Expr>,
    },
    /// `DROP TABLE users` — removes the definition, not the records.
    DropTable {
        /// The table to undefine.
        table: TableRef,
    },
    /// `DROP INDEX by_email ON users`
    DropIndex {
        /// The index's name.
        name: Name,
        /// The table it indexes.
        table: TableRef,
    },
    /// `CREATE users:1 = { … }`
    Create {
        /// The record to write.
        target: RecordTarget,
        /// Its whole content.
        value: Expr,
    },
    /// `SELECT * FROM …`
    Select(Select),
    /// `UPDATE users:1 = { … }` — the value is replaced, never merged.
    Update {
        /// The record to write.
        target: RecordTarget,
        /// Its whole new content.
        value: Expr,
    },
    /// `DELETE users:1`
    Delete {
        /// The record to remove.
        target: RecordTarget,
    },
    /// `GET sessions:'abc'` as a statement of its own.
    Get {
        /// The key to read.
        target: RecordTarget,
    },
    /// `SET sessions:'abc' = …`
    Set {
        /// The key to write.
        target: RecordTarget,
        /// The whole value.
        value: Expr,
    },
    /// `DEL sessions:'abc'`
    Del {
        /// The key to remove.
        target: RecordTarget,
    },
    /// `KEYS FROM sessions RANGE 'a'..'m'`
    Keys {
        /// The space to list.
        space: TableRef,
        /// The range of keys, when the statement bounds it.
        range: Option<RangeExpr>,
    },
    /// `BEGIN`
    Begin,
    /// `COMMIT`
    Commit,
    /// `CANCEL`
    Cancel,
}

/// A read of records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Select {
    /// Which values each record answers with.
    pub projection: Projection,
    /// Which access path the statement resolves to.
    pub from: Source,
    /// The keys the records are grouped by, when the read groups.
    ///
    /// Empty means no grouping — which is not the same as no aggregate:
    /// `SELECT count(*) FROM users` folds every record into one group without
    /// naming a key, because the commonest question the language can be asked
    /// should not need a clause that means nothing.
    pub group: Vec<FieldPath>,
    /// The keys the answer is sorted by, in order of significance.
    pub order: Vec<Ordering>,
    /// How many records to pass over before answering.
    pub start: Option<u64>,
    /// How many to answer with at most.
    pub limit: Option<u64>,
    /// Where the statement sits in the source.
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
    /// `*` — the record as it is stored.
    All,
    /// A named list, in the order it was written.
    ///
    /// Order is carried even though the answer is a name-ordered object, because
    /// an error naming the second of two colliding projections should point at
    /// the one the author wrote second.
    Values(Vec<Projected>),
}

/// One projected value, and the name it answers under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projected {
    /// How the value is produced.
    pub value: Projectable,
    /// The name it answers under.
    ///
    /// Resolved at parse rather than left for the executor: whether two
    /// projections collide is a property of the statement, so it is knowable
    /// before anything runs and is refused there.
    pub name: Name,
}

/// The three access paths the store has, named by what the statement targets.
///
/// Which one runs is decided here, by the shape of the statement, and not by a
/// cost model — there is no planner at this milestone and nothing pretends
/// otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// One record, by its identity.
    Record(RecordTarget),
    /// Every record of a table.
    Table(TableRef),
    /// The far side of one hop along an edge table.
    ///
    /// `users:1->follows` reads the edge records themselves;
    /// `users:1->follows->users` resolves one step further and reads the records
    /// the edges point at. Both are index reads, because an edge table carries
    /// an index on each endpoint from the moment it is declared.
    Traverse {
        /// Where the walk starts.
        from: RecordTarget,
        /// Which way the arrows point.
        direction: Direction,
        /// The edge table being walked.
        edges: TableRef,
        /// The table the far endpoint is read from, when the statement names one.
        target: Option<TableRef>,
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
}

/// An operator producing a number from two numbers.
///
/// Separate from [`BinaryOp`] for the same reason `AND` and `OR` are: these
/// answer with a number and can fail on their operands, while a comparison
/// answers with a boolean and is total in both arguments. One enum would leave
/// each implementation with arms it can never reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithmeticOp {
    /// `+` — addition. Numbers only; concatenation is `string::concat`.
    Add,
    /// `-` — subtraction.
    Subtract,
    /// `*` — multiplication.
    Multiply,
    /// `/` — division, which always produces at least a decimal.
    ///
    /// `7 / 2` is `3.5` and not `3`: truncating integer division is the classic
    /// silent wrong answer, where the query looks right and the number is not.
    Divide,
    /// `%` — remainder.
    Remainder,
}

impl ArithmeticOp {
    /// How the operator is written.
    #[must_use]
    pub const fn spelling(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Subtract => "-",
            Self::Multiply => "*",
            Self::Divide => "/",
            Self::Remainder => "%",
        }
    }
}

/// An operator taking two values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// `=` — the two values are the same value.
    Equal,
    /// `!=` — they are not.
    NotEqual,
    /// `<` — below, in the value system's declared order across types.
    Less,
    /// `<=` — below or the same.
    LessOrEqual,
    /// `>` — above.
    Greater,
    /// `>=` — above or the same.
    GreaterOrEqual,
    /// `IN` — the collection on the **right** holds the value on the left.
    ///
    /// The mirror of [`BinaryOp::Contains`], and both exist because both read
    /// naturally in different sentences: `'urgent' IN tags` and
    /// `tags CONTAINS 'urgent'` ask the same question from either end.
    In,
    /// `CONTAINS` — the collection on the **left** holds the value on the right.
    ///
    /// Membership, not substring — a different question from [`BinaryOp::Like`],
    /// which is why both exist. `tags CONTAINS 'urgent'` asks whether an array
    /// or a set holds that element; `body LIKE '%urgent%'` asks whether text
    /// contains those characters.
    Contains,
    /// `LIKE` — the text matches a pattern, as SQL's `LIKE` does.
    ///
    /// The pattern covers the **whole** value — which is why a substring search
    /// is written `'%text%'` — with `%` standing for any run of characters and
    /// `_` for exactly one.
    Like,
    /// The same, ignoring case.
    Ilike,
    /// `MATCHES` — the analyzed text holds every term of the query.
    ///
    /// A third question, not a special case of the other two: `LIKE` is a
    /// pattern over the whole value and `CONTAINS` is membership in a
    /// collection, and neither can ask whether text holds a *word*.
    Matches,
}

impl BinaryOp {
    /// How the operator is written.
    #[must_use]
    pub const fn spelling(self) -> &'static str {
        match self {
            Self::Equal => "=",
            Self::NotEqual => "!=",
            Self::Less => "<",
            Self::LessOrEqual => "<=",
            Self::Greater => ">",
            Self::GreaterOrEqual => ">=",
            Self::In => "IN",
            Self::Contains => "CONTAINS",
            Self::Like => "LIKE",
            Self::Ilike => "ILIKE",
            Self::Matches => "MATCHES",
        }
    }
}

/// A value written in the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expr {
    /// What the expression is.
    pub kind: ExprKind,
    /// Where it sits in the source.
    pub span: Span,
}

/// The expression forms this milestone accepts.
///
/// There is no arithmetic and no call yet: an expression is a value, a container
/// of expressions, a read, or a test built out of those. The reads are what make
/// a key-value value usable inside a record statement without either model
/// knowing about the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExprKind {
    /// A literal that is already a whole value.
    Literal(Value),
    /// A value read out of the record being tested: `name`, `address.city`.
    ///
    /// Only meaningful where there **is** a record — inside a condition. In a
    /// value position a bare name is a table, which is why the two positions
    /// read the same token differently and why the parser knows which it is in.
    Path(FieldPath),
    /// `NOT <expr>` — the operand must be a boolean.
    Not(Box<Expr>),
    /// `-<expr>` — the operand must be a number.
    Negate(Box<Expr>),
    /// `group::name(a, b)` — a call of one of the language's own functions.
    ///
    /// Arity is checked when the statement is read, because the set of
    /// functions is known then; argument types are checked when it runs,
    /// because until a record is in hand there is nothing to check.
    Call {
        /// Which function.
        function: Function,
        /// Its arguments, in order.
        arguments: Vec<Expr>,
        /// Where the name was written, for a failure to point at.
        span: Span,
    },
    /// `<expr> AND <expr>` — both must be booleans, and the right is evaluated
    /// only when the left holds.
    ///
    /// Separate from [`ExprKind::Binary`] rather than an operator inside it,
    /// because composing conditions and comparing values are genuinely
    /// different: one short-circuits and demands booleans, the other takes any
    /// two values and always looks at both. Folding them together would leave
    /// every value-level operator with two arms it can never reach.
    And(Box<Expr>, Box<Expr>),
    /// `<expr> OR <expr>` — the right is evaluated only when the left does not
    /// hold.
    Or(Box<Expr>, Box<Expr>),
    /// Two numbers and an operator.
    Arithmetic {
        /// Which operator.
        op: ArithmeticOp,
        /// The left operand.
        left: Box<Expr>,
        /// The right operand.
        right: Box<Expr>,
    },
    /// Two values and an operator.
    Binary {
        /// Which operator.
        op: BinaryOp,
        /// The left operand.
        left: Box<Expr>,
        /// The right operand.
        right: Box<Expr>,
    },
    /// A table named in a value position.
    Table(TableRef),
    /// A record named in a value position: `users:1`.
    Record(RecordTarget),
    /// `[a, b]`
    Array(Vec<Expr>),
    /// `set [a, b]`
    Set(Vec<Expr>),
    /// `{ name: 'ada' }`
    Object(Vec<Field>),
    /// `1..10`, `1..=10`
    Range(RangeExpr),
    /// `GET sessions:'abc'` in a value position.
    Get(RecordTarget),
    /// `(SELECT * FROM users:1)` in a value position.
    Select(Box<Select>),
}

/// One field of an object literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// The field's name.
    pub name: Name,
    /// Its value.
    pub value: Expr,
}

/// A span between two values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeExpr {
    /// The lower bound, always included.
    pub start: Box<Expr>,
    /// The upper bound.
    pub end: Box<Expr>,
    /// Whether the upper bound is included: `..=` rather than `..`.
    pub inclusive: bool,
}

/// What a projection produces: one value per record, or one per group.
///
/// The two are different **arities**, not two operators, which is why an
/// aggregate could not ride along with the functions: everything else in the
/// read language answers one row per record, and this does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Projectable {
    /// An expression, evaluated against each record.
    Value(Expr),
    /// A fold over the records of a group.
    Aggregate {
        /// Which fold.
        fold: Aggregate,
        /// What it folds over — absent for `count(*)`, which folds over the
        /// records themselves rather than over a value in them.
        over: Option<Box<Expr>>,
        /// Where it was written.
        span: Span,
    },
}

/// A fold over the records of a group.
///
/// Spelled without a namespace, where every function has one. That is the
/// namespacing rule earning its keep rather than being broken by it:
/// `array::len` counts one record's array and `count` counts records, and a
/// reader can tell which arity they are looking at from the spelling alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregate {
    /// `count(*)` counts records; `count(<expr>)` counts the records where the
    /// expression is present and not null.
    Count,
    /// `sum(<expr>)` — over nothing, zero.
    Sum,
    /// `mean(<expr>)` — over nothing, `NONE`.
    Mean,
    /// `min(<expr>)`, in the value system's order.
    Min,
    /// `max(<expr>)`, in the same order.
    Max,
}

impl Aggregate {
    /// Every fold, so a listing cannot drift from the set.
    pub const ALL: &'static [Self] = &[Self::Count, Self::Sum, Self::Mean, Self::Min, Self::Max];

    /// How the fold is written.
    #[must_use]
    pub const fn spelling(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Sum => "sum",
            Self::Mean => "mean",
            Self::Min => "min",
            Self::Max => "max",
        }
    }

    /// The fold a word spells, if it spells one.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|fold| fold.spelling().eq_ignore_ascii_case(word))
    }
}

/// An expression kept as the text it was written as.
///
/// The parser validates it by parsing it and then keeps the **source slice**,
/// because that is what the catalog stores: a definition should stay legible in
/// a dump, and a printer that rebuilt the text from the tree would be a second
/// grammar to keep in step with the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// The text, exactly as the author wrote it.
    pub text: String,
    /// Where it sits in the source.
    pub span: Span,
}

/// A route to a value inside a record, as written.
///
/// Separate from [`Name`] rather than replacing it, because the two are asked
/// for in different positions and only one of them may be nested: a table is
/// named, an index is named, a field *declaration* names a top-level field, and
/// only a filter and an index's projection address a value that may sit deeper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldPath {
    /// The route itself.
    pub path: Path,
    /// Where it sits in the source.
    pub span: Span,
}

/// A name as written, with where it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Name {
    /// The name itself. Case-sensitive, unlike a keyword.
    pub text: String,
    /// Where it sits in the source.
    pub span: Span,
}

/// Which endpoint of an edge a traversal starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// `->` — the walk starts at the edge's `out` and arrives at its `in`.
    Outgoing,
    /// `<-` — the walk starts at the edge's `in` and arrives at its `out`.
    Incoming,
}

impl Direction {
    /// The endpoint field a walk in this direction matches on.
    #[must_use]
    pub const fn from_field(self) -> &'static str {
        match self {
            Self::Outgoing => "out",
            Self::Incoming => "in",
        }
    }

    /// The endpoint field a walk in this direction arrives at.
    #[must_use]
    pub const fn to_field(self) -> &'static str {
        match self {
            Self::Outgoing => "in",
            Self::Incoming => "out",
        }
    }
}

/// A table, optionally qualified by its database.
///
/// An explicit qualification always wins over the session's context, so that a
/// script that says which database it means cannot be run against another one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    /// The database, when the name is qualified: `orders.users`.
    pub database: Option<Name>,
    /// The table's own name.
    pub name: Name,
    /// Where the whole reference sits.
    pub span: Span,
}

/// One record, by table and identity: `users:1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordTarget {
    /// The table the record lives in.
    pub table: TableRef,
    /// Its identity within that table.
    pub id: RecordId,
    /// Where the whole reference sits.
    pub span: Span,
}
