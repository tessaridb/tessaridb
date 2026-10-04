#![allow(clippy::panic, clippy::unwrap_used)]

use super::*;
use tessari_types::{DatabaseId, Epoch, NamespaceId, RecordId, Sequence, ShardId, TableId};

mod codec;
mod expiry;
mod records;
mod stamps;

const ONE_NODE: [u8; NODE_ID_LEN] = [1; NODE_ID_LEN];
const ANOTHER_NODE: [u8; NODE_ID_LEN] = [2; NODE_ID_LEN];

fn a_stamp(nodes: &[([u8; NODE_ID_LEN], u64)]) -> CausalStamp {
    let mut stamp = CausalStamp::new();
    for (node, times) in nodes {
        for _ in 0..*times {
            stamp.advance(*node);
        }
    }
    stamp
}
