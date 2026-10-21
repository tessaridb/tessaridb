//! The router as the store's backend: each call to the side its key or its
//! range's start names (ADR-0130).

use std::sync::Arc;

use tessari_encoding::{StoreKey, VersionPositionKey};
use tessari_kv::{
    GroupFuture, Key, KeyRange, Keyspace, KvBackend, KvFuture, ScanRequest, Value, WriteBatch,
};

use super::{Routed, Side};

impl KvBackend for Routed {
    fn name(&self) -> &'static str {
        self.disk.name()
    }

    fn get<'a>(&'a self, keyspace: Keyspace, key: &'a Key) -> KvFuture<'a, Option<Value>> {
        Box::pin(async move {
            if *key == VersionPositionKey.encode() {
                return self.version().await;
            }
            self.side(key.as_slice()).backend().get(keyspace, key).await
        })
    }

    fn scan<'a>(&'a self, request: &'a ScanRequest) -> KvFuture<'a, Vec<(Key, Value)>> {
        Box::pin(async move { self.side_of(&request.range).backend().scan(request).await })
    }

    fn sweep<'a>(&'a self, request: &'a ScanRequest) -> KvFuture<'a, Vec<(Key, Value)>> {
        Box::pin(async move { self.side_of(&request.range).backend().sweep(request).await })
    }

    fn first_of_each<'a>(
        &'a self,
        keyspace: Keyspace,
        ranges: &'a [KeyRange],
    ) -> KvFuture<'a, Vec<Option<(Key, Value)>>> {
        // Whole to the store's backend when no range is held, so a backend that
        // answers many ranges in one ask still does.
        let sides: Vec<Side<'_>> = ranges.iter().map(|range| self.side_of(range)).collect();
        if sides.iter().all(|side| matches!(side, Side::Disk(_))) {
            return self.disk.first_of_each(keyspace, ranges);
        }
        Box::pin(async move {
            let mut found = Vec::with_capacity(ranges.len());
            for (range, side) in ranges.iter().zip(&sides) {
                found.push(
                    side.backend()
                        .first_of_each(keyspace, std::slice::from_ref(range))
                        .await?
                        .pop()
                        .flatten(),
                );
            }
            Ok(found)
        })
    }

    fn count<'a>(&'a self, keyspace: Keyspace, range: &'a KeyRange) -> KvFuture<'a, u64> {
        Box::pin(async move { self.side_of(range).backend().count(keyspace, range).await })
    }

    fn contains<'a>(&'a self, keyspace: Keyspace, key: &'a Key) -> KvFuture<'a, bool> {
        Box::pin(async move {
            if *key == VersionPositionKey.encode() {
                return Ok(self.version().await?.is_some());
            }
            self.side(key.as_slice())
                .backend()
                .contains(keyspace, key)
                .await
        })
    }

    fn delete_range<'a>(&'a self, keyspace: Keyspace, range: &'a KeyRange) -> KvFuture<'a, ()> {
        Box::pin(async move {
            self.side_of(range)
                .backend()
                .delete_range(keyspace, range)
                .await
        })
    }

    fn apply(&self, batch: WriteBatch) -> KvFuture<'_, ()> {
        Box::pin(async move {
            let Some(mut parts) = self.one(batch).await? else {
                return Ok(());
            };
            if !parts.disk.is_empty() {
                self.disk.apply(std::mem::take(&mut parts.disk)).await?;
            }
            self.land_held(parts).await
        })
    }

    fn apply_unsynced(&self, batch: WriteBatch) -> KvFuture<'_, ()> {
        Box::pin(async move {
            let Some(mut parts) = self.one(batch).await? else {
                return Ok(());
            };
            if !parts.disk.is_empty() {
                self.disk
                    .apply_unsynced(std::mem::take(&mut parts.disk))
                    .await?;
            }
            self.land_held(parts).await
        })
    }

    fn sync_applied(&self) -> KvFuture<'_, ()> {
        self.disk.sync_applied()
    }

    fn apply_group(&self, batches: Vec<WriteBatch>) -> GroupFuture<'_> {
        Box::pin(async move {
            let (parts, refused) = match self.split(batches).await {
                Ok(split) => split,
                Err(failure) => return (0, Err(failure)),
            };
            // The disk halves land first, as one group, and only the batches
            // whose disk half landed have their memory half applied.
            let mut durable = Vec::new();
            let mut owners = Vec::new();
            for (index, part) in parts.iter().enumerate() {
                if !part.disk.is_empty() {
                    durable.push(part.disk.clone());
                    owners.push(index);
                }
            }
            let (on_disk, failed) = if durable.is_empty() {
                (0, Ok(()))
            } else {
                self.disk.apply_group(durable).await
            };
            let landed = match &failed {
                Ok(()) => parts.len(),
                Err(_) => owners.get(on_disk).copied().unwrap_or(parts.len()),
            };
            for part in parts.into_iter().take(landed) {
                if part.holds_memory()
                    && let Err(failure) = self.land_held(part).await
                {
                    return (0, Err(failure));
                }
            }
            match (failed, refused) {
                (Err(failure), _) => (landed, Err(failure)),
                (Ok(()), Some(refused)) => (landed, Err(refused)),
                (Ok(()), None) => (landed, Ok(())),
            }
        })
    }

    fn groups_writes(&self) -> bool {
        self.disk.groups_writes()
    }

    fn background_errors(&self) -> KvFuture<'_, u64> {
        self.disk.background_errors()
    }

    fn close(self: Arc<Self>) -> KvFuture<'static, ()> {
        Arc::clone(&self.disk).close()
    }
}
