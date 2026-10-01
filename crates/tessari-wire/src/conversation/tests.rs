//! Which refusals leave the node as a redirect (G051 SG3).

#![allow(clippy::unwrap_used)]

use tessari_encoding::NODE_ID_LEN;
use tessari_session::Peer;
use tessari_types::Epoch;
use tessaridb::Db;

use super::redirected;
use crate::redirect::{Elsewhere, Settlement};

const WHOLE_AT: &str = "whole.example:9180";

fn whole() -> Peer {
    Peer {
        endpoint: WHOLE_AT.to_owned(),
        node: [5; NODE_ID_LEN],
        epoch: Epoch::new(11),
    }
}

/// A moved map naming a whole holder is the read a client follows there once,
/// and remembers nothing: the map moves back into agreement on its own.
#[test]
fn a_moved_map_naming_a_whole_holder_is_a_transient_redirect() {
    let db = Db::in_memory().unwrap();
    let moved = |holder| {
        Err(tessaridb::Error::ShardMapMoved {
            table: "ledger".to_owned(),
            shard: 1,
            holder,
        })
    };
    assert_eq!(
        redirected(&db, &moved(Some(whole()))),
        Some(Elsewhere {
            endpoint: WHOLE_AT.to_owned(),
            node: [5; NODE_ID_LEN],
            epoch: Epoch::new(11),
            settlement: Settlement::Transient,
        })
    );
    assert_eq!(redirected(&db, &moved(None)), None, "nobody to send it to");
}
