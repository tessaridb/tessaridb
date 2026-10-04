#![allow(clippy::unwrap_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::thread::JoinHandle;

use tessari_encoding::NODE_ID_LEN;
use tessari_storage::{Catalog, Reach};
use tessari_types::{DatabaseId, NamespaceId, RecordId, ShardId, TableId};
use tessaridb::Db;

use super::{Gather, Page, Ungathered};
use crate::collection::{Serving, Subscriptions};
use crate::error::{Error, Result};
use crate::grant::Deciding;
use crate::link::tests::{Authority, THERE, hello, settled};
use crate::link::{Answered, Ask};
use crate::peer::Purpose;

mod folding;
mod narrowing;
mod pages;

const LEADER: [u8; NODE_ID_LEN] = [71_u8; NODE_ID_LEN];

/// A catalog answer a test chooses.
#[derive(Debug)]
struct Granting(Option<Reach>);

impl Subscriptions for Granting {
    fn granted(&self, _follower: [u8; NODE_ID_LEN]) -> Result<Option<Reach>> {
        Ok(self.0)
    }
}

fn leader() -> (Arc<Db>, TableId) {
    let db = Db::in_memory().unwrap();
    db.session()
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; \
                 USE DATABASE shop; \
                 DEFINE TABLE ledger (n int) IDENTITY uuid SPLIT AT 'g'; \
                 CREATE ledger:'a' = { n: 1 }; CREATE ledger:'b' = { n: 2 }; \
                 CREATE ledger:'c' = { n: 3 }; CREATE ledger:'h' = { n: 4 };",
        )
        .unwrap();
    let table = {
        let mut transaction = db.store().begin().unwrap();
        Catalog::new(&mut transaction)
            .table_id(NamespaceId::new(1), DatabaseId::new(1), "ledger")
            .unwrap()
            .unwrap()
    };
    (Arc::new(db), table)
}

fn shard(table: TableId, id: u32) -> Reach {
    Reach::Shard(
        NamespaceId::new(1),
        DatabaseId::new(1),
        table,
        ShardId::new(id),
    )
}

/// A door for `LEADER` answering `rounds` connections out of `db`.
fn door(
    authority: &Authority,
    db: &Arc<Db>,
    granted: Option<Reach>,
    budget: usize,
    rounds: usize,
) -> (SocketAddr, JoinHandle<()>) {
    folding_door(
        authority,
        db,
        granted,
        (budget, tessari_constants::GATHER_FOLD_RECORDS),
        rounds,
    )
}

/// The same, folding at most `fold` records into a page of groups.
fn folding_door(
    authority: &Authority,
    db: &Arc<Db>,
    granted: Option<Reach>,
    (budget, fold): (usize, usize),
    rounds: usize,
) -> (SocketAddr, JoinHandle<()>) {
    let peers = crate::link::tests::bind_with(
        "127.0.0.1:0",
        authority.issue(LEADER, Purpose::Peer),
        &authority.der(),
    )
    .unwrap();
    let address = peers.address().unwrap();
    let mine = hello(LEADER);
    let db = Arc::clone(db);
    let handle = std::thread::spawn(move || {
        let granting = Granting(granted);
        for _ in 0..rounds {
            drop(peers.greet(
                || Ok(mine),
                &LEADER,
                &Deciding::holding(settled()),
                &Serving::within(db.store(), &granting, budget).folding_by(fold),
            ));
        }
    });
    (address, handle)
}

fn ask(authority: &Authority, address: SocketAddr, gather: &Gather) -> Result<Page> {
    match crate::link::tests::call_with(
        address,
        authority.issue(THERE, Purpose::Peer),
        &authority.der(),
        LEADER,
        &hello(THERE),
        Ask::Gather(gather),
    )?
    .1
    {
        Answered::Gathered(page) => Ok(page),
        other => panic!("answered {other:?}"),
    }
}

fn asking(table: TableId, shard: u32) -> Gather {
    Gather {
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(1),
        table,
        shard: ShardId::new(shard),
        from: None,
        to: None,
        after: None,
        pushed: None,
        enough: None,
        reduce: None,
        ordered: None,
        counting: None,
    }
}

fn narrowed(condition: &str, bound: i64, visible: Option<&str>) -> tessari_session::Pushed {
    tessari_session::Pushed {
        visible: visible.map(|field| [field.to_owned()].into()),
        condition: condition.to_owned(),
        parameters: [("p0".to_owned(), tessari_types::Value::from(bound))].into(),
    }
}
