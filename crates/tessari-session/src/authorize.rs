//! Whether a session may do what it is about to.
//!
//! Four questions, asked in order and never merged, because they fail for
//! different reasons and send the reader to different people:
//!
//! 1. **The authority** — does this identity hold what the statement demands,
//!    anywhere at all? Changed by a `GRANT … ON` at any reach.
//! 2. **The tenancy** — is what it names its own? Fixed at the resolution of a
//!    namespace and a database, which is the one place every path reaching a
//!    record must pass.
//! 3. **The authority again, at the container reached** — a holding over one
//!    database must not answer for its sibling, and the first question cannot
//!    see the difference because holding it *there* is still holding it.
//! 4. **The grants** — has it been given this table? Changed by one more
//!    `GRANT`.
//!
//! The first used to be *the role*, and a rank could not express the rule this
//! store was asked for: writing records in a namespace and creating databases
//! in it are independent in both directions, and in a total order they cannot
//! be. What replaced it is a set of `(kind, reach)` pairs and a subset test.
//!
//! # Not everything that reads records is a statement
//!
//! [`Session::may_read`] and [`Session::readable`] exist because of that: a
//! subscription takes records from the log and never reaches the executor, so a
//! check attached to *running a statement* does not cover it. Attaching a rule
//! to a mechanism rather than to a capability is what let this store grow two
//! authorization holes on the change feed, and the answer is that both questions
//! are asked here rather than a second time somewhere else.

mod grants;
mod tenancy;
use tessari_encoding::{LogId, LogRecord, NODE_ID_LEN};
use tessari_ql::StatementKind;
use tessari_storage::{Catalog, Kind, Reach, Role, Store, Verb};
use tessari_types::{Sequence, TableId};

use crate::error::{Error, Result};
use crate::identity::{Identity, Needs};
use crate::session::Session;

impl<'a> Session<'a> {
    /// Refuse when this session may not read.
    ///
    /// # Why a session has to answer this at all
    ///
    /// Because not everything that reads records is a statement. A subscription
    /// takes records from the log directly and never reaches the executor, so
    /// without this it would be reading with no identity check whatsoever — on a
    /// closed store, an anonymous caller receiving every write there is.
    ///
    /// It is here rather than in whatever asks because "who may read" is one
    /// rule, and a second copy of it in a network surface is a second place for
    /// it to be answered differently.
    ///
    /// # Why it re-reads, and why that makes it `&mut`
    ///
    /// Because its caller is a *loop*. A subscription that asked this once and
    /// then pushed for an hour would be bounded by the connection rather than by
    /// anything a revocation could reach — the same defect a statement path had
    /// until [`Session::refresh`] existed, but lasting longer. So this is the
    /// identity gate a feed re-asks every round, and what it establishes is what
    /// the rest of that round reads: [`Session::readable`] and the field
    /// visibility both run against the record this call just read.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotSignedIn`] on a closed store with no identity, and
    /// [`Error::RoleForbids`] when the role is not enough.
    pub fn may_read(&mut self, store: &Store) -> Result<()> {
        let open = self.refresh(store)?;
        // A span over nothing, because there is no script here to point into —
        // and inventing one would put a caret under a character nobody wrote.
        self.identity
            .allows_needs(Needs::READ, open, tessari_ql::Span::new(0, 0))
    }

    /// Refuse when this session may not take the log.
    ///
    /// # Taking the log is not reading the records
    ///
    /// A subscription is a peer asking for the store's mutations as they were
    /// written, and the log carries more than the records anybody can `SELECT`:
    /// it carries the system tenancy too — the definitions, and the users,
    /// credentials and grants that travel to every subscriber. So this demands
    /// [`Kind::Replicate`] and not [`Kind::Read`], and a caller holding `read`
    /// over the whole store is refused here. That is the disclosure the split
    /// exists to prevent, and it is stated rather than left to be noticed.
    ///
    /// It is separate from [`Kind::Operate`] in the other direction: receiving
    /// the log and reading what the cluster is doing are two grants, so a
    /// replica need not be an operator and an observer need not be a replica.
    ///
    /// # Why it lives here and not where the request arrives
    ///
    /// The same reason [`Session::may_read`] does. A refusal decided in a
    /// network surface is a second place for this question to be answered, and
    /// the one that drifts is the one nobody is reading. This store has already
    /// paid for that once on the change feed.
    ///
    /// # Why it re-reads, and why that makes it `&mut`
    ///
    /// Because its caller is a loop. Asked once and then streamed from for an
    /// hour, the authorization would be bounded by the connection rather than by
    /// anything a revocation could reach — [`Session::may_read`]'s reason,
    /// lasting longer.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotSignedIn`] on a closed store with no identity,
    /// [`Error::RoleForbids`] when the authority is not held at all, and
    /// [`Error::NotTheWholeStore`] when it is held somewhere narrower than the
    /// subscription asks for.
    pub fn may_replicate(&mut self, store: &Store, over: Reach) -> Result<()> {
        let open = self.refresh(store)?;
        // A span over nothing: there is no script here to point into, and
        // inventing one would put a caret under a character nobody wrote.
        let span = tessari_ql::Span::new(0, 0);
        let Some(user) = self.identity.user() else {
            // Anonymous. On an open store that is allowed, here as everywhere
            // else — a store nobody has closed has no identity to refuse, and a
            // cluster has to be able to start before its first user exists. On a
            // closed store it is `NotSignedIn`, which is a different answer from
            // "I know you and no" and a client needs to tell them apart.
            return if open {
                Ok(())
            } else {
                Err(Error::NotSignedIn { span })
            };
        };
        if !user
            .authorities
            .iter()
            .any(|held| held.kind == Kind::Replicate)
        {
            return Err(Error::RoleForbids {
                role: user.role.map_or("authorities", Role::name),
                needs: Kind::Replicate.name(),
                span,
            });
        }
        if !user.authorities.permits(Kind::Replicate, over) {
            // Held, but not this far. Reported as the reach it is rather than as
            // a missing authority, because those two send the reader to
            // different people: one needs a grant, the other needs a wider one.
            return Err(Error::NotTheWholeStore {
                user: user.name.clone(),
                span,
            });
        }
        Ok(())
    }

    /// Log records for a peer, and the only authorized way to reach them.
    ///
    /// # The door is the enforcement, not a rule somebody applies
    ///
    /// [`Session::may_replicate`] could have been left for each caller to ask
    /// before reading the log itself. That is the arrangement that produced the
    /// change feed's two holes: the check was attached to a mechanism, and the
    /// next mechanism that read records inherited nothing. So the check and the
    /// read are one call, and a surface that wants the log for a peer cannot get
    /// it without passing through here.
    ///
    /// # The subscription's scope is the same object the authority is
    ///
    /// `over` is both halves of the question: what the caller must be permitted
    /// for, and what the stream then carries. That is not a convenience — it is
    /// what makes the two impossible to disagree. A design in which the check
    /// took one value and the filter took another would have a state in which a
    /// caller authorized for one namespace is served another, and nothing in
    /// either call would be wrong on its own.
    ///
    /// A store-reach subscriber receives the whole log. A narrower one receives
    /// every sequence, with the mutations outside its reach elided — including
    /// the users, credentials and grants that are not its tenancy's, which is
    /// the disclosure [`Reach`] is carrying here rather than merely naming.
    ///
    /// # The follower names itself, by the id it gave itself
    ///
    /// `node` is not decoration and it is not optional. A membership row that
    /// names no node is a statement about every node holding the log; a pull
    /// that names no follower is progress belonging to nobody, and a leader
    /// cannot report per-follower lag over rows that are not per follower. The
    /// id is the node's own sixteen bytes, so a follower restored from a backup
    /// arrives as a new follower with no inherited progress — the same property
    /// the desired-role binding relies on one level up.
    ///
    /// # An empty answer is still a collection
    ///
    /// A follower that is level asks and receives nothing. That pull is
    /// recorded, because `from` is the follower's own assertion that it holds
    /// `from - 1`, and because treating silence and being-up-to-date as the
    /// same event would report a healthy follower as absent.
    ///
    /// # Errors
    ///
    /// Returns whatever [`Session::may_replicate`] refuses with, or an error
    /// when the log cannot be read or a stored record cannot be decoded.
    pub fn replicate_from(
        &mut self,
        store: &Store,
        node: [u8; NODE_ID_LEN],
        over: Reach,
        log: LogId,
        from: Sequence,
        limit: usize,
    ) -> Result<Vec<(Sequence, LogRecord)>> {
        self.may_replicate(store, over)?;
        let records = store.log_records_within(over, log, from, limit)?;
        // What the follower now holds: the last sequence it was handed, or —
        // when it was handed nothing — the position it told us it was at.
        let reached = records.last().map_or_else(
            || Sequence::new(from.get().saturating_sub(1)),
            |(sequence, _)| *sequence,
        );
        // Recorded here because the door is the only way through, so a peer
        // read that goes unrecorded is not expressible. That is the same
        // argument the door itself was built on.
        store.follower_served(node, log.home, reached);
        Ok(records)
    }

    /// Which tables this session may read, when its user is grant-governed.
    ///
    /// `None` means **no restriction beyond the tenancy** — the user has no
    /// grants, so their role governs, which is the same answer an anonymous
    /// session on an open store gets.
    ///
    /// # Why this exists beside [`Session::may_read`]
    ///
    /// `may_read` answers "may this identity read *at all*", which is the role
    /// question. A grant is the *table* question, and a caller reading records
    /// outside the executor has to ask both — the change feed being the one that
    /// does, and the one where forgetting it means a subscriber receiving a
    /// table nobody granted them.
    ///
    /// The shape is deliberately a filter rather than a refusal: a
    /// grant-governed subscriber watching "everything" should see everything
    /// they were granted, which is what the same user's `SELECT` per table would
    /// answer. Refusing them outright would be a second rule.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read.
    pub fn readable(&self, store: &'a Store) -> Result<Option<Vec<TableId>>> {
        let mut transaction = store.begin()?;
        let readable = self.readable_in(&mut transaction);
        transaction.rollback();
        readable
    }

    /// The same question, for a caller already inside a transaction.
    ///
    /// `INFO FOR DATABASE` is the caller: it runs as a statement, so it has one,
    /// and it must narrow the tables it reports to exactly these. It shares this
    /// implementation rather than asking the catalog itself, because a second
    /// answer to "which tables may this session read" is one waiting to disagree
    /// silently — which is the reason the change feed calls the redactor instead
    /// of reimplementing the field rule.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read.
    pub(crate) fn readable_in(
        &self,
        transaction: &mut tessari_storage::Transaction<'_>,
    ) -> Result<Option<Vec<TableId>>> {
        let Some(user) = self.identity.user() else {
            return Ok(None);
        };
        let grants = Catalog::new(transaction).grants_for(user.id)?;
        if grants.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            grants
                .into_iter()
                .filter(|grant| grant.verbs.contains(&Verb::Read))
                .map(|grant| grant.table)
                .collect(),
        ))
    }

    /// Refuse the statement when this session may not run it.
    ///
    /// The check is one catalog read per statement. It reads `is_open` — whether
    /// the store has any user at all — because an empty store must stay usable,
    /// and that is a property of the data rather than of the session, and it
    /// re-reads this session's own user in the same transaction, for the reason
    /// [`Session::refresh`] gives.
    pub(crate) fn authorize(
        &mut self,
        store: &'a Store,
        kind: &StatementKind,
        span: tessari_ql::Span,
    ) -> Result<()> {
        let open = self.refresh(store)?;
        self.authorize_as_refreshed(store, kind, open, span)
    }

    /// The same check, for a session whose identity this statement already
    /// re-read — an event body, which runs inside the write that refreshed it
    /// (ADR-0110 D6). `open` is what that refresh answered.
    pub(crate) fn authorize_as_refreshed(
        &mut self,
        store: &'a Store,
        kind: &StatementKind,
        open: bool,
        span: tessari_ql::Span,
    ) -> Result<()> {
        // A closed store's one door for a caller nobody signed in (G037).
        if !open && self.identity.user().is_none() {
            return self.public_append(store, kind, span);
        }
        self.identity.allows(kind, open, span)?;
        self.within_tenancy(kind, span)?;
        self.within_authority(store, kind, span)?;
        self.within_grants(store, kind, span)
    }

    /// Re-read this session's own user, and answer whether the store is open.
    ///
    /// # Why a session cannot trust the record it signed in with
    ///
    /// Because half of what a user holds lives in that record and half does
    /// not, and until this existed the two halves revoked on different
    /// schedules. Grants are read from the catalog inside the statement's own
    /// transaction, so `REVOKE … ON TABLE` binds on the next statement. The
    /// role, the tenancy and — since authorities became a set — **everything
    /// this store's permission model is about** lived in a copy taken once at
    /// `sign_in` and never read again, so `ALTER USER`, `DROP USER` and
    /// `REVOKE … ON REACH` reached a connection that was already open **never**.
    /// Both halves are called permissions from outside and nothing distinguished
    /// them, which is the shape a permission cache failure always has.
    ///
    /// So the bound is now the same for both: **one statement**. A surface where
    /// the connection is the session — the wire protocol — used to be bounded by
    /// the connection's lifetime, which is to say by nothing.
    ///
    /// # It costs no transaction and no scan
    ///
    /// The `is_open` read was already here and already scans the users table.
    /// This adds a point read beside it in the same transaction, which is why
    /// the honest description of the cost is one key lookup per statement rather
    /// than one catalog round-trip.
    ///
    /// # A user who has gone
    ///
    /// leaves the session [`Identity::Anonymous`] — the store no longer knows
    /// you — and the ordinary rule then answers, rather than a second rule
    /// invented here. On a store whose *last* user was just dropped that means
    /// the session may do anything, because an empty store is open and there
    /// would otherwise be no way back into it.
    ///
    /// # It reads committed state
    ///
    /// deliberately: it opens its own transaction, so a script that alters its
    /// own user mid-transaction does not re-authorize against a change nobody
    /// has committed yet.
    pub(crate) fn refresh(&mut self, store: &Store) -> Result<bool> {
        let signed = self.identity.user().map(|user| user.id);
        let mut transaction = store.begin()?;
        let catalog = Catalog::new(&mut transaction);
        let open = catalog.is_open()?;
        let found = match signed {
            Some(id) => catalog.user(id)?,
            None => None,
        };
        transaction.rollback();
        if signed.is_some() {
            self.identity =
                found.map_or(Identity::Anonymous, |user| Identity::Signed(Box::new(user)));
        }
        Ok(open)
    }
}
