//! The params of the database each statement runs in, as the values a name
//! falls back to when neither the caller nor a `LET` binds it (ADR-0124 D2).

use std::collections::BTreeMap;

use tessari_ql::{Parameters, Script, StatementKind};
use tessari_storage::Catalog;

use super::Session;
use crate::error::Result;

impl Session<'_> {
    /// Bind `script` as [`Script::bind`] does, falling back to database params.
    ///
    /// The catalog is read only when the caller's map leaves a name unbound, so
    /// a script that names no param pays nothing for the feature.
    /// Bind a parsed script to its parameters. The second answer says
    /// whether the bind asked the store for database defaults — the caller
    /// uses it to decide whether the compiled form may be cached (a bound
    /// default baked the store's answer into the tree, and a redefined
    /// default must not be answered by the old one).
    pub(crate) async fn bind_script(
        &self,
        script: Script,
        parameters: &Parameters,
    ) -> Result<(Script, bool)> {
        match script.clone().bind(parameters) {
            Err(tessari_ql::Error::UnboundParameter { .. }) => {
                let defaults = self.database_params(&script).await?;
                Ok((script.bind_with_defaults(parameters, &defaults)?, false))
            }
            bound => Ok((bound?, true)),
        }
    }

    /// One map per statement: the params of the database it will run in, as
    /// the `USE` statements above it select it.
    async fn database_params(&self, script: &Script) -> Result<Vec<Parameters>> {
        let mut namespace = self.namespace.clone();
        let mut database = self.database.clone();
        let mut held: BTreeMap<(String, String), Parameters> = BTreeMap::new();
        let mut transaction = self.store.begin().await?;
        let mut defaults = Vec::with_capacity(script.statements.len());
        for statement in &script.statements {
            if let StatementKind::Use {
                namespace: named_namespace,
                database: named_database,
                ..
            } = &statement.kind
            {
                if let Some(name) = named_namespace {
                    namespace = Some(name.text.clone());
                }
                if let Some(name) = named_database {
                    database = Some(name.text.clone());
                }
            }
            let (Some(namespace), Some(database)) = (&namespace, &database) else {
                defaults.push(Parameters::new());
                continue;
            };
            let key = (namespace.clone(), database.clone());
            if !held.contains_key(&key) {
                let catalog = Catalog::new(&mut transaction);
                let params = match catalog.namespace_id(namespace).await? {
                    Some(namespace) => match catalog.database_id(namespace, database).await? {
                        Some(id) => catalog.database(id).await?.map(|found| found.params),
                        None => None,
                    },
                    None => None,
                };
                held.insert(key.clone(), params.unwrap_or_default());
            }
            defaults.push(held.get(&key).cloned().unwrap_or_default());
        }
        transaction.rollback();
        Ok(defaults)
    }
}
