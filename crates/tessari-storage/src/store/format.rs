//! The format a store holds, which lags the format this build can write
//! (ADR-0118).

use std::sync::Arc;

use tessari_encoding::{FormatVersion, FormatVersionKey, StoreKey, StoreValue};
use tessari_kv::WriteBatch;

use crate::error::Result;

use super::{Store, read_format_version};

impl Store {
    /// The format this store holds: the newest one any value in it may use.
    ///
    /// It is not [`FormatVersion::CURRENT`], which is only what this build can
    /// write. A store an older build wrote keeps the older stamp until it is
    /// finalized, so that build can still open it; a statement that would write
    /// a newer value asks this first and is refused below it.
    ///
    /// # Errors
    ///
    /// Returns the substrate's failure, or a decoding failure for a stamp that
    /// does not read as a version.
    pub fn held_format(&self) -> Result<FormatVersion> {
        // Every store is stamped at open before anything else is written to
        // it, so an absent stamp is a store this process is creating: it holds
        // what this build writes.
        Ok(read_format_version(Arc::clone(self.backend()))?.unwrap_or(FormatVersion::CURRENT))
    }

    /// Raise the stamp to the format the catalog says this store was finalized
    /// to, when an older build applied that finalize and so never moved it.
    ///
    /// # Errors
    ///
    /// Returns the substrate's failure, or a decoding failure for the stamp or
    /// the record.
    pub(super) fn catch_up_with_the_finalized_format(&self) -> Result<()> {
        let mut transaction = self.begin()?;
        let finalized = crate::catalog::Catalog::new(&mut transaction).finalized_format()?;
        drop(transaction);
        if let Some(raised) = crate::format_stamp::caught_up(self.held_format()?, finalized) {
            self.backend().apply(WriteBatch::new().put(
                FormatVersionKey::keyspace(),
                FormatVersionKey.encode(),
                raised.encode(),
            ))?;
        }
        Ok(())
    }
}
