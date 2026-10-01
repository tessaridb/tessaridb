//! What the peer door asks of this node's store.

use crate::greeting_round::{bind_the_greeter, greeting, hex};
use tessaridb::Db;

/// The node behind the peer door: its log, what it holds, and the catalog a
/// greeting may bind a row in.
pub(crate) struct PeerDoor {
    pub(crate) db: std::sync::Arc<Db>,
}

impl tessari_wire::Origin for PeerDoor {
    // The door serves the log to exactly the peers this store's own catalog
    // subscribed.
    fn collected(
        &self,
        follower: [u8; tessari_storage::NODE_ID_LEN],
        asked: tessari_wire::Collect,
    ) -> tessari_wire::Result<tessari_wire::Collected> {
        tessari_wire::Serving::declared(self.db.store()).collected(follower, asked)
    }

    fn gathered(
        &self,
        asker: [u8; tessari_storage::NODE_ID_LEN],
        asked: &tessari_wire::Gather,
    ) -> tessari_wire::Result<tessari_wire::Page> {
        tessari_wire::Serving::declared(self.db.store()).gathered(asker, asked)
    }

    fn copied(
        &self,
        follower: [u8; tessari_storage::NODE_ID_LEN],
        write: &mut dyn FnMut(u8, Vec<u8>) -> tessari_wire::Result<()>,
    ) -> tessari_wire::Result<()> {
        tessari_wire::Serving::declared(self.db.store()).copied(follower, write)
    }
}

impl tessari_wire::Holding for PeerDoor {
    // Read when a peer has arrived and proved who it is — not before the wait.
    // A door idle for an hour used to greet with hour-old epoch, tail and copy
    // age, which are exactly the fields a router reads.
    fn hello(&self) -> tessari_wire::Result<tessari_wire::Hello> {
        greeting(&self.db).map_err(|why| tessari_wire::Error::NothingToSay(why.to_string()))
    }

    fn met(&self, met: &tessari_wire::Met) {
        log::info!(
            "peer {} greeted at epoch {}, tail {}{}",
            hex(&met.said.node),
            met.said.epoch.get(),
            met.said.tail.get(),
            met.voted
                .map_or(String::new(), |vote| format!(", {vote:?}")),
        );
        bind_the_greeter(&self.db, met.said.node);
    }
}
