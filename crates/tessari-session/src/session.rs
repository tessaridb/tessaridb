//! The session: what a script is run against, and what it remembers between
//! statements.
//!
//! A session remembers two things — the namespace and database `USE` selected —
//! and it remembers them **by name, not by id**. Resolving a name to an id once
//! and keeping it would leave a session pointing at a table that has since been
//! dropped and re-created under the same name, reading the wrong one with no
//! error anywhere. The lookup is one catalog read per statement, and the store
//! is the only thing entitled to say what a name currently means.

mod acknowledging;
mod across;
pub use across::{
    AcrossAnswer, AcrossAsk, AcrossRefusal, PartRefused, Participants, Recovery, RefusalKind,
    across_lapse_millis, recover_staging,
};
mod atomic;
mod running;
mod signing;
mod step;

pub use atomic::Atomic;
use std::path::Path;
use std::sync::Arc;

use tessari_ql::{Parameters, StatementKind, parse};
use tessari_storage::{Catalog, Store, Transaction};

use crate::effect::{Effect, admits};
use crate::elsewhere::Elsewhere;
use crate::error::{Error, Result};
use crate::gather::Gather;
use crate::identity::{self, Identity};
use crate::outcome::Outcome;
use crate::throttle;

/// A hash to check a name that does not exist against.
///
/// A refusal for an unknown name must take about as long as one for a wrong
/// password, or the time itself says which half was wrong. This is a real Argon2
/// hash of a value nobody knows, kept so the work happens either way.
///
/// # It has to actually parse, and for a while it did not
///
/// The value here was hand-written and carried four stray spaces before the
/// salt, so `PasswordHash::new` rejected it and `verifies` returned before
/// reaching the hasher. The equalisation this constant exists for had therefore
/// never happened: a refusal for a missing name cost microseconds and one for a
/// wrong password cost tens of milliseconds, which is exactly the oracle the
/// paragraph above says it prevents. Nothing failed, because a sentinel that
/// does not parse and a password that does not match both come back `false`.
///
/// This one is a genuine `hash` of a value nobody kept, produced at the pinned
/// parameters, and `identity`'s tests hold both halves of that: that it parses,
/// and that its parameters are still the ones the hasher uses. The parse
/// assertion is deliberately not written as "verifying against it returns
/// false", because that passes for the broken sentinel too.
pub(crate) const ABSENT_USER_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$2vm2xorx4jAz1i0WAEts5w$Lfjcmkrqa+uTeY2uCU3GXJVSDbqhtpTrsPyYTXiTpLA";

/// A connection's worth of state: where statements run, and against what.
#[derive(Debug)]
pub struct Session<'a> {
    /// Visible to the crate for the same reason `identity` is.
    pub(crate) store: &'a Store,
    /// Visible to the crate because `detached.rs` carries it across a hop.
    pub(crate) namespace: Option<String>,
    /// Visible to the crate for the same reason as `namespace`.
    pub(crate) database: Option<String>,
    /// Visible to the crate because `authorize.rs` asks it three questions.
    pub(crate) identity: Identity,
    /// Who this session is when it claims, once `USE CONSUMER` has said.
    ///
    /// Visible to the crate because `queue.rs` writes it into a record and
    /// compares it on a release.
    pub(crate) consumer: Option<Consumer>,
    /// What this node knows about the copies it does not hold.
    ///
    /// `None` on a node standing alone, which is every deployment that has not
    /// been told about peers — and that is the reason it is optional rather than
    /// a directory that happens to be empty. An empty directory and *no cluster
    /// to ask* are different facts, and only the second one is true of a single
    /// node. Visible to the crate because `evaluate.rs` is the one thing that
    /// asks it anything.
    pub(crate) elsewhere: Option<Arc<dyn Elsewhere>>,
    /// Who fetches the shards of a split table this node lacks (G033).
    ///
    /// `None` on a node told of no peers, and withheld for the length of a
    /// transaction or a `VERSION` read — see [`Session::step`] — so a read
    /// there refuses exactly as it did before gathering existed.
    pub(crate) gather: Option<Arc<dyn Gather>>,
    /// Who carries a record of a transaction across leaders to the leader of
    /// its range (ADR-0112). A fact about the process, like `gather`; `None`
    /// on a node told of no peers, where a commit across leaders is refused.
    pub(crate) participants: Option<Arc<dyn Participants>>,
    /// Where `BACKUP … TO` may write, when the node was given a folder.
    ///
    /// A fact about the process, like `gather`, so it is taken at the session;
    /// `None` refuses every `TO` rather than writing somewhere nobody chose.
    pub(crate) backups: Option<Arc<Path>>,
    /// The key every backup this node produces is sealed under, and every
    /// sealed backup it reads is opened with (ADR-0108 D7). A fact about the
    /// process, like `backups`; `None` writes backups as they are.
    pub(crate) at_rest: Option<Arc<tessari_vault::AtRestKey>>,
    /// The cluster's sign-in budget, when this node is part of one (ADR-0108
    /// D5). A fact about the process, like `gather`.
    pub(crate) budget: Option<Arc<dyn crate::throttle::Budget>>,
    /// The certificates this node presents, read when `INFO FOR NODE` asks
    /// (ADR-0108 D9). A fact about the process, like `budget`; `None` on a node
    /// that presents none.
    pub(crate) certificates: Option<Arc<dyn crate::presented::Certificates>>,
    /// Where a `BACKUP STATE` answered here writes its snapshot instead of
    /// answering with it (ADR-0094 D6), when the caller is streaming.
    pub(crate) sink: crate::backup_to::Sink,
    /// Whether the script run last committed anything (ADR-0101 D3).
    ///
    /// A redirect invites the client to send the same script elsewhere, which
    /// is safe only while none of it has taken effect: a script is not a
    /// transaction, so `CREATE …; SELECT … STALENESS 1s` has committed its
    /// `CREATE` by the time the read is redirected. The edge asks this before
    /// turning a refusal into a redirect.
    pub(crate) landed: bool,
    /// The strongest acknowledgement a write inside the open transaction asked
    /// for, carried to its `COMMIT` (ADR-0106 D2) — a level asked of one write
    /// is asked of the transaction that lands it.
    pub(crate) acknowledge_open: Option<tessari_types::Acknowledge>,
    /// Whether a write in the open transaction said `ACROSS LEADERS`, which
    /// lets its `COMMIT` reach across leaders as though the `COMMIT` had said
    /// it (ADR-0112 D1).
    pub(crate) across_open: bool,
    /// How many events deep this session runs: zero for a caller's session,
    /// one more for each event body a write ran (ADR-0110 D5).
    pub(crate) event_depth: u8,
}

/// Who a session is, to a queue.
///
/// Two halves that mean different things, and the difference is the whole
/// design: the **name** is the client's and a repeated one means *share the
/// work*, while the **instance** is the engine's and cannot repeat at all.
///
/// Kafka has the client supply both, so `group.instance.id` uniqueness is the
/// operator's problem and a duplicate has to be fenced by epoch. Minting the
/// instance here means uniqueness cannot be violated, and the fencing question
/// does not get answered — it stops existing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Consumer {
    /// The name the session declared. Shared on purpose when it is shared.
    pub name: String,
    /// The value this session was minted, unique and never reissued.
    pub instance: String,
}

impl<'a> Session<'a> {
    /// Open a session on a store, with nothing selected.
    #[must_use]
    pub const fn new(store: &'a Store) -> Self {
        Self {
            store,
            identity: Identity::Anonymous,
            namespace: None,
            database: None,
            consumer: None,
            elsewhere: None,
            gather: None,
            participants: None,
            backups: None,
            at_rest: None,
            budget: None,
            certificates: None,
            sink: crate::backup_to::Sink::none(),
            landed: false,
            acknowledge_open: None,
            across_open: false,
            event_depth: 0,
        }
    }

    /// Open this session among the peers `elsewhere` knows about.
    ///
    /// A bounded read this node's own copy cannot satisfy is redirected to a
    /// copy that can, rather than refused — see [`Elsewhere`] for why the
    /// question is asked that way round and [`crate::Error::ReadIsElsewhere`]
    /// for what the client is told.
    ///
    /// Taken at the session and not at the store, because which peers exist is a
    /// fact about this *process's* place in a cluster and a store knows nothing
    /// about networks. A node that was never told about peers never calls this
    /// and refuses exactly as it did before.
    #[must_use]
    pub fn among(mut self, elsewhere: Arc<dyn Elsewhere>) -> Self {
        self.elsewhere = Some(elsewhere);
        self
    }

    /// Open this session able to gather the shards of a split table this node
    /// lacks from their leaders (G033, ADR-0083), rather than refusing a read
    /// that needs them.
    #[must_use]
    pub fn gathering(mut self, gather: Arc<dyn Gather>) -> Self {
        self.gather = Some(gather);
        self
    }

    /// Open this session able to carry the records of a transaction across
    /// leaders to the leaders of its ranges (ADR-0112).
    #[must_use]
    pub fn participating(mut self, participants: Arc<dyn Participants>) -> Self {
        self.participants = Some(participants);
        self
    }

    /// Open this session counting sign-in tries against the cluster's one
    /// budget (ADR-0108 D5) as well as this node's own.
    #[must_use]
    pub fn budgeted(mut self, budget: Arc<dyn crate::throttle::Budget>) -> Self {
        self.budget = Some(budget);
        self
    }

    /// Open this session reporting the certificates `certificates` reads.
    #[must_use]
    pub fn presenting(mut self, certificates: Arc<dyn crate::presented::Certificates>) -> Self {
        self.certificates = Some(certificates);
        self
    }

    /// Open this session able to write `BACKUP … TO` into `folder`, and nowhere
    /// else.
    #[must_use]
    pub fn backing_up_into(mut self, folder: Arc<Path>) -> Self {
        self.backups = Some(folder);
        self
    }

    /// Open this session sealing every backup it produces under `key`, and
    /// opening every sealed backup it reads with it (ADR-0108 D7).
    #[must_use]
    pub fn sealing_backups(mut self, key: Arc<tessari_vault::AtRestKey>) -> Self {
        self.at_rest = Some(key);
        self
    }

    /// Open this session writing the snapshot its next `BACKUP STATE` takes
    /// into `out`, chunk by chunk, rather than answering with the bytes.
    ///
    /// The statement still decides who may take it: this only changes where the
    /// file goes once the statement has been allowed. The answer is then a
    /// summary — the form, the records and the version — since the file has
    /// already left. Used by a surface that writes the file somewhere other than
    /// memory, so a node's memory does not grow with the store it backs up.
    #[must_use]
    pub fn snapshot_into(mut self, out: Box<dyn std::io::Write + Send>) -> Self {
        self.sink = crate::backup_to::Sink::to(out);
        self
    }

    /// The namespace `USE` selected, if any.
    #[must_use]
    pub fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    /// The database `USE` selected, if any.
    #[must_use]
    pub fn database(&self) -> Option<&str> {
        self.database.as_deref()
    }

    /// Read a script and run it, returning one outcome per statement.
    ///
    /// A statement outside `BEGIN` is its own transaction. Inside one, every
    /// statement joins it, so a script may define a table and write to it and
    /// have both land or neither.
    ///
    /// # Errors
    ///
    /// Returns the first failure. Work buffered in an uncommitted transaction is
    /// discarded — nothing reaches the store until `commit`.
    pub fn run(&mut self, source: &str) -> Result<Vec<Outcome>> {
        self.run_with(source, &Parameters::new())
    }

    /// Read a script, give its parameters the values `parameters` binds, and run
    /// it.
    ///
    /// This is what [`Session::run`] does with an empty map, and it exists so a
    /// caller with a value does not have to write that value into the script
    /// text. A parameter is legal wherever a literal is and nowhere a name is,
    /// and binding happens **after** parsing — so a supplied value cannot become
    /// syntax no matter what it holds.
    ///
    /// A binding nobody used is accepted; a parameter nobody bound is refused,
    /// before the first statement runs.
    ///
    /// # Errors
    ///
    /// [`tessari_ql::Error::UnboundParameter`] when the script names a parameter
    /// this map has no value for, and nothing is written when it does. Otherwise
    /// as [`Session::run`].
    pub fn run_with(&mut self, source: &str, parameters: &Parameters) -> Result<Vec<Outcome>> {
        let script = parse(source)?.bind(parameters)?;
        self.run_script(script)
    }

    /// The user this session is signed in as, if any.
    #[must_use]
    pub const fn signed_in(&self) -> Option<&tessari_storage::UserDefinition> {
        self.identity.user()
    }
}

/// Whether a store refusal is the write-write race a second run can win.
const fn conflicting(refusal: &tessari_storage::Error) -> bool {
    matches!(
        refusal,
        tessari_storage::Error::Conflict { .. } | tessari_storage::Error::CommitContention { .. }
    )
}

/// Commit, and let the one refusal a caller fixes with a statement carry that
/// statement.
///
/// Every check that can refuse a write runs inside the commit, so this is the
/// one place a caller's write can be refused by the store, and therefore the one
/// place worth teaching. It is deliberately not a second validation pass: the
/// commit is unchanged and only its failure is read.
fn settle(transaction: Transaction<'_>) -> Result<()> {
    match transaction.commit() {
        Ok(_) => Ok(()),
        Err(refusal) => Err(advised(refusal)),
    }
}

/// A store refusal, with the remedy attached when the remedy is real.
///
/// The suggestion is built from the caller's own field, table and value — never
/// from what else the table declares, which a caller's grants may hide
/// (ADR-0044) — and then **parsed**. A name this store accepts is not always a
/// name the language can spell: a record's fields can arrive from a bound
/// parameter, so one may be a reserved word or hold a space, and the statement
/// naming it would not read back. Suggesting it anyway would be worse than
/// suggesting nothing, because it looks like something to paste. So the parse is
/// the gate, and a suggestion that fails it is dropped rather than repaired.
fn advised(refusal: tessari_storage::Error) -> Error {
    let tessari_storage::Error::UndeclaredField {
        table, field, kind, ..
    } = &refusal
    else {
        return Error::Store(refusal);
    };
    let suggestion = format!("DEFINE FIELD {field} ON {table} TYPE {}", kind.name());
    if parse(&suggestion).is_err() {
        return Error::Store(refusal);
    }
    Error::UndeclaredField {
        refusal: Box::new(refusal),
        suggestion,
    }
}

/// The version clause a statement carries, if it carries one.
///
/// Only a read can: `VERSION` names which state answers the question, and every
/// other statement changes state rather than asking about it. A free function so
/// that the one place deciding which snapshot to open is also the one place that
/// knows which statements may ask for a snapshot at all.
const fn read_version(kind: &StatementKind) -> Option<tessari_ql::Version> {
    match kind {
        StatementKind::Select(select) => select.version,
        _ => None,
    }
}
