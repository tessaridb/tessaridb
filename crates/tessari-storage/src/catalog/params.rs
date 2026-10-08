//! A database's named values — `DEFINE PARAM` (ADR-0124 D2).
//!
//! They sit on the database's own catalog record, so they replicate, snapshot
//! and restore with it and need no record kind of their own.

use std::collections::BTreeMap;

use tessari_types::{DatabaseId, Value};

use super::{Catalog, system};
use crate::error::Result;

impl Catalog<'_, '_> {
    /// Replace a database's params, keeping everything else about it.
    ///
    /// Answers `false` when there is no database under that id.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn set_params(&mut self, id: DatabaseId, params: BTreeMap<String, Value>) -> Result<bool> {
        let Some(mut definition) = self.database(id)? else {
            return Ok(false);
        };
        definition.params = params;
        self.write(system::DATABASES, id.get(), &definition.to_value());
        Ok(true)
    }
}
