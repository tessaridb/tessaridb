//! Who leads each election line, as the write gate judges it (ADR-0113 D3).

use std::collections::BTreeSet;

use tessari_encoding::NODE_ID_LEN;
use tessari_types::Reach;

use super::Store;
use super::leadership::Led;
use crate::error::Result;

impl Store {
    /// The store line and every placed range, each with the node that leads
    /// it — `None` while nobody does, or while the range admits two writers.
    ///
    /// One catalog read for all of them, and the answer [`Self::leader_of`]
    /// gives for each, so a balancer counting what a node leads and a write
    /// redirected to that node cannot disagree.
    ///
    /// # Errors
    ///
    /// The substrate's failure, and a decoding failure when a stored definition
    /// cannot be read.
    pub fn line_leaders(&self) -> Result<Vec<(Reach, Option<[u8; NODE_ID_LEN]>)>> {
        let me = self.node_identity()?.id;
        let mut transaction = self.begin()?;
        let catalog = crate::catalog::Catalog::new(&mut transaction);
        let held = catalog.leaderships()?;
        let placed: BTreeSet<Reach> = catalog
            .replicas()?
            .into_iter()
            .filter_map(|peer| peer.leads)
            .collect();
        drop(transaction);
        std::iter::once(Reach::Store)
            .chain(placed.iter().copied())
            .map(|line| {
                let leader = match self.led(&held, &placed, line, &me)? {
                    Led::Here => Some(me),
                    Led::Elsewhere(leader) => Some(leader.node),
                    Led::Unled | Led::Shared => None,
                };
                Ok((line, leader))
            })
            .collect()
    }
}
