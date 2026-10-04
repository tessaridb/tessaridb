use super::{Destination, Directory};
use core::cell::RefCell;
use core::time::Duration;
use std::time::Instant;
use tessari_encoding::{NODE_ID_LEN, NodeVersion, Roles};
use tessari_storage::ReplicaDefinition;
use tessari_types::{Epoch, Sequence};

use crate::peer::Hello;

mod greeting;
mod leaders;
mod routing;

const ONE: [u8; NODE_ID_LEN] = [1; NODE_ID_LEN];
const ANOTHER: [u8; NODE_ID_LEN] = [2; NODE_ID_LEN];
const THIRD: [u8; NODE_ID_LEN] = [3; NODE_ID_LEN];

/// A declared peer row: a name, where it answers, and who is there.
fn declared(id: u32, endpoint: &str, node: Option<[u8; NODE_ID_LEN]>) -> ReplicaDefinition {
    ReplicaDefinition {
        name: format!("peer{id}"),
        endpoint: endpoint.to_owned(),
        roles: Roles::SERVING,
        node,
        // Routing is not subscription: which peers this node greets is a
        // different question from what those peers may collect, so these
        // rows deliberately grant nothing.
        replicates: None,
        leads: None,
        clients: None,
        http: None,
        fingerprint: None,
        join: None,
        releasing: false,
        preferred: false,
        region: None,
    }
}

/// A greeting function that records every endpoint it was asked to dial,
/// and refuses the ones named in `silent`.
fn greeter<'a>(
    dialled: &'a RefCell<Vec<String>>,
    silent: &'a [&'a str],
) -> impl Fn(&str, [u8; NODE_ID_LEN]) -> Result<Hello, ()> + 'a {
    move |endpoint, node| {
        dialled.borrow_mut().push(endpoint.to_owned());
        if silent.contains(&endpoint) {
            return Err(());
        }
        Ok(said(node, Some(Duration::from_secs(1)), true))
    }
}

/// A greeting from `node`, saying its copy is `age` old and that it `serves`.
fn said(node: [u8; NODE_ID_LEN], age: Option<Duration>, serves: bool) -> Hello {
    Hello {
        node,
        build: NodeVersion {
            major: 0,
            minor: 1,
            patch: 1,
        },
        epoch: Epoch::new(7),
        roles: if serves { Roles::SERVING } else { Roles::NONE },
        tail: Sequence::new(4096),
        tail_leadership: Epoch::new(1),
        current_as_of: age,
        policy: None,
        line: None,
    }
}

/// A directory holding one serving peer at `two.example:9080`, five seconds
/// old when it was heard.
fn one_peer(heard_at: Instant) -> Directory {
    let mut directory = Directory::new();
    directory.heard(
        "two.example:9080",
        said(ANOTHER, Some(Duration::from_secs(5)), true),
        heard_at,
    );
    directory
}

/// A greeting from a node that claims the writable role at `epoch`.
fn leads(node: [u8; NODE_ID_LEN], epoch: u64) -> Hello {
    let mut hello = said(node, Some(Duration::from_secs(1)), true);
    hello.roles = Roles::SERVING.and(Roles::WRITABLE);
    hello.epoch = Epoch::new(epoch);
    hello
}
