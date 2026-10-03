//! Folding a placement given back to the store line away (ADR-0098 D3).
//!
//! `ALTER REPLICA b LEADS NONE` on the last row placing a range marks the row
//! releasing and keeps the range carved, so nobody else writes it. The store
//! line's leader then campaigns for the range on its own line, and the
//! election's rules carry the safety: a new epoch only once the old lease has
//! run out, and never to a candidate behind a voter's copy of the line. Once
//! this node leads the range as well as the store line, the placement is
//! folded away here — the range returns to the store line, which the same
//! node leads, so no instant has two writers.

use tessari_storage::Catalog;
use tessari_types::Reach;

use crate::{Db, Result};

impl Db {
    /// Fold away every placement being given back that this node may fold:
    /// the range it now leads as the store line's leader, or one another row
    /// has placed since — the release is then an ordinary move. Answers the
    /// rows folded.
    ///
    /// # Errors
    ///
    /// The store's failure to read the catalog or commit.
    pub fn hand_back_ranges(&self) -> Result<Vec<String>> {
        let store = self.store();
        if !store.leads(Reach::Store)? {
            return Ok(Vec::new());
        }
        let mut reading = store.begin()?;
        let rows = Catalog::new(&mut reading).replicas()?;
        reading.rollback();
        let mut folded = Vec::new();
        for row in rows.iter().filter(|row| row.releasing) {
            let Some(range) = row.leads else {
                continue;
            };
            let placed_again = rows
                .iter()
                .any(|other| !other.releasing && other.leads == Some(range));
            if !placed_again && !store.leads(range)? {
                continue;
            }
            let mut writing = store.begin()?;
            if Catalog::new(&mut writing).finish_release(&row.name)? {
                writing.commit()?;
                folded.push(row.name.clone());
            } else {
                writing.rollback();
            }
        }
        Ok(folded)
    }
}
