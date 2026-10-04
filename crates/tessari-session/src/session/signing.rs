use super::*;

impl<'a> Session<'a> {
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
            .unwrap_or_else(|| self.store.attempts().permit(name));
        if !permitted {
            tracing::warn!(user = %name, "sign-in refused: too many recent failures");
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
            tracing::warn!(user = %name, "sign-in refused: already verifying as many as this node will");
            return Err(Error::SignInThrottled);
        };

        let Some(user) = found else {
            // The hash is still computed for a name that does not exist, so the
            // time a refusal takes does not say whether the name did.
            let _ = identity::verifies(password, ABSENT_USER_HASH);
            // The name is reported and the reason is not, for the same reason
            // the caller is told neither: a log an operator reads is also a log
            // an attacker reads once they are inside.
            tracing::warn!(user = %name, "sign-in refused");
            // Counted against the name that was tried, not against the user that
            // was not found. Counting only known names would let an attacker
            // enumerate the catalog by watching which names start to wait.
            self.missed(name);
            return Err(Error::SignInRefused);
        };
        if !identity::verifies(password, &user.secret) {
            tracing::warn!(user = %name, "sign-in refused");
            self.missed(name);
            return Err(Error::SignInRefused);
        }
        tracing::info!(user = %name, "signed in");
        self.store.attempts().succeeded(name);
        if let Some(budget) = &self.budget {
            budget.succeeded(name);
        }
        self.identity = Identity::Signed(Box::new(user));
        Ok(())
    }

    /// Count a missed try as `name` here and, in a cluster, in the shared table.
    fn missed(&self, name: &str) {
        self.store.attempts().failed(name);
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
            participants: self.participants.clone(),
            // A probe answers who may do what and never writes a file.
            backups: None,
            at_rest: None,
            budget: self.budget.clone(),
            // A probe answers who may do what, and reports no certificates.
            certificates: None,
            sink: crate::backup_to::Sink::none(),
            landed: false,
            acknowledge_open: None,
            across_open: false,
            event_depth: 0,
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
            tracing::warn!(user = %user.name, "a password change was refused");
            return Err(Error::CurrentPasswordRefused);
        }

        user.secret = secret;
        let mut transaction = self.store.begin()?;
        Catalog::new(&mut transaction).update_user(&user);
        transaction.commit()?;
        tracing::info!(user = %user.name, "a user changed their own password");
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
