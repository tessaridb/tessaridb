//! What a statement needs its caller to hold, and where.

mod statements;

use tessari_ql::{CreateTarget, Expr, ExprKind, InfoSubject, StatementKind};
use tessari_storage::{Kind, UserDefinition};

/// What a statement demands: a set of authority kinds, and where they are held.
///
/// # A set, because a rank was the defect
///
/// This used to be one of five ordered classes, and the last arm of [`Needs::of`]
/// assigned `Write` to **twenty-six** statements that are three different
/// authorities: nine that change records, fifteen that create and drop the
/// containers records live in, and two — `DEFINE NAMESPACE` and `DROP NAMESPACE`
/// — whose subject is the store itself rather than anything inside it. Those are
/// the counts of the arm **as it stood at the split**, not of the tree today: the
/// `manage` arm has taken every table kind declared since. So *writing in a
/// namespace* and *creating databases in it* were one permission, and no repair
/// that kept an ordering could separate them — put managing above writing and
/// every manager writes, put it below and every writer manages, and there is no
/// third position. Splitting that arm is the whole of the owner's fourth rule.
///
/// # Exhaustive, and the new way to be wrong
///
/// [`Needs::of`] still has no catch-all, so a statement added to the language
/// cannot compile until somebody classifies it. That protects against a
/// **missing** arm and not against a **thin** one: `{write}` type-checks exactly
/// like `{read, write}`, so an arm with too few kinds is a silent privilege
/// escalation the compiler cannot see. The three sets that exist *only* because
/// something is disclosed — `BACKUP`, `CREATE`/`UPDATE`, `DEFINE KAFKA CONSUMER` —
/// each carry a negative test holding the lesser authority alone, and that test
/// is the only thing standing where the compiler cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Needs {
    /// Every kind the statement requires — all of them, never one of them.
    pub(crate) kinds: &'static [Kind],
    /// Where they must be held.
    pub(crate) at: At,
}

/// Where a demand has to be answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum At {
    /// The store itself, and nothing smaller.
    ///
    /// A caller holding a tenancy of their own does not qualify however much
    /// they hold inside it, because the subject of these statements is the thing
    /// that *contains* their tenancy. An owner of one database must not satisfy
    /// this, or `BACKUP` hands them every record in every namespace.
    Store,
    /// Every container the statement reaches.
    ///
    /// The resolved tenancy of each table it names, and the session's own when
    /// it names none — which is what stops a statement naming no table from
    /// passing a per-container loop vacuously, the shape this codebase has had
    /// to refuse `BACKUP` by name for.
    Reached,
}

impl Needs {
    /// Nothing at all: the statement demands no authority.
    ///
    /// A transaction verb, which acts on nothing — the authority is demanded by
    /// the statements inside it. On a closed store a caller still has to be
    /// signed in, and that is a different question, asked in [`Identity`].
    const NOTHING: Self = Self {
        kinds: &[],
        at: At::Reached,
    };
    /// Reading records or the catalog.
    pub(crate) const READ: Self = Self {
        kinds: &[Kind::Read],
        at: At::Reached,
    };
    /// Changing records without learning anything about them.
    const WRITE: Self = Self {
        kinds: &[Kind::Write],
        at: At::Reached,
    };
    /// Changing records by a statement whose refusal discloses prior state.
    const READ_WRITE: Self = Self {
        kinds: &[Kind::Read, Kind::Write],
        at: At::Reached,
    };
    /// Creating and dropping a container's children, and shaping them.
    const MANAGE: Self = Self {
        kinds: &[Kind::Manage],
        at: At::Reached,
    };
    /// The same, where the container is the store — declaring a namespace.
    const MANAGE_STORE: Self = Self {
        kinds: &[Kind::Manage],
        at: At::Store,
    };
    /// Declaring users and moving authority around.
    const GOVERN: Self = Self {
        kinds: &[Kind::Govern],
        at: At::Reached,
    };
    /// Running the node: topology and replicas.
    const OPERATE_STORE: Self = Self {
        kinds: &[Kind::Operate],
        at: At::Store,
    };
    /// Running the node *and* seeing everything in it — the backup file.
    const READ_OPERATE_STORE: Self = Self {
        kinds: &[Kind::Read, Kind::Operate],
        at: At::Store,
    };
    /// Creating, filling and reading a file on the node — a restore.
    const MANAGE_WRITE_OPERATE_STORE: Self = Self {
        kinds: &[Kind::Manage, Kind::Write, Kind::Operate],
        at: At::Store,
    };
    /// Governing, where the thing governed is the store — the audit trail.
    ///
    /// [`Self::GOVERN`] with the container widened, as [`Self::MANAGE_STORE`] is
    /// to [`Self::MANAGE`]. Nothing new is invented: the audit trail is a
    /// question about identities, which is what `govern` answers, and it is held
    /// store-wide because a vault read is recorded before anybody knows whose
    /// tenancy it belonged to. An owner of one namespace must not satisfy it, or
    /// they read every other namespace's reads.
    const GOVERN_STORE: Self = Self {
        kinds: &[Kind::Govern],
        at: At::Store,
    };
    /// Declaring a thing that will later write on the declarer's behalf.
    const MANAGE_WRITE: Self = Self {
        kinds: &[Kind::Manage, Kind::Write],
        at: At::Reached,
    };
    /// Declaring a thing that will later read a topic and write on the
    /// declarer's behalf — a topic consumer (ADR-0087).
    const MANAGE_READ_WRITE: Self = Self {
        kinds: &[Kind::Manage, Kind::Read, Kind::Write],
        at: At::Reached,
    };

    /// The kinds demanded.
    pub(crate) const fn kinds(self) -> &'static [Kind] {
        self.kinds
    }

    /// Where they must be held.
    pub(crate) const fn at(self) -> At {
        self.at
    }

    /// Whether this demand is satisfied by reading alone.
    ///
    /// The one question the table-grant loop asks of it: a grant is a verb on a
    /// table, and there are two verbs. Everything that is not purely a read
    /// needs the write, which is the mapping this had before the kinds existed.
    pub(crate) const fn only_reads(self) -> bool {
        matches!(self.kinds, [Kind::Read])
    }

    /// The first demanded kind this user holds nowhere at all.
    ///
    /// # The role ladder used to answer this, and could not
    ///
    /// It asked whether a rank was high enough, so it could only say *more* or
    /// *less* — and the rule this store had to express is that writing records
    /// and managing containers are neither. The question now is whether the
    /// user's stored set contains the kind, and a set answers it directly.
    ///
    /// **This is the coarse half of two.** It asks whether the authority is held
    /// *anywhere*, which is what makes the refusal arrive with a useful message
    /// before any name is resolved. Whether it is held at the container the
    /// statement actually reaches is [`crate::Session::within_authority`], and
    /// neither is sufficient alone: this one would let a holder of `manage` over
    /// one database manage a sibling, and that one passes vacuously over a
    /// statement that names no table.
    pub(crate) fn unheld_by(self, user: &UserDefinition) -> Option<Kind> {
        self.kinds
            .iter()
            .copied()
            .find(|kind| !user.authorities.iter().any(|held| held.kind == *kind))
    }
}

/// Whether an expression reads this node's own identity anywhere inside it.
///
/// Reading `$node` is administering rather than reading (see [`Needs::of`]), and
/// an expression can carry that read down inside a group, a call argument or an
/// array. Answering the question shallowly would let the deeper spelling through
/// with a viewer's permission, which is the whole reason this walks.
pub(crate) fn holds_node_read(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Select(select) => matches!(select.from, tessari_ql::Source::Node),
        ExprKind::Not(inner) | ExprKind::Negate(inner) => holds_node_read(inner),
        ExprKind::Route { value, .. } => holds_node_read(value),
        ExprKind::If {
            condition,
            then,
            otherwise,
        } => {
            holds_node_read(condition)
                || holds_node_read(then)
                || otherwise.as_deref().is_some_and(holds_node_read)
        }
        ExprKind::Coalesce(left, right) => holds_node_read(left) || holds_node_read(right),
        ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Arithmetic { left, right, .. }
        | ExprKind::Binary { left, right, .. } => holds_node_read(left) || holds_node_read(right),
        ExprKind::Fold { over, .. } => over.as_deref().is_some_and(holds_node_read),
        ExprKind::Call { arguments, .. } => arguments.iter().any(holds_node_read),
        ExprKind::Array(items) | ExprKind::Set(items) => items.iter().any(holds_node_read),
        ExprKind::Object(fields) => fields.iter().any(|field| holds_node_read(&field.value)),
        ExprKind::Range(range) => holds_node_read(&range.start) || holds_node_read(&range.end),
        ExprKind::Literal(_)
        | ExprKind::Parameter(_)
        | ExprKind::Path(_)
        | ExprKind::Table(_)
        | ExprKind::Record(_)
        | ExprKind::Get(_)
        | ExprKind::Ttl(_) => false,
    }
}
