//! Releasing a held space a drop removed (G079 W5c).
//!
//! `DROP SPACE` deletes the definition in the dropping transaction, and the
//! space's map can be forgotten only after that commit lands and no reader can
//! still see the space: a snapshot older than the drop reads the space as it
//! stood, so its keys stay until the oldest live snapshot is past the drop. The
//! catalog call names the space here as a HINT — the transaction may yet be
//! cancelled — and the release reads the committed catalog before it forgets
//! anything.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tessari_types::{DatabaseId, NamespaceId, TableId};

use super::{HeldSpaces, Place, place_of};
use crate::catalog::{Catalog, SpaceDeclaration, TableKind};
use crate::error::Result;
use crate::store::Store;

impl HeldSpaces {
    /// Name a held table a drop is removing, to be released once the drop has
    /// landed and nothing can read it.
    pub(crate) fn dropping(&self, namespace: NamespaceId, database: DatabaseId, table: TableId) {
        let place = place_of(namespace, database, table);
        if self.tables.contains_key(&place) {
            self.dropping.entry(place).or_insert(None);
            // Release: pairs with the AcqRel swap in `release_dropped_spaces`.
            self.any_dropping.store(true, Ordering::Release);
        }
    }
}

impl Store {
    /// Forget every held space whose drop has landed and is older than every
    /// live reader.
    ///
    /// Called after a commit lands while a drop is named, and by the expiry
    /// removal pass for a release a reader held back.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog or the committed version cannot be
    /// read; nothing is forgotten then.
    pub(crate) async fn release_dropped_spaces(&self) -> Result<()> {
        let held = self.held_spaces();
        // Taken down first, so a drop named while this runs raises it again.
        if !held.any_dropping.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let named: Vec<Place> = held.dropping.iter().map(|entry| *entry.key()).collect();
        let mut pending = false;
        for place in named {
            let (namespace, database, table) = ids_of(&place);
            if self.holds(namespace, database, table).await? {
                // The drop was cancelled.
                held.dropping.remove(&place);
                continue;
            }
            let landed = self.committed_version().await?;
            let gone_at = held
                .dropping
                .entry(place)
                .or_insert(None)
                .get_or_insert(landed)
                .to_owned();
            if self.retention_floor().await? >= gone_at {
                held.tables.remove(&place);
                held.dropping.remove(&place);
            } else {
                pending = true;
            }
        }
        if pending {
            held.any_dropping.store(true, Ordering::Release);
        }
        Ok(())
    }

    /// Whether the committed catalog still declares `table` a space held in
    /// memory.
    async fn holds(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
    ) -> Result<bool> {
        let mut reading = self.begin().await?;
        let definition: Option<Arc<_>> = Catalog::new(&mut reading).table_shared(table).await?;
        Ok(definition.is_some_and(|definition| {
            definition.namespace == namespace
                && definition.database == database
                && matches!(
                    definition.kind,
                    TableKind::Space(SpaceDeclaration { memory: true, .. })
                )
        }))
    }
}

/// The identifiers a place spells.
fn ids_of(place: &Place) -> (NamespaceId, DatabaseId, TableId) {
    let [n0, n1, n2, n3, d0, d1, d2, d3, t0, t1, t2, t3] = *place;
    (
        NamespaceId::new(u32::from_be_bytes([n0, n1, n2, n3])),
        DatabaseId::new(u32::from_be_bytes([d0, d1, d2, d3])),
        TableId::new(u32::from_be_bytes([t0, t1, t2, t3])),
    )
}
