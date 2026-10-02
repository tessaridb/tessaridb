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
mod atomic;
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
            backups: None,
            at_rest: None,
            budget: None,
            sink: crate::backup_to::Sink::none(),
            landed: false,
            acknowledge_open: None,
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

    /// Open this session counting sign-in tries against the cluster's one
    /// budget (ADR-0108 D5) as well as this node's own.
    #[must_use]
    pub fn budgeted(mut self, budget: Arc<dyn crate::throttle::Budget>) -> Self {
        self.budget = Some(budget);
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

    /// Run a script that another node sent on its caller's behalf (ADR-0108 D2).
    ///
    /// As [`Session::run_with`], in the namespace and database the caller's
    /// own session had selected, except that a script holding any statement
    /// that changes authority or membership, or reads or replaces the whole
    /// store, is refused whole before anything in it runs.
    ///
    /// The selection is put in front of the script as a `USE` statement that is
    /// BUILT rather than written, so names another node sent can never become
    /// syntax, and so the tenancy rule a `USE` carries is applied to it.
    ///
    /// # Errors
    ///
    /// [`Error::MayNotTravel`] naming the first such statement; otherwise as
    /// [`Session::run_with`].
    pub fn run_coordinated(
        &mut self,
        (namespace, database): (Option<&str>, Option<&str>),
        source: &str,
        parameters: &Parameters,
    ) -> Result<Vec<Outcome>> {
        let mut script = parse(source)?;
        if let Some(statement) = script
            .statements
            .iter()
            .find_map(|statement| crate::administration::stays_home(&statement.kind))
        {
            return Err(Error::MayNotTravel { statement });
        }
        if namespace.is_some() || database.is_some() {
            let span = tessari_ql::Span { start: 0, end: 0 };
            let named = |text: Option<&str>| {
                text.map(|text| tessari_ql::Name {
                    text: text.to_owned(),
                    span,
                })
            };
            script.statements.insert(
                0,
                tessari_ql::Statement {
                    kind: StatementKind::Use {
                        namespace: named(namespace),
                        database: named(database),
                        consumer: None,
                    },
                    span,
                    acknowledge: None,
                },
            );
        }
        let ran = self.run_script(script.bind(parameters)?)?;
        // The built `USE` answered too; the caller sent none and is owed none.
        Ok(if namespace.is_some() || database.is_some() {
            ran.into_iter().skip(1).collect()
        } else {
            ran
        })
    }

    /// The user this session is signed in as, if any.
    #[must_use]
    pub const fn signed_in(&self) -> Option<&tessari_storage::UserDefinition> {
        self.identity.user()
    }

    /// Run a script that is already parsed and bound, exactly as
    /// [`Session::run_with`] runs one it read.
    ///
    /// Shared with the vault surface, whose statements are built rather than
    /// read so that a passphrase is never text (ADR-0092 D2).
    pub(crate) fn run_script(&mut self, mut script: tessari_ql::Script) -> Result<Vec<Outcome>> {
        let store = self.store;

        // Where a statement may run, asked once for the whole script and before
        // any of it runs — a script that writes must not have its first half
        // committed here and its second half refused.
        //
        // **A read pays nothing for the cluster.** The roles live in the store,
        // so consulting them costs a read, and a node standing alone would pay
        // it on every `SELECT` for an answer that is always yes. `Effect` is
        // pure, so asking it first keeps that cost on the writes it belongs to
        // (ADR-0018's `Alone`, and G008 kill criterion 3).
        if matches!(Effect::of_script(&script), Effect::Write) {
            admits(store.node_identity()?.roles, &script)?;
        }

        let mut open: Option<(Transaction<'a>, tessari_ql::Span)> = None;
        let outcomes = self.run_statements(store, &mut open, &mut script)?;

        if let Some((transaction, span)) = open {
            transaction.rollback();
            return Err(Error::UnclosedTransaction { span });
        }
        Ok(outcomes)
    }

    /// Run a parsed, bound script's statements against `open`, which is the
    /// transaction they join when one is open.
    ///
    /// Shared by [`Session::run_with`], where `open` starts empty and the script
    /// opens and closes its own, and [`Session::atomically`], where the caller
    /// holds it across several scripts.
    fn run_statements(
        &mut self,
        store: &'a Store,
        open: &mut Option<(Transaction<'a>, tessari_ql::Span)>,
        script: &mut tessari_ql::Script,
    ) -> Result<Vec<Outcome>> {
        let mut outcomes = Vec::with_capacity(script.statements.len());
        self.landed = false;

        // By index rather than by iterator, because a `LET` reaches forward: the
        // value it produces is substituted into the statements that have not run
        // yet, so the loop holds `&mut script` across the step.
        let mut at = 0usize;
        while at < script.statements.len() {
            let outcome = self.step(store, open, &script.statements[at])?;
            // A write outside a transaction committed as it ran, and a `COMMIT`
            // committed what the transaction held. A write inside an open
            // transaction has not landed yet: refused at its `COMMIT`, it rolls
            // back with everything beside it.
            let kind = &script.statements[at].kind;
            if matches!(kind, StatementKind::Commit)
                || (open.is_none() && Effect::of(kind) == Effect::Write)
            {
                self.landed = true;
            }
            let outcome = match &script.statements[at].kind {
                StatementKind::Let { name, .. } => {
                    // Substitution, not a lookup table — the same walk the
                    // caller's parameters take, for the same reason: by the time
                    // a statement runs, every name in it is a literal, so the
                    // planner still finds a right-hand side an index can serve.
                    let bound = match outcome {
                        Outcome::Value(value) => value,
                        // Unreachable while `execute` answers a binding with a
                        // value, and named rather than unwrapped so that a
                        // change there is a compile-time conversation.
                        _ => {
                            return Err(Error::BindingIsNotAValue {
                                span: script.statements[at].span,
                            });
                        }
                    };
                    let name = name.clone();
                    let mut supplied = Parameters::new();
                    supplied.insert(name, bound);
                    for later in &mut script.statements[at.saturating_add(1)..] {
                        later.substitute(&supplied)?;
                    }
                    Outcome::Done
                }
                _ => outcome,
            };
            outcomes.push(outcome);
            at = at.saturating_add(1);
        }
        Ok(outcomes)
    }

    /// Whether the script run last committed anything before it ended.
    ///
    /// Read by the wire and HTTP surfaces before a refusal becomes a redirect
    /// (ADR-0101 D3): a client told to go elsewhere sends the whole script
    /// again, so a script that has already committed part of itself is answered
    /// with the refusal instead.
    #[must_use]
    pub const fn landed(&self) -> bool {
        self.landed
    }

    /// Sign in as `name`, if that password matches.
    ///
    /// **Not a statement**, deliberately: a script is text a caller composes,
    /// logs, pastes into an issue and sends through a proxy, and a password in
    /// one is a password in all of those.
    ///
    /// Signing in again replaces the identity rather than adding to it, so a
    /// session is one conversation with one user at a time.
    ///
    /// # What this costs, and what stops it costing that repeatedly
    ///
    /// Checking a password is expensive by design — nineteen mebibytes and tens
    /// of milliseconds — so an unbounded sign-in path is an amplifier a caller
    /// needs no valid credential to use. Two bounds stand in front of it, both
    /// **before** the store is read: an identity that has missed too many times
    /// in a row is made to wait, and this process runs only so many verifications
    /// at once. See `throttle` for why neither substitutes for the other.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SignInRefused`] for a wrong name and a wrong password
    /// alike — telling them apart tells an attacker which half to keep guessing
    /// at — [`Error::SignInThrottled`] when either bound declined to try, and a
    /// substrate failure otherwise.
    pub fn sign_in(&mut self, name: &str, password: &str) -> Result<()> {
        // First, and before the transaction below: a throttled attempt has to
        // cost a lock and an array index, or the refusal has bounded nothing.
        // The cluster's table when there is one and it answers in time, else
        // this node's own (ADR-0108 D5). Both are kept: this node's own count
        // is what stands in while the shared one cannot be asked.
        let permitted = self
            .budget
            .as_ref()
            .and_then(|budget| budget.permit(name))
            .unwrap_or_else(|| throttle::attempts().permit(name));
        if !permitted {
            log::warn!("sign-in for {name} refused: too many recent failures");
            return Err(Error::SignInThrottled);
        }
        let mut transaction = self.store.begin()?;
        let found = Catalog::new(&mut transaction)
            .users()?
            .into_iter()
            .find(|user| user.name == name);
        transaction.rollback();

        // The place is taken here rather than above, because what it bounds is
        // the memory the hash below holds. Held across the catalog read it would
        // be spent on waiting for a disk instead of on hashing, which refuses
        // callers the memory bound never needed to refuse.
        let Some(_verifying) = throttle::verifying() else {
            // The two limits are one answer to the caller and two lines here,
            // because an operator tuning them needs to know which was reached
            // and an attacker must not.
            log::warn!("sign-in for {name} refused: already verifying as many as this node will");
            return Err(Error::SignInThrottled);
        };

        let Some(user) = found else {
            // The hash is still computed for a name that does not exist, so the
            // time a refusal takes does not say whether the name did.
            let _ = identity::verifies(password, ABSENT_USER_HASH);
            // The name is reported and the reason is not, for the same reason
            // the caller is told neither: a log an operator reads is also a log
            // an attacker reads once they are inside.
            log::warn!("sign-in refused for {name}");
            // Counted against the name that was tried, not against the user that
            // was not found. Counting only known names would let an attacker
            // enumerate the catalog by watching which names start to wait.
            self.missed(name);
            return Err(Error::SignInRefused);
        };
        if !identity::verifies(password, &user.secret) {
            log::warn!("sign-in refused for {name}");
            self.missed(name);
            return Err(Error::SignInRefused);
        }
        log::info!("signed in as {name}");
        throttle::attempts().succeeded(name);
        if let Some(budget) = &self.budget {
            budget.succeeded(name);
        }
        self.identity = Identity::Signed(Box::new(user));
        Ok(())
    }

    /// Count a missed try as `name` here and, in a cluster, in the shared table.
    fn missed(&self, name: &str) {
        throttle::attempts().failed(name);
        if let Some(budget) = &self.budget {
            budget.failed(name);
        }
    }

    /// Act as a user the store already declared, without a credential.
    ///
    /// # Why this exists, and what it is not
    ///
    /// A declared consumer writes records long after the session that declared
    /// it has gone, and until this existed it wrote them as **nobody** — so the
    /// authority question was asked once, at `DEFINE KAFKA CONSUMER`, and never again.
    /// Demoting the declarer, revoking their authority or deleting the account
    /// outright did not stop the writing, because there was no identity in the
    /// loop for any of those to act on.
    ///
    /// This is the rule the rest of the store already follows — *a thing acts
    /// with the authority of whoever asked for it* — reaching the one path that
    /// had escaped it. Because the identity is re-established from the catalog
    /// on every batch, a revocation takes effect on the next one rather than
    /// never.
    ///
    /// **It is not a way around a password.** It takes an id rather than a name
    /// so it cannot be reached from anything a caller types, and every authority
    /// check downstream is the ordinary one — this hands out an identity, not a
    /// permission. It is `pub` only because the ingestion runner is another
    /// crate; an embedder able to call it is already linked against the store
    /// and holds every byte in it, so it crosses no boundary that was not
    /// already open. Nothing reachable over the wire calls it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownUser`] when no user carries that id — which is
    /// what a deleted declarer looks like, and is therefore how deleting one
    /// stops the consumer they declared.
    pub fn acting_as(&mut self, id: u32) -> Result<()> {
        let mut transaction = self.store.begin()?;
        let found = tessari_storage::Catalog::new(&mut transaction)
            .users()?
            .into_iter()
            .find(|user| user.id == id);
        transaction.rollback();
        let Some(user) = found else {
            return Err(Error::UnknownUser { id });
        };
        self.identity = Identity::Signed(Box::new(user));
        Ok(())
    }

    /// A throwaway session on this store, selecting what this one selects, as
    /// somebody else.
    ///
    /// The only caller is `INFO FOR ACCESS TO TABLE`, and it exists because that
    /// statement must not answer from a second reading of the catalog. The
    /// function that decides whether a user may reach a table takes a session
    /// and a statement, so the report builds the session and hands it the
    /// statement — and gets the store's real answer rather than a re-derivation
    /// of it.
    ///
    /// It carries the **asker's** namespace and database rather than the
    /// subject's, because the object being reported on lives in the asker's
    /// selection. A subject declared somewhere else is then refused by the
    /// ordinary tenancy check, which is the report's answer rather than a gap
    /// in it.
    ///
    /// Like [`Session::acting_as`] this hands out an identity and not a
    /// permission: every check downstream is the ordinary one, it takes an id so
    /// nothing a caller types can reach it, and the statement that uses it
    /// already needs `govern`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownUser`] when no user carries that id — which a
    /// caller iterating the catalog it just read will not see, and which is
    /// still an error rather than a silent omission.
    pub(crate) fn probing(&self, id: u32) -> Result<Self> {
        let mut probe = Self {
            store: self.store,
            namespace: self.namespace.clone(),
            database: self.database.clone(),
            identity: Identity::Anonymous,
            // A probe reads a catalog as somebody else and never claims, so it
            // carries no claimant. Copying one would let a permission probe
            // release the real session's work.
            consumer: None,
            // Carried, unlike the claimant: what this node knows about its peers
            // is the same fact whoever is asking, and a probe that lost it would
            // answer a bounded read differently from the session that spawned it.
            elsewhere: self.elsewhere.clone(),
            gather: self.gather.clone(),
            // A probe answers who may do what and never writes a file.
            backups: None,
            at_rest: None,
            budget: self.budget.clone(),
            sink: crate::backup_to::Sink::none(),
            landed: false,
            acknowledge_open: None,
        };
        probe.acting_as(id)?;
        Ok(probe)
    }

    /// Change **this session's own** password, proving the current one.
    ///
    /// **Not a statement**, for the same reason `sign_in` is not: it carries a
    /// credential, and a script is text a caller composes, logs, pastes into an
    /// issue and sends through a proxy.
    ///
    /// # Why this exists beside `ALTER USER`
    ///
    /// `ALTER USER … SET PASSWORD` is *administering somebody*, so it needs an
    /// owner who administers the tenancy they sit in. That is right for
    /// somebody else's credential and leaves a hole for your own: a `viewer` or
    /// an `editor` whose password may have leaked could not rotate it at all,
    /// and had to ask an owner — who then chooses it, and knows it.
    ///
    /// # Why the current password is required
    ///
    /// Because being signed in is not proof of a password. A token can be copied
    /// off a plaintext connection or read out of a log, and if holding one were
    /// enough to set a new password then a stolen token would be a permanent
    /// takeover: the thief locks the owner out, and a closed store has no door
    /// from outside. So this asks for the password as well as the session.
    ///
    /// That second proof is also what makes it safe for this to be the one path
    /// that touches a user without administering them: the subject is always the
    /// caller, so there is no subject to bound.
    ///
    /// Every token this user holds stops working, because a ticket is checked by
    /// comparing the record it was cut from and the record has changed.
    ///
    /// # Errors
    ///
    /// [`Error::NotSignedIn`] for an anonymous session, [`Error::CurrentPasswordRefused`]
    /// when the current password does not match, [`Error::PasswordEmpty`] when
    /// the new one is empty, [`Error::Unknown`] when the user has been removed
    /// since signing in, and a substrate failure otherwise.
    pub fn change_password(&mut self, current: &str, new: &str) -> Result<()> {
        // A span over nothing: no script produced this, and inventing one would
        // put a caret under a character nobody wrote.
        let span = tessari_ql::Span::new(0, 0);
        let Identity::Signed(who) = &self.identity else {
            return Err(Error::NotSignedIn { span });
        };
        let name = who.name.clone();
        let id = who.id;

        // Re-read rather than trust the session's copy, for the reason a ticket
        // is re-read: a session open across a `DROP USER` would otherwise write
        // a hash back over an id the catalog no longer holds.
        let mut transaction = self.store.begin()?;
        let found = Catalog::new(&mut transaction)
            .users()?
            .into_iter()
            .find(|user| user.id == id);
        transaction.rollback();
        let Some(mut user) = found else {
            return Err(Error::Unknown {
                entity: "user",
                name,
                span,
            });
        };

        // The new password is refused **before** the current one is checked, so
        // an unusable new password does not spend a verification. `hash` is what
        // refuses an empty one, in one place for every path that sets a password.
        let secret = identity::hash(new, span)?;

        let Some(_verifying) = throttle::verifying() else {
            return Err(Error::SignInThrottled);
        };
        if !identity::verifies(current, &user.secret) {
            log::warn!("a password change was refused for {}", user.name);
            return Err(Error::CurrentPasswordRefused);
        }

        user.secret = secret;
        let mut transaction = self.store.begin()?;
        Catalog::new(&mut transaction).update_user(&user);
        transaction.commit()?;
        log::info!("{} changed their own password", user.name);
        // The session keeps running as the same user, with the record it now
        // has: leaving the old copy here would make the next `ticket()` cut one
        // against a record that no longer exists.
        self.identity = Identity::Signed(Box::new(user));
        Ok(())
    }

    /// Forget who this session is.
    pub fn sign_out(&mut self) {
        self.identity = Identity::Anonymous;
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
