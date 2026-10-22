//! A producer on a node that does not lead the topic, answered by the node that
//! does (G079 W5d).

use std::sync::Arc;

use tessari_encoding::Roles;
use tessari_storage::{Catalog, Reach};
use tessari_types::{Epoch, NamespaceId};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

use super::{ask, produce_body, produced};
use crate::codec::Writer;
use crate::records::{Outgoing, encode_batch};
use crate::surface::{Database, Node};

const SETUP: &str = "DEFINE NAMESPACE app; USE NAMESPACE app; DEFINE DATABASE main; \
                     USE DATABASE main; DEFINE TOPIC orders;";

/// Runs a carried script on the leader, as the peer door does.
#[derive(Debug)]
struct ToTheLeader(Arc<tessaridb::Db>);

impl tessaridb::Coordinate for ToTheLeader {
    fn coordinate<'a>(
        &'a self,
        asked: &'a tessaridb::Coordination<'_>,
    ) -> tessaridb::Coordinating<'a> {
        Box::pin(async move {
            let mut session = self.0.session();
            let ran = session
                .run_coordinated(
                    (asked.namespace, asked.database),
                    asked.script,
                    asked.parameters,
                )
                .await;
            Ok(tessaridb::carried::render(&ran))
        })
    }
}

/// A node whose catalog says another node leads `app`, serving Kafka, and
/// that leader.
async fn beside_the_leader(carrying: bool) -> (String, CancellationToken, Arc<tessaridb::Db>) {
    let leader = Arc::new(tessaridb::Db::in_memory().await.unwrap());
    leader.session().run(SETUP).await.unwrap();
    let here = Arc::new(tessaridb::Db::in_memory().await.unwrap());
    here.session().run(SETUP).await.unwrap();
    let mut transaction = here.store().begin().await.unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    catalog
        .create_replica(
            "leader",
            "10.0.0.2:9081",
            Roles::SERVING.and(Roles::WRITABLE).and(Roles::COORDINATING),
            Some([9; 16]),
            None,
            None,
        )
        .await
        .unwrap();
    catalog
        .record_leadership(
            Reach::Namespace(NamespaceId::new(1)),
            [9; 16],
            Epoch::new(7),
        )
        .unwrap();
    transaction.commit().await.unwrap();
    if carrying {
        here.coordinate_through(Arc::new(ToTheLeader(Arc::clone(&leader))));
    }
    let node = Node::bind(Arc::clone(&here), "127.0.0.1:0")
        .unwrap()
        .reading_bare_names_in(Database::parse("app.main").unwrap());
    let address = node.address().unwrap();
    let stop = CancellationToken::new();
    let stopping = stop.clone();
    tokio::spawn(async move { node.serve(stopping).await });
    (address, stop, leader)
}

fn one_record() -> Vec<u8> {
    let mut batch = Writer::new();
    encode_batch(
        &mut batch,
        0,
        &[Outgoing {
            timestamp: 1_760_000_000_000,
            key: Some(b"k"),
            value: Some(b"v"),
            headers: &[],
        }],
    )
    .unwrap();
    batch.into_bytes()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_produce_on_a_node_that_does_not_lead_lands_on_the_leader() {
    let (address, stop, leader) = beside_the_leader(true).await;
    let mut stream = TcpStream::connect(&address).await.unwrap();
    let batch = one_record();
    let body = produce_body("orders", &batch);
    assert_eq!(produced(&ask(&mut stream, 0, 9, true, &body).await), (0, 0));
    assert_eq!(produced(&ask(&mut stream, 0, 9, true, &body).await), (0, 1));
    // Both are the leader's: its topic holds two positions.
    let answered = leader
        .session()
        .run("USE NAMESPACE app; USE DATABASE main; READ FROM orders LIMIT 10;")
        .await
        .unwrap();
    assert!(
        matches!(answered.last(), Some(tessaridb::Outcome::Records { records, .. }) if records.len() == 2),
        "{answered:?}"
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_that_carries_nothing_refuses_the_produce() {
    let (address, stop, _) = beside_the_leader(false).await;
    let mut stream = TcpStream::connect(&address).await.unwrap();
    let body = produce_body("orders", &one_record());
    let (error, _) = produced(&ask(&mut stream, 0, 9, true, &body).await);
    assert_ne!(error, 0);
    stop.cancel();
}
