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

    // A join token offered by the node the handshake proved (ADR-0108 D9). The
    // fingerprint is not compared here: a pinned row binds in `met`, where the
    // presented certificate is in hand, and a token is evidence of its own.
    fn joined(
        &self,
        node: [u8; tessari_storage::NODE_ID_LEN],
        token: &[u8; 32],
    ) -> tessari_wire::Result<bool> {
        Ok(bind_the_greeter(&self.db, node, None, Some(token)))
    }

    fn places(
        &self,
        candidate: [u8; tessari_storage::NODE_ID_LEN],
        range: tessari_types::Reach,
    ) -> bool {
        tessari_wire::Serving::declared(self.db.store()).places(candidate, range)
    }

    fn reached_on(
        &self,
        range: tessari_types::Reach,
    ) -> tessari_wire::Result<Option<tessari_wire::Reached>> {
        tessari_wire::Serving::declared(self.db.store()).reached_on(range)
    }

    fn attempted(&self, asked: &tessari_wire::Attempt) -> bool {
        asked.answered(self.db.store())
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
        bind_the_greeter(&self.db, met.said.node, Some(&met.presented), None);
    }

    fn commits(&self) -> tokio::sync::watch::Receiver<u64> {
        self.db.commits().watching()
    }

    // A request another node carried here for its caller (ADR-0108 D1–D3). The
    // door believed the signature; this node judges the account and the reach,
    // runs the script as that user — its own grants decide — and answers in
    // the shape the caller's surface reads.
    fn coordinated(
        &self,
        from: [u8; tessari_storage::NODE_ID_LEN],
        assertion: &tessari_wire::Assertion,
        asked: &tessari_wire::Coordinate,
    ) -> Result<tessaridb::Coordinated, String> {
        let mut session = tessari_wire::admit_asserted(&self.db, from, assertion)?;
        let ran = session.run_coordinated(
            (asked.namespace.as_deref(), asked.database.as_deref()),
            &asked.script,
            &asked.parameters,
        );
        Ok(match asked.surface {
            tessaridb::Surface::Wire { .. } => tessari_wire::render_coordinated(&self.db, &ran),
            tessaridb::Surface::Http => tessari_http::render_coordinated(&self.db, &ran),
        })
    }

    // A record of a transaction across leaders another node carried here
    // (ADR-0112): judged as a coordinated request is — the account and the
    // reach — then written as that user, whose grants here decide.
    fn across(
        &self,
        from: [u8; tessari_storage::NODE_ID_LEN],
        assertion: &tessari_wire::Assertion,
        asked: &[u8],
    ) -> Result<Vec<u8>, String> {
        let asked = tessari_session::AcrossAsk::decode(asked)?;
        let mut session = tessari_wire::admit_asserted(&self.db, from, assertion)?;
        session
            .answer_across(&asked)
            .map(|answer| answer.encode().to_vec())
            .map_err(|refused| refused.to_string())
    }
}
