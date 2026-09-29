//! Whether a statement stays inside the tenancy its caller is confined to.

use crate::error::{Error, Result};
use crate::session::Session;
use tessari_ql::StatementKind;
use tessari_storage::Catalog;

impl<'a> Session<'a> {
    /// Refuse a statement reaching outside the tenancy its user belongs to.
    ///
    /// This one catches `USE` specifically, which resolves no tenancy of its own
    /// — it only records a name for the statements after it. Without it a scoped
    /// user's `USE NAMESPACE other` would succeed and the refusal would arrive
    /// one statement later, naming something the author did not just write.
    pub(crate) fn within_tenancy(
        &self,
        kind: &StatementKind,
        span: tessari_ql::Span,
    ) -> Result<()> {
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
    pub(crate) fn holds_something_named(
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
    pub(crate) fn names_own_tenancy(
        &self,
        user: &tessari_storage::UserDefinition,
        named: &str,
    ) -> bool {
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
