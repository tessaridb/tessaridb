//! INFO for users and what they may reach.

use std::collections::BTreeMap;

use tessari_ql::{Name, Span, TableRef};
use tessari_storage::{Catalog, Transaction};
use tessari_types::Value;

use crate::error::{Error, Result};
use crate::session::Session;

use super::{described_authorities, described_grant, described_user, reading, selecting, writing};

impl Session<'_> {
    /// One user's role, tenancy and grants.
    ///
    /// Needs `Administer`, decided by `Needs::of` before this runs, so every
    /// caller reaching here is an owner. Nothing is filtered: an owner asking
    /// what somebody may do gets the answer or the refusal, because a grant list
    /// with rows quietly removed would be read as the whole of what that user
    /// can reach.
    ///
    /// **The password hash is not in the report.** The stored definition carries
    /// it — `UserDefinition::to_value` writes it, because that value is what the
    /// catalog holds — so the report is built field by field rather than from
    /// that value. Reusing it would put every hash in the store onto the wire
    /// and into whatever logs the answer.
    pub(super) fn info_user(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let catalog = Catalog::new(transaction);
        let Some(user) = catalog
            .users()?
            .into_iter()
            .find(|found| found.name == name.text)
        else {
            return Err(Error::Unknown {
                entity: "user",
                name: name.text.clone(),
                span,
            });
        };
        // The same boundary the listing draws, drawn again here. A caller who
        // cannot be *shown* somebody in a list has no business reading their
        // role, their tenancy and every grant they hold by naming them instead
        // — and the singular form is the one an operator reaches for when they
        // already have a name to try.
        if !self.administers(&user) {
            return Err(Error::NotYours {
                user: user.name.clone(),
                span,
            });
        }
        let grants = catalog.grants_for(user.id)?;
        let mut described = Vec::new();
        for grant in &grants {
            described.push(described_grant(&catalog, grant)?);
        }
        let mut report = described_user(&user);
        if let Some(id) = user.namespace
            && let Some(found) = catalog.namespace(id)?
        {
            report.insert("namespace".to_owned(), Value::from(found.name.as_str()));
        }
        if let Some(id) = user.database
            && let Some(found) = catalog.database(id)?
        {
            report.insert("database".to_owned(), Value::from(found.name.as_str()));
        }
        report.insert("grants".to_owned(), Value::Array(described));
        report.insert(
            "authorities".to_owned(),
            described_authorities(&catalog, &user)?,
        );
        Ok(report)
    }

    /// Every user of the tenancy this caller administers.
    ///
    /// Needs `Administer`, decided by `Needs::of` before this runs, which is why
    /// there is no permission check in the body. What the body does instead is
    /// bound the answer to the caller's **own** tenancy: passing the check says
    /// somebody administers something, and it does not say they administer the
    /// whole store.
    ///
    /// The three cases are the three tenancies a user can hold, and the rule is
    /// containment rather than equality — a store owner sees everyone, a
    /// namespace owner sees that namespace, a database owner sees that database.
    /// A user of a *different* tenancy at the same depth is not visible to
    /// either, which is the case that would otherwise leak quietly.
    ///
    /// Grants are deliberately absent: they are per-user detail, one catalog read
    /// each, and `INFO FOR USER <name>` is where a single subject is examined.
    pub(super) fn info_users(
        &self,
        transaction: &mut Transaction<'_>,
    ) -> Result<BTreeMap<String, Value>> {
        let catalog = Catalog::new(transaction);
        let mut listed = Vec::new();
        for user in catalog.users()? {
            if !self.administers(&user) {
                continue;
            }
            let mut described = described_user(&user);
            if let Some(id) = user.namespace
                && let Some(found) = catalog.namespace(id)?
            {
                described.insert("namespace".to_owned(), Value::from(found.name.as_str()));
            }
            if let Some(id) = user.database
                && let Some(found) = catalog.database(id)?
            {
                described.insert("database".to_owned(), Value::from(found.name.as_str()));
            }
            listed.push(Value::Object(described));
        }
        Ok(BTreeMap::from([("users".to_owned(), Value::Array(listed))]))
    }

    /// Who can reach one table, answered by asking rather than by deriving.
    ///
    /// # Why this does not read a grant
    ///
    /// Because a report that read grants and authorities and worked out what
    /// they add up to would be a **second evaluator**, and the store already has
    /// one — `Session::authorize`, the function every statement passes through.
    /// Two evaluators of one rule agree until they do not, and the moment they
    /// stop is invisible: nothing fails, the report simply becomes fiction, and
    /// the person reading it is by definition somebody auditing a system they
    /// cannot otherwise see into. So each answer here is obtained by signing a
    /// throwaway session in as that user and putting a real `SELECT` and a real
    /// `DELETE` to the real check.
    ///
    /// That also means every rule holds without being restated: the tenancy
    /// gate, the held set, the grant loop and the open-store rule all apply
    /// because they are the same code. A user declared in another namespace
    /// reports `false` for the reason they would be refused, not because this
    /// function remembered to exclude them.
    ///
    /// # Everybody administered is listed, including the ones who cannot
    ///
    /// A row saying `bob` reaches nothing looks like noise until you notice that
    /// leaving it out makes two different facts look identical — *bob cannot
    /// reach this* and *the caller cannot see bob*. An audit answer has to
    /// distinguish those, and only the caller's own tenancy boundary decides who
    /// appears, exactly as it does for `INFO FOR USERS`.
    ///
    /// The probes never run. `authorize` decides from the statement's shape, so
    /// the record id below names nothing that has to exist.
    pub(super) fn info_access(
        &self,
        transaction: &mut Transaction<'_>,
        table: &TableRef,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        // Resolved first, so a report is never produced for a table that is not
        // there: an empty access list is the same shape as a typo.
        let (_, id) = self.resolve_table(transaction, table)?;
        let catalog = Catalog::new(transaction);
        let Some(definition) = catalog.table(id)? else {
            return Err(Error::Unknown {
                entity: "table",
                name: table.name.text.clone(),
                span: table.span,
            });
        };
        let users = catalog.users()?;
        // Three statements and not two. Reaching a table starts with selecting
        // the container it is in, and that first step is where a declared
        // tenancy is enforced — `within_tenancy` looks at `USE` and at nothing
        // else, because a selection is a name until something resolves it. A
        // probe handed the asker's selection outright would therefore skip the
        // only gate confining a tenant, and report that somebody from another
        // namespace reads this table. The first draft of this function did
        // exactly that, and the cross-product test caught it.
        let selecting = selecting(
            self.namespace(),
            table
                .database
                .as_ref()
                .map_or_else(|| self.database(), |name| Some(name.text.as_str())),
            table.span,
        );
        let reading = reading(table);
        let writing = writing(table);
        let mut listed = Vec::new();
        for user in users {
            if !self.administers(&user) {
                continue;
            }
            let mut probe = self.probing(user.id)?;
            let arrived = probe.authorize(self.store, &selecting, span).is_ok();
            let read = arrived && probe.authorize(self.store, &reading, span).is_ok();
            let write = arrived && probe.authorize(self.store, &writing, span).is_ok();
            listed.push(Value::Object(BTreeMap::from([
                ("user".to_owned(), Value::from(user.name.as_str())),
                ("read".to_owned(), Value::Bool(read)),
                ("write".to_owned(), Value::Bool(write)),
            ])));
        }
        Ok(BTreeMap::from([
            ("table".to_owned(), Value::from(definition.name.as_str())),
            ("access".to_owned(), Value::Array(listed)),
        ]))
    }
}
