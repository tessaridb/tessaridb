//! A RESP client on a node that does not lead a held space, answered by the
//! node that does (G079 W5d).

use std::sync::Arc;

use tessari_encoding::Roles;
use tessari_storage::{Catalog, Reach};
use tessari_types::{Epoch, NamespaceId};
use tokio_util::sync::CancellationToken;

use super::{Client, bulk};
use crate::codec::Reply;
use crate::surface::{Node, Space};

const SETUP: &str = "DEFINE NAMESPACE app; USE NAMESPACE app; DEFINE DATABASE main; \
                     USE DATABASE main; DEFINE SPACE hot IN MEMORY MAX 100;";

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

/// A node whose catalog says another node leads `app`, serving RESP over its
/// held space, and that leader.
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
        .serving_space(Space::parse("app.main.hot").unwrap());
    let address = node.address().unwrap();
    let stop = CancellationToken::new();
    let stopping = stop.clone();
    tokio::spawn(async move { node.serve(stopping).await });
    (address, stop, leader)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_on_a_node_that_does_not_lead_is_answered_by_the_leader() {
    let (address, stop, leader) = beside_the_leader(true).await;
    let mut client = Client::to(&address).await;
    assert_eq!(client.ask(&["SET", "n", "1"]).await, Reply::OK);
    assert_eq!(client.ask(&["INCR", "n"]).await, Reply::Integer(2));
    assert_eq!(client.ask(&["GET", "n"]).await, bulk("2"));
    // The key is in the leader's memory: read there directly.
    let mut session = leader.session();
    let answered = session
        .run("USE NAMESPACE app; USE DATABASE main; GET hot:'n';")
        .await
        .unwrap();
    assert!(
        matches!(answered.last(), Some(tessaridb::Outcome::Value(value)) if *value == tessari_types::Value::from(2)),
        "{answered:?}"
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_that_carries_nothing_answers_where_to_go() {
    let (address, stop, _) = beside_the_leader(false).await;
    let mut client = Client::to(&address).await;
    let answered = client.ask(&["SET", "n", "1"]).await;
    assert!(
        matches!(&answered, Reply::Error(said)
            if said.starts_with("TRYAGAIN") && said.contains("10.0.0.2:9081")),
        "{answered:?}"
    );
    stop.cancel();
}
