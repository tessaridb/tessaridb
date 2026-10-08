//! Expressions: what a condition, a projection or a value is built from.

mod aggregate;

use super::{BinaryOp, FieldPath, Name, RecordTarget, Select, TableRef};
use crate::function::Function;
use crate::token::Span;
pub use aggregate::{Aggregate, Retention};
use tessari_types::{Duration, RecordId, Value};

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

/// A value written in the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expr {
    /// What the expression is.
    pub kind: ExprKind,
    /// Where it sits in the source.
    pub span: Span,
}

impl Expr {
    /// Whether two expressions say the same thing, wherever they were written.
    ///
    /// `==` compares spans, so two textually identical expressions in one
    /// statement are unequal — which is right for an AST and wrong for the one
    /// question a caller keeps needing to ask: *is this projection projecting
    /// that group key?* `GROUP BY time::bucket(at, 1h)` and the projection that
    /// names it are the same expression written twice, at two places.
    ///
    /// The walk compares kinds and recurses; a span never takes part.
    #[must_use]
    pub fn same_shape(&self, other: &Self) -> bool {
        match (&self.kind, &other.kind) {
            (ExprKind::Not(left), ExprKind::Not(right))
            | (ExprKind::Negate(left), ExprKind::Negate(right)) => left.same_shape(right),
            (ExprKind::And(a, b), ExprKind::And(c, d))
            | (ExprKind::Or(a, b), ExprKind::Or(c, d)) => a.same_shape(c) && b.same_shape(d),
            (
                ExprKind::Binary { op, left, right },
                ExprKind::Binary {
                    op: other_op,
                    left: other_left,
                    right: other_right,
                },
            ) => op == other_op && left.same_shape(other_left) && right.same_shape(other_right),
            (
                ExprKind::Arithmetic { op, left, right },
                ExprKind::Arithmetic {
                    op: other_op,
                    left: other_left,
                    right: other_right,
                },
            ) => op == other_op && left.same_shape(other_left) && right.same_shape(other_right),
            (
                ExprKind::Call {
                    function,
                    arguments,
                    ..
                },
                ExprKind::Call {
                    function: other_function,
                    arguments: other_arguments,
                    ..
                },
            ) => {
                function == other_function
                    && arguments.len() == other_arguments.len()
                    && arguments
                        .iter()
                        .zip(other_arguments.iter())
                        .all(|(left, right)| left.same_shape(right))
            }
            (ExprKind::Array(left), ExprKind::Array(right))
            | (ExprKind::Set(left), ExprKind::Set(right)) => {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right.iter())
                        .all(|(one, two)| one.same_shape(two))
            }
            (ExprKind::Path(left), ExprKind::Path(right)) => left.path == right.path,
            (ExprKind::Literal(left), ExprKind::Literal(right)) => left == right,
            (ExprKind::Table(left), ExprKind::Table(right)) => left.name.text == right.name.text,
            // Everything else — a record target, a nested read, an object — is
            // compared as written, spans and all. Two nested reads that differ
            // only in where they sit are not a case this question is ever asked
            // about, and claiming they are the same would be a guess.
            (left, right) => left == right,
        }
    }
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
    /// `$name` — a value the caller supplies when the script is run.
    ///
    /// Present only between parsing and binding: [`Script::bind`] replaces every
    /// one of these with the [`ExprKind::Literal`] it is bound to, so nothing
    /// downstream — the planner, the index chooser, the evaluator — ever meets
    /// one. That is the whole design: substitution happens **after** parsing, so
    /// there is no stage left at which a supplied value could be read as
    /// grammar.
    Parameter(String),
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
    /// `count(*)`, `mean(age)` — a fold over the records of a group.
    ///
    /// An expression node rather than a projection of its own, so a fold
    /// composes: `mean(price) * 1.2` is arithmetic whose left operand happens to
    /// collapse many records into one.
    ///
    /// It is legal only in a projection, and only in a read that groups —
    /// which is every read that contains one, since a fold is what makes a read
    /// grouped. In a condition it is refused and the refusal names what it would
    /// be: a filter over groups is `HAVING`, which has its own scoping rule and
    /// its own row in the specification's list of absences.
    ///
    /// Its value is **constant within a group**, and the evaluator makes that
    /// literally true rather than claiming it: the fold is computed once per
    /// group and substituted into the tree as a literal before the enclosing
    /// expression is evaluated.
    Fold {
        /// Which fold.
        fold: Aggregate,
        /// What it folds over — absent for `count(*)`, which folds over the
        /// records themselves rather than over a value in them.
        over: Option<Box<Expr>>,
        /// The instant each value was observed at, for the counter folds —
        /// `increase(v, at)` orders its values by it (ADR-0088 §5) — or the rank
        /// `approx_quantile(v, q)` is asked at (ADR-0122 C3). `None` for every
        /// other fold.
        at: Option<Box<Expr>>,
        /// Where it was written.
        span: Span,
    },
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
    /// `IF <test> THEN <a> ELSE <b> END` — a value that depends on a test.
    ///
    /// An expression rather than a statement, deliberately: what was missing was
    /// not control flow but the ability to *compute* a value conditionally — in
    /// a projection, an assignment, a filter, an ordering. A statement form
    /// would have served none of those positions.
    ///
    /// Without an `ELSE` the answer is `NONE`, which is what a path into a field
    /// the record does not have already answers — so the two absences compose
    /// rather than needing a rule apiece.
    If {
        /// The test, which must answer with a boolean.
        condition: Box<Expr>,
        /// The value when it holds.
        then: Box<Expr>,
        /// The value when it does not; absent means `NONE`.
        otherwise: Option<Box<Expr>>,
    },
    /// `a ?? b` — the left value unless it holds nothing.
    ///
    /// "Holds nothing" is `NONE` **or** `NULL`, and this is the one place the
    /// language treats the two alike. It is the right place: the question `??`
    /// asks is *"is there a value here for me to use"*, and the answer is no in
    /// both cases. Everywhere else keeps them apart, which is why `= NONE` and
    /// `= NULL` remain different questions.
    ///
    /// The right side is evaluated **only** when the left holds nothing, so
    /// `cached ?? (SELECT …)` does not pay for a read it does not need.
    Coalesce(Box<Expr>, Box<Expr>),
    /// `$after.total`, `$before.lines[0]` — a route into a value, rather than
    /// into the record a condition is testing.
    ///
    /// Written after a parameter, which is the one value a script holds whose
    /// fields it may want: an event's `$before` and `$after`, a `LET`'s result
    /// (ADR-0110). A step that reaches nothing answers `NONE`, the absence a
    /// missing field already is everywhere else, so `$before.v ?? 0` reads a
    /// created record as zero. `[*]` is not a step here: a route into one value
    /// answers one value.
    Route {
        /// The value walked into.
        value: Box<Expr>,
        /// The steps, `.field` and `[n]`.
        steps: Vec<tessari_types::Step>,
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
    /// `TTL sessions:'abc'` — how long the key has left: a duration, `NULL`
    /// when it never expires, `NONE` when there is no key (G035).
    Ttl(RecordTarget),
    /// `(SELECT * FROM users:1)` in a value position.
    Select(Box<Select>),
}

/// What `DEFINE TOPIC` declares after the name (G037). Every clause is optional
/// and each appears at most once.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TopicClauses {
    /// `RETAIN 7d` — how long a message is kept.
    pub retain: Option<Duration>,
    /// `MAX BYTES n` — the most bytes one message may encode to.
    pub max_bytes: Option<u64>,
    /// `RETAIN BYTES n` — the most payload bytes the topic keeps; past it the
    /// oldest messages are removed by the commit that appends (G055 C8).
    pub retain_bytes: Option<u64>,
    /// `PUBLIC RATE n PER d` — appends a caller nobody signed in may make, per
    /// window, on each node.
    pub public: Option<(u64, Duration)>,
    /// `DEDUPLICATE 5m ON msg_id` — a message whose `msg_id` was published less
    /// than the window ago is not appended (ADR-0124 D8).
    pub deduplicate: Option<(Duration, String)>,
}

/// What `DEFINE GROUP` declares after the topic (G042, ADR-0086).
///
/// The deadline is required and has no default, as a queue's `TIMEOUT` has
/// none: how long a reader has before its message is handed to another is a
/// choice about the reader's work that the store cannot guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupClauses {
    /// `ACK DEADLINE 30s` — how long a reader has to acknowledge a message.
    pub deadline: Duration,
    /// `DELIVERIES n` — after this many deliveries a message is dead-lettered.
    pub deliveries: Option<u64>,
    /// `IN FLIGHT n` — the most messages the group holds unacknowledged.
    pub in_flight: Option<u64>,
    /// `DEAD LETTER TO <topic>` — where a dead-lettered message is appended.
    pub dead_letter: Option<TableRef>,
}

/// `MAX n [EVICT NONE]` on `DEFINE SPACE` (G036).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceBound {
    /// The most keys the space holds; never zero.
    pub max: u64,
    /// `EVICT NONE`: refuse a write past the limit instead of evicting the
    /// least recently modified keys.
    pub refuse: bool,
}

/// When a conditional `SET` writes (G035).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetCondition {
    /// `IF ABSENT` — only when there is no key.
    Absent,
    /// `IF PRESENT` — only when there is one.
    Present,
    /// `IF = <value>` — only when the key holds this value.
    Equals(Expr),
}

/// How a record target names the record.
///
/// A record is `table:id`, and the two halves are different things: the table is
/// a **name**, which a caller may never supply, and the id is a **value**, which
/// they may. So a parameter stands here and nowhere else in an identity —
/// `GET sessions:$token` is the shape a key-value read actually has, and
/// building it as text is the string-building parameters exist to remove.
///
/// [`Script::bind`] replaces every [`Identity::Parameter`] with the value it is
/// bound to, so nothing past binding meets one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// Written in the statement.
    Fixed(RecordId),
    /// Supplied by the caller.
    Parameter(String),
}

impl Identity {
    /// The identity, once it is one.
    ///
    /// # Errors
    ///
    /// [`crate::Error::UnboundParameter`] when a parameter reaches execution,
    /// which binding makes unreachable — so this says the script was run without
    /// being bound rather than guessing at a value.
    pub fn fixed(&self, span: Span) -> crate::Result<&RecordId> {
        match self {
            Self::Fixed(id) => Ok(id),
            Self::Parameter(name) => Err(crate::Error::UnboundParameter {
                name: name.clone(),
                span,
            }),
        }
    }
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
