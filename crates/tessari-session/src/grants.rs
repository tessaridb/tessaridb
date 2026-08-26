//! Running `GRANT` and `REVOKE`.
//!
//! The rule they express is one sentence: **a user's grants, if they have any,
//! are the whole story, and a user with none is governed by their role.** What
//! enforces it is `authorize.rs`; what is here is only the two statements that
//! change it.
//!
//! # Both statements say what the result is, not what they add
//!
//! `GRANT read ON users TO ada` leaves ada with `read` on `users` whatever she
//! had before, because an operator reading a provisioning script should not have
//! to know the history to know the outcome. That is also what makes it
//! idempotent, since a grant is identified by the pair it names.

use tessari_ql::{Name, Span, TableRef};
use tessari_storage::{Catalog, Transaction, UserDefinition, Verb};

use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

impl Session<'_> {
    /// Give a user verbs on a table.
    ///
    /// # The first grant is also a restriction
    ///
    /// A user with no grants is governed by their role and reaches every table
    /// in their tenancy. The moment they have one, their grants are the whole
    /// story. That is stated rather than discovered, and it is what makes the
    /// feature able to narrow — a role can only widen.
    pub(crate) fn grant(
        &self,
        transaction: &mut Transaction<'_>,
        verbs: &[Name],
        table: &TableRef,
        fields: &[Name],
        user: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let (asked, held) = self.grant_operands(transaction, verbs, table, user, span)?;
        // A user who cannot see a field but may write the record would overwrite
        // it whole and destroy what they cannot see — a data-loss hole created by
        // the permission system rather than closed by it.
        if !fields.is_empty() && asked.0.contains(&Verb::Write) {
            return Err(Error::FieldsOnAWrite { span });
        }
        let named: Vec<String> = fields.iter().map(|name| name.text.clone()).collect();
        Catalog::new(transaction).grant(held.id, asked.1, &asked.0, &named)?;
        Ok(Outcome::Done)
    }

    /// Take verbs away, and refuse to take away the last grant.
    pub(crate) fn revoke(
        &self,
        transaction: &mut Transaction<'_>,
        verbs: &[Name],
        table: &TableRef,
        user: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let (asked, held) = self.grant_operands(transaction, verbs, table, user, span)?;
        let existing = Catalog::new(transaction).grants_for(held.id)?;
        let remaining: Vec<Verb> = existing
            .iter()
            .find(|grant| grant.table == asked.1)
            .map(|grant| {
                grant
                    .verbs
                    .iter()
                    .copied()
                    .filter(|verb| !asked.0.contains(verb))
                    .collect()
            })
            .unwrap_or_default();

        // Removing the last grant would widen this user from a named table to
        // every table their role allows. That is the one direction a revocation
        // must never go silently.
        if remaining.is_empty() && existing.len() <= 1 {
            return Err(Error::LastGrant {
                user: held.name,
                span,
            });
        }
        let mut catalog = Catalog::new(transaction);
        if remaining.is_empty() {
            catalog.revoke(held.id, asked.1);
        } else {
            // The field list travels with the grant it belongs to: revoking a
            // verb narrows what may be done, never what may be seen.
            let kept = existing
                .iter()
                .find(|grant| grant.table == asked.1)
                .map(|grant| grant.fields.clone())
                .unwrap_or_default();
            catalog.grant(held.id, asked.1, &remaining, &kept)?;
        }
        Ok(Outcome::Done)
    }

    /// The verbs, the table and the user a grant statement names.
    ///
    /// Shared because `GRANT` and `REVOKE` resolve exactly the same three things
    /// and differ only in what they then do with them.
    fn grant_operands(
        &self,
        transaction: &mut Transaction<'_>,
        verbs: &[Name],
        table: &TableRef,
        user: &Name,
        span: Span,
    ) -> Result<((Vec<Verb>, tessari_types::TableId), UserDefinition)> {
        let mut asked = Vec::new();
        for name in verbs {
            let Some(verb) = Verb::parse(&name.text) else {
                return Err(Error::NoSuchVerb {
                    name: name.text.clone(),
                    span: name.span,
                });
            };
            asked.push(verb);
        }
        // Resolved through the same path every other statement uses, so a grant
        // cannot name a table outside the granting session's own tenancy.
        let (_, id) = self.resolve_table(transaction, table)?;
        let held = Catalog::new(transaction)
            .users()?
            .into_iter()
            .find(|held| held.name == user.text)
            .ok_or_else(|| Error::Unknown {
                entity: "user",
                name: user.text.clone(),
                span,
            })?;
        // The table is already bounded — `resolve_table` cannot reach outside
        // the granting session's tenancy — but the **user** was not, and a grant
        // is a change to the user rather than to the table. It NARROWS them: the
        // store's own rule is that a user with even one grant is reduced to
        // exactly what they were granted. So an owner of one database could aim
        // a grant at the store's owner and take away everything else they had.
        // Measured before it was closed: after one `GRANT read ON t TO root`,
        // `BACKUP` answered "root holds grants, and a backup is every table at
        // once" — an owner of a part had disabled the whole store's only
        // recovery path, from below, with a statement that reads as generosity.
        if !self.administers(&held) {
            return Err(Error::NotYours {
                user: held.name.clone(),
                span,
            });
        }
        Ok(((asked, id), held))
    }
}
