//! `INFO FOR POOL[S]` — the namespace's workload pools: the declaration from
//! the catalog, the counts from this process's gate tally.
//!
//! The two halves answer different questions and the row says which is which:
//! the declaration follows a backup the way every catalog row does, and the
//! counts are process-local for [`crate::info`]'s reason — the tally a live
//! gate keeps is a fact about this process, and reporting it as the
//! cluster's would be a claim the store cannot check.

use std::collections::BTreeMap;

use tessari_ql::{Name, Span};
use tessari_storage::{Catalog, Kind, PoolDefinition, Transaction};
use tessari_types::{Reach, Value};

use crate::error::{Error, Result};
use crate::info::named_database;
use crate::outcome::Outcome;
use crate::session::Session;

impl Session<'_> {
    /// `INFO FOR POOL web` — one pool's declaration with this node's counts.
    pub(super) async fn info_pool(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let namespace = self.namespace_id(transaction, span).await?;
        self.permits_pool_reading(namespace, span)?;
        let catalog = Catalog::new(transaction);
        let pools = catalog.pools(namespace).await?;
        let Some(definition) = pools.iter().find(|held| held.name == name.text) else {
            return Err(Error::Unknown {
                entity: "pool",
                name: name.text.clone(),
                span,
            });
        };
        self.described_pool_fields(&catalog, definition).await
    }

    /// `INFO FOR POOLS` — every pool the selected namespace holds, by name.
    pub(super) async fn info_pools(
        &self,
        transaction: &mut Transaction<'_>,
        span: Span,
    ) -> Result<Outcome> {
        let namespace = self.namespace_id(transaction, span).await?;
        self.permits_pool_reading(namespace, span)?;
        let catalog = Catalog::new(transaction);
        let mut pools = catalog.pools(namespace).await?;
        // Listed in name order, for the reason `by_name` sorts: a report
        // whose shape depends on declaration order is two answers to one
        // question.
        pools.sort_by(|a, b| a.name.cmp(&b.name));
        let mut rows = Vec::with_capacity(pools.len());
        for definition in &pools {
            rows.push(Value::Object(
                self.described_pool_fields(&catalog, definition).await?,
            ));
        }
        Ok(Outcome::Value(Value::Array(rows)))
    }

    /// The one permission rule this subject carries: the rows describe who
    /// may run what, so they are answered to the caller who may manage the
    /// namespace — [`Kind::Manage`] at the namespace's reach, the authority
    /// declaring a pool already needs.
    fn permits_pool_reading(
        &self,
        namespace: tessari_types::NamespaceId,
        span: Span,
    ) -> Result<()> {
        if let Some(user) = self.signed_in()
            && !user
                .authorities
                .permits(Kind::Manage, Reach::Namespace(namespace))
        {
            return Err(Error::RoleForbids {
                role: user.role.map_or("authorities", tessari_storage::Role::name),
                needs: Kind::Manage.name(),
                span,
            });
        }
        Ok(())
    }

    /// One pool as the report states it: the declaration beside the row, the
    /// counts this process's gate holds, the database resolved back to the
    /// name the operator wrote.
    async fn described_pool_fields(
        &self,
        catalog: &Catalog<'_, '_>,
        definition: &PoolDefinition,
    ) -> Result<BTreeMap<String, Value>> {
        let namespace = self.namespace().map_or_else(String::new, str::to_owned);
        let reading = self
            .store
            .pool_counters()
            .into_iter()
            .find(|held| held.namespace == namespace && held.pool == definition.name);
        let (running, refused) = reading.map_or((0, 0), |held| (held.running, held.refused));
        let pool = &definition.pool;
        let as_count = |count: u64| {
            Value::Number(tessari_types::Number::Integer(
                i64::try_from(count).unwrap_or(i64::MAX),
            ))
        };
        Ok(BTreeMap::from([
            ("name".to_owned(), Value::from(definition.name.as_str())),
            ("max_concurrent".to_owned(), as_count(pool.max_concurrent())),
            (
                "database".to_owned(),
                match pool.database() {
                    None => Value::Null,
                    Some(database) => Value::from(named_database(catalog, database).await?),
                },
            ),
            (
                "user".to_owned(),
                pool.user().map_or(Value::Null, Value::from),
            ),
            ("reads".to_owned(), Value::Bool(pool.kinds().reads)),
            ("writes".to_owned(), Value::Bool(pool.kinds().writes)),
            (
                "definitions".to_owned(),
                Value::Bool(pool.kinds().definitions),
            ),
            ("running".to_owned(), as_count(running)),
            ("refused".to_owned(), as_count(refused)),
        ]))
    }
}
