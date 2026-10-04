//! A graph's nodes, read one at a time as a walk reaches them (G058 C2, Q-906).
//!
//! Every approximate read used to decode the whole graph before its first step —
//! about 6 ms at 20 000 nodes of 32 components, the floor under every walk —
//! while the walk itself visits a few hundred. Here a node is read by its key
//! the first time the operation asks for it and kept for the rest of that
//! operation.
//!
//! # The cache, stated
//!
//! - **Key**: the node's record id within one index (the store key is the
//!   index's address, the level and the id).
//! - **Bound**: the nodes one operation touches — a walk's visited set, or a
//!   write batch's walks and edits. Nothing outlives the operation; the
//!   engine's own block cache, which is bounded and invalidated by the engine,
//!   holds the bytes across operations.
//! - **Invalidation**: none needed, because nothing is shared. A batch's own
//!   edits are held here as the newest version of each node it touched, so
//!   the walks later in the same batch see them, exactly as they did when the
//!   whole graph was held.
//!
//! # The entry point
//!
//! The walk starts from the smallest record id the graph holds, as it always
//! has — which is what keeps two replicas building one graph. Node keys sort in
//! record-id order (the key grammar is order-preserving), so that is the first
//! stored key not removed in this operation, or a node this operation added if
//! that is smaller.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use tessari_encoding::{IndexAddress, StoreKey, StoreValue, VectorNode, VectorNodeKey};
use tessari_kv::{KeyRange, KvBackend, ScanDirection, ScanRequest};
use tessari_types::RecordId;

use super::GROUND;
use crate::error::Result;

/// Where the nodes this operation has not read yet are.
struct Stored {
    backend: Arc<dyn KvBackend>,
    address: IndexAddress,
}

/// The nodes of one graph as one operation sees them.
pub(super) struct Nodes {
    /// Every node read or written so far; `None` is a node this operation
    /// removed, or one it looked for and found absent.
    held: RefCell<BTreeMap<RecordId, Option<Arc<VectorNode>>>>,
    /// The ids this operation wrote a node at — the only ones the store may
    /// not hold yet.
    written: RefCell<BTreeSet<RecordId>>,
    stored: Option<Stored>,
    /// Whether `held` is the whole graph — built in memory, or read whole.
    complete: RefCell<bool>,
}

impl std::fmt::Debug for Nodes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Nodes")
            .field("held", &self.held.borrow().len())
            .field("complete", &*self.complete.borrow())
            .finish_non_exhaustive()
    }
}

impl Nodes {
    /// A graph with nothing stored behind it.
    pub(super) fn in_memory() -> Self {
        Self {
            held: RefCell::new(BTreeMap::new()),
            written: RefCell::new(BTreeSet::new()),
            stored: None,
            complete: RefCell::new(true),
        }
    }

    /// The nodes of an index, read as they are asked for.
    pub(super) fn stored(backend: Arc<dyn KvBackend>, address: IndexAddress) -> Self {
        Self {
            held: RefCell::new(BTreeMap::new()),
            written: RefCell::new(BTreeSet::new()),
            stored: Some(Stored { backend, address }),
            complete: RefCell::new(false),
        }
    }

    /// The node `id`, if the graph holds it.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or the node cannot be decoded.
    pub(super) fn get(&self, id: &RecordId) -> Result<Option<Arc<VectorNode>>> {
        if let Some(held) = self.held.borrow().get(id) {
            return Ok(held.clone());
        }
        let Some(stored) = self.stored.as_ref().filter(|_| !*self.complete.borrow()) else {
            return Ok(None);
        };
        let key = VectorNodeKey::new(stored.address, GROUND, id.clone()).encode();
        let found = stored
            .backend
            .get(VectorNodeKey::keyspace(), &key)?
            .map(|bytes| VectorNode::decode(bytes.as_slice()).map(Arc::new))
            .transpose()?;
        self.held.borrow_mut().insert(id.clone(), found.clone());
        Ok(found)
    }

    /// Whether the graph holds `id`.
    ///
    /// # Errors
    ///
    /// As [`Self::get`].
    pub(super) fn contains(&self, id: &RecordId) -> Result<bool> {
        Ok(self.get(id)?.is_some())
    }

    /// Put `node` at `id`, the newest version for the rest of the operation.
    pub(super) fn put(&self, id: RecordId, node: VectorNode) {
        self.written.borrow_mut().insert(id.clone());
        self.held.borrow_mut().insert(id, Some(Arc::new(node)));
    }

    /// Take `id` out of the graph for the rest of the operation.
    pub(super) fn remove(&self, id: &RecordId) {
        self.held.borrow_mut().insert(id.clone(), None);
    }

    /// The smallest record id the graph holds — the walk's entry point.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a key cannot be decoded.
    pub(super) fn first(&self) -> Result<Option<RecordId>> {
        let added = self
            .held
            .borrow()
            .iter()
            .find(|(_, node)| node.is_some())
            .map(|(id, _)| id.clone());
        let stored = match self.stored.as_ref().filter(|_| !*self.complete.borrow()) {
            Some(stored) => self.first_stored(stored)?,
            None => None,
        };
        Ok(match (added, stored) {
            (Some(one), Some(other)) => Some(one.min(other)),
            (one, other) => one.or(other),
        })
    }

    /// The first stored node this operation has not removed.
    fn first_stored(&self, stored: &Stored) -> Result<Option<RecordId>> {
        // The removed ones are few — what one batch deleted — so asking for one
        // more than there are always reaches a survivor if one exists.
        let removed = self
            .held
            .borrow()
            .values()
            .filter(|node| node.is_none())
            .count();
        let request = ScanRequest {
            keyspace: VectorNodeKey::keyspace(),
            range: KeyRange::prefix(&VectorNodeKey::level_prefix(&stored.address, GROUND)),
            direction: ScanDirection::Forward,
            limit: Some(removed.saturating_add(1)),
        };
        for (key, _) in stored.backend.scan(&request)? {
            let id = VectorNodeKey::decode(key.as_slice())?.id;
            if !matches!(self.held.borrow().get(&id), Some(None)) {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    /// Whether the graph holds more than `count` nodes, reading at most
    /// `count + 1` stored keys.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a key cannot be decoded.
    pub(super) fn more_than(&self, count: usize) -> Result<bool> {
        if *self.complete.borrow() {
            let held = self
                .held
                .borrow()
                .values()
                .filter(|node| node.is_some())
                .count();
            return Ok(held > count);
        }
        let Some(stored) = &self.stored else {
            return Ok(false);
        };
        let request = ScanRequest {
            keyspace: VectorNodeKey::keyspace(),
            range: KeyRange::prefix(&VectorNodeKey::level_prefix(&stored.address, GROUND)),
            direction: ScanDirection::Forward,
            limit: Some(count.saturating_add(1)),
        };
        let mut scanned = BTreeSet::new();
        for (key, _) in stored.backend.scan(&request)? {
            scanned.insert(VectorNodeKey::decode(key.as_slice())?.id);
        }
        let held = self.held.borrow();
        let present = scanned
            .iter()
            .filter(|id| !matches!(held.get(*id), Some(None)))
            .count();
        // Nodes this operation wrote that the scan did not reach.
        let added = self
            .written
            .borrow()
            .iter()
            .filter(|id| !scanned.contains(*id) && matches!(held.get(*id), Some(Some(_))))
            .count();
        Ok(present.saturating_add(added) > count)
    }

    /// Read every stored node this operation has not read or changed, so the
    /// graph can be walked whole — what a recall measurement needs.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a node cannot be decoded.
    pub(super) fn load_all(&self) -> Result<()> {
        if *self.complete.borrow() {
            return Ok(());
        }
        if let Some(stored) = &self.stored {
            let request = ScanRequest {
                keyspace: VectorNodeKey::keyspace(),
                range: KeyRange::prefix(&VectorNodeKey::level_prefix(&stored.address, GROUND)),
                direction: ScanDirection::Forward,
                limit: None,
            };
            for (key, value) in stored.backend.scan(&request)? {
                let id = VectorNodeKey::decode(key.as_slice())?.id;
                if self.held.borrow().contains_key(&id) {
                    continue;
                }
                let node = VectorNode::decode(value.as_slice())?;
                self.held.borrow_mut().insert(id, Some(Arc::new(node)));
            }
        }
        *self.complete.borrow_mut() = true;
        Ok(())
    }

    /// How many nodes this operation holds, read or written — the cache's
    /// bound, asserted by the tests.
    #[cfg(test)]
    pub(super) fn held(&self) -> usize {
        self.held.borrow().len()
    }

    /// Every node the graph holds, in record-id order; call [`Self::load_all`]
    /// first.
    pub(super) fn present(&self) -> Vec<(RecordId, Arc<VectorNode>)> {
        self.held
            .borrow()
            .iter()
            .filter_map(|(id, node)| node.clone().map(|node| (id.clone(), node)))
            .collect()
    }
}
