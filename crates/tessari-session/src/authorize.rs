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

use tessari_encoding::{LogRecord, NODE_ID_LEN};
use tessari_ql::StatementKind;
use tessari_storage::{Catalog, Kind, Reach, Role, Store, Verb};
use tessari_types::{Sequence, TableId};

use crate::error::{Error, Result};
use crate::identity::{At, Identity, Needs};
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
        from: Sequence,
        limit: usize,
    ) -> Result<Vec<(Sequence, LogRecord)>> {
        self.may_replicate(store, over)?;
        let records = store.log_records_within(over, from, limit)?;
        // What the follower now holds: the last sequence it was handed, or —
        // when it was handed nothing — the position it told us it was at.
        let reached = records.last().map_or_else(
            || Sequence::new(from.get().saturating_sub(1)),
            |(sequence, _)| *sequence,
        );
        // Recorded here because the door is the only way through, so a peer
        // read that goes unrecorded is not expressible. That is the same
        // argument the door itself was built on.
        store.follower_served(node, reached);
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
    fn refresh(&mut self, store: &Store) -> Result<bool> {
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

    /// Refuse a container this user holds no authority over.
    ///
    /// # Why this is beside the grant check and not inside the tenancy one
    ///
    /// [`Session::permits`] runs at every tenancy resolution and does not know
    /// which statement it is resolving for, so it cannot know which authority
    /// kinds are demanded — and giving it the statement would push a permission
    /// decision into the reference resolver, which is statement-agnostic on
    /// purpose. So the kinds are asked here, walking the same resolved tables
    /// [`Session::within_grants`] walks, for the same reason.
    ///
    /// # The two halves, and why neither is sufficient
    ///
    /// `Needs::unheld_by` already refused a caller who holds the demanded kind
    /// **nowhere**. That is the coarse half: it catches the common case — a
    /// holder of `write` running `DEFINE TABLE` — and it gives the refusal a
    /// message before any name is resolved. It cannot catch the other case,
    /// because holding `manage` over one database is holding it *somewhere*, and
    /// under the coarse question alone that would answer for a sibling database
    /// too.
    ///
    /// This half closes that, and it is deliberately not vacuous: a statement
    /// naming no table falls back to the tenancy the session is working in
    /// rather than to an empty loop. An empty loop reading *every container it
    /// names is held* passes for reasons that have nothing to do with
    /// permission, which is the shape this store has already had to refuse
    /// `BACKUP` by name for.
    fn within_authority(
        &self,
        store: &'a Store,
        kind: &StatementKind,
        span: tessari_ql::Span,
    ) -> Result<()> {
        let Some(user) = self.identity.user() else {
            return Ok(());
        };
        let needs = Needs::of(kind);
        if needs.kinds().is_empty() {
            return Ok(());
        }
        for reach in self.reaches(store, kind, needs, span)? {
            for demanded in needs.kinds() {
                if !user.authorities.permits(*demanded, reach) {
                    return Err(Error::RoleForbids {
                        role: user.role.map_or("authorities", Role::name),
                        needs: demanded.name(),
                        span,
                    });
                }
            }
        }
        Ok(())
    }

    /// The containers a statement's authority is demanded at.
    ///
    /// One entry per table it names, resolved to the database that table lives
    /// in — so a statement naming two tables in two databases is authorized
    /// twice, and a read reaching across a qualified name is asked about the
    /// database it reached rather than the one the session selected.
    ///
    /// A table that does not resolve contributes nothing: it is refused by
    /// whatever resolves it, with a message about the table rather than about an
    /// authority, and answering here first would turn *no such table* into a
    /// permission refusal that tells the reader less.
    fn reaches(
        &self,
        store: &'a Store,
        kind: &StatementKind,
        needs: Needs,
        span: tessari_ql::Span,
    ) -> Result<Vec<Reach>> {
        if needs.at() == At::Store {
            return Ok(vec![Reach::Store]);
        }
        let mut reaches = Vec::new();
        for table in crate::reach::tables_named(kind) {
            let mut transaction = store.begin()?;
            let resolved = self.resolve_table(&mut transaction, table);
            transaction.rollback();
            if let Ok((context, _)) = resolved {
                reaches.push(Reach::Database(context.namespace, context.database));
            }
        }
        if reaches.is_empty() {
            // The fallback that stops the loop above passing vacuously. A
            // statement naming no table still acts *somewhere*, and that
            // somewhere is the tenancy the session selected — which is what
            // `DEFINE TABLE`, `DROP DATABASE` and `INFO FOR DATABASE` are all
            // asking about.
            //
            // A session that has selected nothing yet contributes no container,
            // and that is correct rather than a hole: it has resolved no name,
            // so there is nothing for an authority to be held over, and every
            // statement that goes on to name one is asked again at the naming.
            let mut transaction = store.begin()?;
            let selected = self.context(&mut transaction, None, span).ok();
            transaction.rollback();
            if let Some(context) = selected {
                reaches.push(Reach::Database(context.namespace, context.database));
            }
        }
        Ok(reaches)
    }

    /// Refuse a table this user's grants do not name.
    ///
    /// # Grants, if a user has any, are the whole story
    ///
    /// A user with none is governed by their role, which is what lets grants be
    /// added to a store whose users already work without changing what any of
    /// them may do. A user with one reaches exactly what they were granted,
    /// because a role can only widen and a permission system that cannot narrow
    /// is decoration.
    ///
    /// Which tables a statement names comes from [`crate::reach::tables_named`],
    /// an exhaustive match — so a statement form added later cannot reach a
    /// table until somebody has decided whether grants apply to it.
    fn within_grants(
        &self,
        store: &'a Store,
        kind: &StatementKind,
        span: tessari_ql::Span,
    ) -> Result<()> {
        let Some(user) = self.identity.user() else {
            return Ok(());
        };
        let mut transaction = store.begin()?;
        let grants = Catalog::new(&mut transaction).grants_for(user.id)?;
        transaction.rollback();
        if grants.is_empty() {
            return Ok(());
        }

        // A grant names a table that exists. Declaring one therefore has no
        // grant that could permit it, and saying so is better than a refusal
        // that reads like a bug.
        //
        // All four declarations of a table, because `reach::tables_named`
        // returns an EMPTY list for every one of them and says in its own
        // comment that the caller handles them. Anything named there and
        // missing here is not refused by the loop below either — the loop
        // iterates the tables a statement names, and these name none — so it
        // is simply allowed. `DEFINE BUCKET` was in exactly that position
        // before this line listed it.
        if matches!(
            kind,
            StatementKind::DefineTable { .. }
                | StatementKind::DefineSpace { .. }
                | StatementKind::DefineBucket { .. }
                | StatementKind::DefineCollection { .. }
        ) {
            return Err(Error::GrantedUserCannotDeclare {
                user: user.name.clone(),
                span,
            });
        }

        // A backup reaches **every** table and therefore names none, so the loop
        // below — every table this statement names is granted — would pass over
        // it vacuously. That is the same shape as the defect a `READ` falling
        // through a catch-all produced, so it is refused here by name rather than
        // left to an emptiness that reads as permission.
        if matches!(kind, StatementKind::Backup { .. }) {
            return Err(Error::GrantedUserCannotBackUp {
                user: user.name.clone(),
                span,
            });
        }

        // A grant is a verb on a table and there are two verbs, so the kinds
        // collapse here: a demand answered by reading alone asks for the read,
        // and everything else asks for the write. The statements with no table
        // to grant on are refused above by name — a granted user cannot back up
        // or declare — so this mapping is about the reach rather than about them.
        let verb = if Needs::of(kind).only_reads() {
            Verb::Read
        } else {
            Verb::Write
        };
        for table in crate::reach::tables_named(kind) {
            let mut transaction = store.begin()?;
            let resolved = self.resolve_table(&mut transaction, table);
            transaction.rollback();
            // A table that does not resolve is refused by whatever resolves it,
            // with a message about the table rather than about a grant.
            let Ok((_, id)) = resolved else { continue };
            let granted = grants
                .iter()
                .any(|grant| grant.table == id && grant.verbs.contains(&verb));
            if !granted {
                return Err(Error::NotGranted {
                    user: user.name.clone(),
                    table: table.name.text.clone(),
                    needs: verb.name(),
                    span,
                });
            }
        }
        Ok(())
    }

    /// Refuse a resolved tenancy that is not the signed-in user's own.
    ///
    /// This is the check that **cannot be walked around**, and it is here rather
    /// than at `USE` for a reason: a statement may name a database directly —
    /// `SELECT * FROM other.notes` — and never touch the session's selection at
    /// all. Every path that reaches a record first resolves a namespace and a
    /// database into ids, so refusing at that resolution refuses all of them by
    /// construction. Checking only `USE` would guard the front door of a room
    /// with two.
    ///
    /// The refusal names the **tenancy** and never says whether the record or
    /// the table exists: a refusal that leaks that has answered the question it
    /// declined.
    /// `named` is the tenancy as the author wrote it, which is what the refusal
    /// echoes back. Naming it leaks nothing they did not already type, and it is
    /// the only thing here that reads as an answer to what they asked.
    pub(crate) fn permits(
        &self,
        namespace: tessari_types::NamespaceId,
        database: tessari_types::DatabaseId,
        named: &str,
        span: tessari_ql::Span,
    ) -> Result<()> {
        let Some(user) = self.identity.user() else {
            return Ok(());
        };
        let outside = user.namespace.is_some_and(|own| own != namespace)
            || user.database.is_some_and(|own| own != database);
        if outside {
            return Err(Error::OutsideTenancy {
                name: named.to_owned(),
                span,
            });
        }
        Ok(())
    }

    /// Refuse a statement reaching outside the tenancy its user belongs to.
    ///
    /// This one catches `USE` specifically, which resolves no tenancy of its own
    /// — it only records a name for the statements after it. Without it a scoped
    /// user's `USE NAMESPACE other` would succeed and the refusal would arrive
    /// one statement later, naming something the author did not just write.
    fn within_tenancy(&self, kind: &StatementKind, span: tessari_ql::Span) -> Result<()> {
        let Some(user) = self.identity.user() else {
            return Ok(());
        };
        let StatementKind::Use {
            namespace,
            database,
            // A consumer name is not a tenancy: it selects who this session is
            // to a queue, reaches no namespace and no database, and a scoped
            // user naming one is not reaching outside anything.
            consumer: _,
        } = kind
        else {
            return Ok(());
        };
        // A scoped user may only work inside the namespace and database it was
        // declared in, and `USE` is where a session says which those are.
        if user.namespace.is_some() {
            for named in [namespace.as_ref(), database.as_ref()]
                .into_iter()
                .flatten()
            {
                if !self.names_own_tenancy(user, &named.text) {
                    return Err(Error::OutsideTenancy {
                        name: named.text.clone(),
                        span,
                    });
                }
            }
        }
        self.holds_something_named(user, namespace.as_ref(), database.as_ref(), span)
    }

    /// Refuse a `USE` naming a container this user holds nothing at.
    ///
    /// # Selecting is not reading, and it is not nothing either
    ///
    /// `USE` demands **something at the container** rather than `read` on it,
    /// which is the weakest predicate that still closes an oracle. Demanding
    /// `read` would stop a `govern`-only administrator selecting the namespace
    /// they administer, and demanding a read at all would stop a `write`-only
    /// ingestion identity selecting its own database — the model's headline case.
    /// Demanding nothing leaves a signed-in caller able to name any namespace in
    /// the store and learn from the refusal whether it exists.
    ///
    /// The check applies to **every** signed-in caller and not only a
    /// tenancy-scoped one. The scoped ones were already held by the name check
    /// above; the hole was a store-reach caller holding one namespace, who was
    /// bounded by nothing at all.
    ///
    /// # A container that is absent and one that is out of reach refuse alike
    ///
    /// Deliberately, and it is the whole point: telling them apart is the oracle
    /// this closes, so a refusal that said *no such namespace* for one and
    /// *not yours* for the other would leave it open with extra steps. The cost
    /// is that a typo now reads as a permission refusal, which is the same trade
    /// [`Error::OutsideTenancy`] already makes for a table.
    fn holds_something_named(
        &self,
        user: &tessari_storage::UserDefinition,
        namespace: Option<&tessari_ql::Name>,
        database: Option<&tessari_ql::Name>,
        span: tessari_ql::Span,
    ) -> Result<()> {
        let selected = self.namespace().map(ToOwned::to_owned);
        let Some(within) = namespace.map(|named| named.text.clone()).or(selected) else {
            // `USE DATABASE` with no namespace selected names no container at
            // all, and fails on its own terms in the statement after it. There
            // is nothing here to hold an authority over.
            return Ok(());
        };
        let mut transaction = self.store.begin()?;
        let catalog = Catalog::new(&mut transaction);
        let found = catalog.namespace_id(&within).ok().flatten();
        let reach = match (found, database) {
            (None, _) => None,
            (Some(id), None) => Some(tessari_storage::Reach::Namespace(id)),
            (Some(id), Some(named)) => catalog
                .database_id(id, &named.text)
                .ok()
                .flatten()
                .map(|inner| tessari_storage::Reach::Database(id, inner)),
        };
        transaction.rollback();
        if reach.is_some_and(|reach| user.authorities.touches(reach)) {
            return Ok(());
        }
        Err(Error::OutsideTenancy {
            // The deepest name written, because that is the one the author is
            // looking at.
            name: database
                .or(namespace)
                .map_or(within, |named| named.text.clone()),
            span,
        })
    }

    /// Whether this name is one the user's own reach covers.
    ///
    /// # A namespace contains its databases, and this used to forget that
    ///
    /// The first two cases are the user's own namespace and their own database,
    /// and they were the whole check while a user's reach was always a namespace
    /// *and* a database — a namespace-scoped user was not a thing that could be
    /// declared. Now one can be, and without the third case they could select
    /// their own namespace and then no database inside it, which makes the
    /// authority the model exists to express unusable by the person holding it.
    ///
    /// The third case asks the reach rather than comparing another name: any
    /// database that resolves inside the namespace the user holds is theirs,
    /// because holding a namespace is holding what it contains. It fires only
    /// when the user has no database of their own, so a database-scoped user is
    /// still confined to exactly one.
    fn names_own_tenancy(&self, user: &tessari_storage::UserDefinition, named: &str) -> bool {
        let Ok(mut transaction) = self.store.begin() else {
            return false;
        };
        let catalog = Catalog::new(&mut transaction);
        let named_namespace = user.namespace.is_some_and(|id| {
            catalog
                .namespace(id)
                .ok()
                .flatten()
                .is_some_and(|found| found.name == named)
        });
        let named_database = user.database.is_some_and(|id| {
            catalog
                .database(id)
                .ok()
                .flatten()
                .is_some_and(|found| found.name == named)
        });
        let inside_own_namespace = user.database.is_none()
            && user.namespace.is_some_and(|id| {
                catalog
                    .database_id(id, named)
                    .is_ok_and(|found| found.is_some())
            });
        transaction.rollback();
        named_namespace || named_database || inside_own_namespace
    }
}
