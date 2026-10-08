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
    pub(crate) fn bind_script(&self, script: Script, parameters: &Parameters) -> Result<Script> {
        match script.clone().bind(parameters) {
            Err(tessari_ql::Error::UnboundParameter { .. }) => {
                let defaults = self.database_params(&script)?;
                Ok(script.bind_with_defaults(parameters, &defaults)?)
            }
            bound => Ok(bound?),
        }
    }

    /// One map per statement: the params of the database it will run in, as
    /// the `USE` statements above it select it.
    fn database_params(&self, script: &Script) -> Result<Vec<Parameters>> {
        let mut namespace = self.namespace.clone();
        let mut database = self.database.clone();
        let mut held: BTreeMap<(String, String), Parameters> = BTreeMap::new();
        let mut transaction = self.store.begin()?;
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
                let params = match catalog.namespace_id(namespace)? {
                    Some(namespace) => match catalog.database_id(namespace, database)? {
                        Some(id) => catalog.database(id)?.map(|found| found.params),
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
