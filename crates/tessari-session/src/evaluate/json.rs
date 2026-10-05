//! `json::encode`, which needs the catalog for one thing: a table's name.

use std::collections::BTreeSet;

use tessari_storage::{Catalog, Transaction};
use tessari_types::Value;
use tessari_types::json::{Names, referenced_tables, write};

use crate::error::Result;
use crate::session::Session;

impl Session<'_> {
    /// The argument as compact JSON text, its record references written by
    /// their tables' names (ADR-0116 D2).
    ///
    /// An absent or null argument answers `none`, the rule every call follows.
    /// The catalog is read only when the value holds a reference at all.
    pub(super) fn json_encode(
        &self,
        transaction: &mut Transaction<'_>,
        arguments: &[Value],
    ) -> Result<Value> {
        let held = arguments.first().unwrap_or(&Value::None);
        if !held.is_present() || *held == Value::Null {
            return Ok(Value::None);
        }
        let mut wanted = BTreeSet::new();
        referenced_tables(held, &mut wanted);
        let mut names = Names::new();
        if !wanted.is_empty() {
            let catalog = Catalog::new(transaction);
            for table in wanted {
                // A dropped table keeps its id and gains no name, as on HTTP.
                if let Some(defined) = catalog.table(table)? {
                    names.insert(table, defined.name);
                }
            }
        }
        let mut out = String::new();
        write(&mut out, held, &names);
        Ok(Value::from(out.as_str()))
    }
}
