//! A subscriber and a node, over a real socket.
//!
//! These are the tests that cannot be written against the codec: that a change
//! written *after* a subscribe actually arrives, that a backlog is replayed
//! rather than skipped, that a subscriber watching one table is not told about
//! another, and that a client which stops reading ends its own connection rather
//! than the node's memory.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use tessari_wire::{Became, Client, Follow, Happened, Node};
use tessaridb::{Db, Value};

/// A node on a loopback port the operating system picked, plus its address.
fn serving(db: Arc<Db>) -> (Arc<Node>, String) {
    let node = Arc::new(Node::bind(db, "127.0.0.1:0").unwrap());
    let address = node.address().unwrap();
    let held = Arc::clone(&node);
    drop(std::thread::spawn(move || held.serve()));
    (node, address)
}

const READY: &str = "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
                     DEFINE DATABASE orders; USE DATABASE orders; \
                     DEFINE COLLECTION users; DEFINE COLLECTION orders;";

/// A connection with a database selected, ready to run or to follow.
fn selected(address: &str) -> Client {
    let mut client = Client::connect(address).unwrap();
    client
        .run("USE NAMESPACE prod; USE DATABASE orders;", None)
        .unwrap();
    client
}

/// Read a feed on its own thread and hand each change over as it arrives.
///
/// A blocking read cannot be given a deadline from outside it, and a test that
/// waits forever for a change that will never come reports as a timeout
/// somewhere else — usually in whatever runs the suite — and says nothing about
/// which assertion was waiting. A thread and a channel give the deadline back to
/// the assertion, and are also exactly the shape a real subscriber has.
fn feeding(client: Client, asked: &Follow) -> mpsc::Receiver<Happened> {
    let mut feed = client.follow(asked).unwrap();
    let (sent, waiting) = mpsc::channel();
    drop(std::thread::spawn(move || {
        while let Ok(Some(change)) = feed.wait() {
            if sent.send(change).is_err() {
                break;
            }
        }
    }));
    waiting
}

/// Where `READY`'s records land, and therefore which log a feed counts in.
///
/// The facade resolves a subscription's log from the session's tenancy, so a
/// position taken from the store's own log would name a moment in a counter the
/// feed never reads (S6.2, Q-620).
const FIXTURE_HOME: tessaridb::Reach = tessaridb::Reach::Database(
    tessaridb::NamespaceId::new(1),
    tessaridb::DatabaseId::new(1),
);

/// The next change, or a failure naming what was being waited for.
fn within(feed: &mpsc::Receiver<Happened>, what: &str) -> Happened {
    feed.recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|_| panic!("{what} did not arrive"))
}

#[test]
fn a_change_written_after_a_subscribe_arrives() {
    let db = Arc::new(Db::in_memory().unwrap());
    let (_node, address) = serving(Arc::clone(&db));
    let mut writer = Client::connect(&address).unwrap();
    writer.run(READY, None).unwrap();

    let feed = feeding(
        selected(&address),
        &Follow {
            from: db
                .committed_tail(db.store().own_log(FIXTURE_HOME).unwrap())
                .unwrap()
                .get()
                + 1,
            table: None,
        },
    );

    writer
        .run("CREATE users:1 = { name: 'ada' };", None)
        .unwrap();

    let change = within(&feed, "a change");
    assert_eq!(change.table, "users");
    assert_eq!(change.id, "1");
    match change.became {
        Became::Written(Value::Object(held)) => {
            assert_eq!(held.get("name"), Some(&Value::from("ada")));
        }
        other => panic!("not a write: {other:?}"),
    }
}

#[test]
fn a_subscription_from_an_earlier_position_replays_what_it_missed() {
    // The whole reason a position is a number the client keeps: a subscriber
    // that was away comes back to what happened while it was.
    let db = Arc::new(Db::in_memory().unwrap());
    let (_node, address) = serving(Arc::clone(&db));
    let mut writer = Client::connect(&address).unwrap();
    writer.run(READY, None).unwrap();
    let before = db
        .committed_tail(db.store().own_log(FIXTURE_HOME).unwrap())
        .unwrap()
        .get();
    writer
        .run(
            "CREATE users:1 = { name: 'ada' }; CREATE users:2 = { name: 'grace' };",
            None,
        )
        .unwrap();

    let feed = feeding(
        selected(&address),
        &Follow {
            from: before,
            table: None,
        },
    );

    let first = within(&feed, "the first");
    let second = within(&feed, "the second");
    assert_eq!(first.id, "1");
    assert_eq!(second.id, "2");
}

#[test]
fn watching_one_table_is_not_told_about_another() {
    let db = Arc::new(Db::in_memory().unwrap());
    let (_node, address) = serving(Arc::clone(&db));
    let mut writer = Client::connect(&address).unwrap();
    writer.run(READY, None).unwrap();

    let feed = feeding(
        selected(&address),
        &Follow {
            from: db
                .committed_tail(db.store().own_log(FIXTURE_HOME).unwrap())
                .unwrap()
                .get()
                + 1,
            table: Some("orders".to_owned()),
        },
    );

    // The write to `users` comes first, so if the filter did nothing this would
    // be what arrives.
    writer
        .run("CREATE users:1 = { name: 'ada' };", None)
        .unwrap();
    writer.run("CREATE orders:1 = { total: 3 };", None).unwrap();

    let change = within(&feed, "a change");
    assert_eq!(change.table, "orders");
    assert_eq!(change.id, "1");
}

#[test]
fn a_removal_is_told_apart_from_a_write() {
    let db = Arc::new(Db::in_memory().unwrap());
    let (_node, address) = serving(Arc::clone(&db));
    let mut writer = Client::connect(&address).unwrap();
    writer.run(READY, None).unwrap();
    writer
        .run("CREATE users:1 = { name: 'ada' };", None)
        .unwrap();

    let feed = feeding(
        selected(&address),
        &Follow {
            from: db
                .committed_tail(db.store().own_log(FIXTURE_HOME).unwrap())
                .unwrap()
                .get()
                + 1,
            table: None,
        },
    );

    writer.run("DELETE users:1;", None).unwrap();

    let change = within(&feed, "a change");
    assert_eq!(change.became, Became::Removed);
}

#[test]
fn a_pushed_value_is_the_value_it_was_and_not_a_projection() {
    // A subscriber applying changes has the console's problem: a decimal it has
    // to decide about is a decimal it will get wrong.
    let db = Arc::new(Db::in_memory().unwrap());
    let (_node, address) = serving(Arc::clone(&db));
    let mut writer = Client::connect(&address).unwrap();
    writer.run(READY, None).unwrap();

    let feed = feeding(
        selected(&address),
        &Follow {
            from: db
                .committed_tail(db.store().own_log(FIXTURE_HOME).unwrap())
                .unwrap()
                .get()
                + 1,
            table: None,
        },
    );

    writer
        .run(
            "CREATE users:1 = { owed: dec 12.34, waited: 2s, seen: datetime '2026-01-15T09:30:00Z' };",
            None,
        )
        .unwrap();

    let change = within(&feed, "a change");
    let Became::Written(Value::Object(held)) = change.became else {
        panic!("not a write");
    };
    assert!(matches!(
        held.get("owed"),
        Some(Value::Number(tessaridb::Number::Decimal(_)))
    ));
    assert!(matches!(held.get("waited"), Some(Value::Duration(_))));
    assert!(matches!(held.get("seen"), Some(Value::Datetime(_))));
}

#[test]
fn watching_a_table_that_is_not_there_is_refused_rather_than_silently_empty() {
    // A subscription to a name nobody defined delivers nothing forever, which
    // looks exactly like a subscription to a quiet table.
    let db = Arc::new(Db::in_memory().unwrap());
    let (_node, address) = serving(Arc::clone(&db));
    Client::connect(&address).unwrap().run(READY, None).unwrap();

    // Read directly rather than through `feeding`: the node refuses before it
    // pushes anything, so there is nothing to wait for.
    let mut feed = selected(&address)
        .follow(&Follow {
            from: 0,
            table: Some("nonesuch".to_owned()),
        })
        .unwrap();
    let refused = feed.wait().expect_err("a refusal");
    assert!(refused.to_string().contains("nonesuch"), "{refused}");
}

#[test]
fn watching_a_table_before_a_database_is_selected_says_so() {
    let db = Arc::new(Db::in_memory().unwrap());
    let (_node, address) = serving(Arc::clone(&db));
    Client::connect(&address).unwrap().run(READY, None).unwrap();

    // No `USE` on this connection, which is the point.
    let mut feed = Client::connect(&address)
        .unwrap()
        .follow(&Follow {
            from: 0,
            table: Some("users".to_owned()),
        })
        .unwrap();
    let refused = feed.wait().expect_err("a refusal");
    assert!(refused.to_string().contains("no database"), "{refused}");
}

#[test]
fn a_node_with_a_subscriber_still_answers_everybody_else() {
    // A thread per connection is what buys this, and it is worth asserting
    // rather than assuming: a feed that held anything shared would stop the
    // node the moment somebody subscribed.
    let db = Arc::new(Db::in_memory().unwrap());
    let (_node, address) = serving(Arc::clone(&db));
    let mut writer = Client::connect(&address).unwrap();
    writer.run(READY, None).unwrap();

    let _feed = selected(&address)
        .follow(&Follow {
            from: 0,
            table: None,
        })
        .unwrap();

    let answers = selected(&address)
        .run(
            "CREATE users:9 = { name: 'held' }; SELECT * FROM users:9;",
            None,
        )
        .unwrap();
    assert_eq!(answers.len(), 2);
}

#[test]
fn a_closed_store_refuses_an_unauthenticated_subscription() {
    // A subscription reads records without going anywhere near the executor, so
    // "who may read" has to be asked here or not at all — and not at all means
    // an anonymous connection receiving every write on a store that refuses it a
    // `SELECT`.
    let db = Arc::new(Db::in_memory().unwrap());
    let (_node, address) = serving(Arc::clone(&db));
    let mut owner = Client::connect(&address).unwrap();
    owner.run(READY, None).unwrap();
    owner
        .run("DEFINE USER ada ROLE owner PASSWORD 'a long one';", None)
        .unwrap();

    let mut refused = Client::connect(&address)
        .unwrap()
        .follow(&Follow {
            from: 0,
            table: None,
        })
        .unwrap();
    let said = refused.wait().expect_err("a refusal");
    assert!(!said.to_string().is_empty(), "the refusal said nothing");

    // And the same connection, signed in, is allowed.
    let mut signed = Client::connect(&address).unwrap();
    signed
        .run(
            "USE NAMESPACE prod; USE DATABASE orders;",
            Some(("ada", "a long one")),
        )
        .unwrap();
    let feed = feeding(
        signed,
        &Follow {
            from: 0,
            table: None,
        },
    );
    // The store is closed now, so even the connection that closed it signs in.
    owner
        .run(
            "CREATE users:1 = { name: 'ada' };",
            Some(("ada", "a long one")),
        )
        .unwrap();
    assert_eq!(
        within(&feed, "a change for the signed-in subscriber").id,
        "1"
    );
}

#[test]
fn a_subscription_is_confined_to_the_database_the_session_selected() {
    // The log is every tenancy's. A subscription that did not confine itself
    // would hand a caller every write in the store — which is a great deal more
    // than the database they said `USE` for.
    let db = Arc::new(Db::in_memory().unwrap());
    let (_node, address) = serving(Arc::clone(&db));
    let mut writer = Client::connect(&address).unwrap();
    writer.run(READY, None).unwrap();
    writer
        .run(
            "DEFINE DATABASE elsewhere; USE DATABASE elsewhere; DEFINE COLLECTION secrets;",
            None,
        )
        .unwrap();

    // Watching everything, from a session that selected `orders`.
    let feed = feeding(
        selected(&address),
        &Follow {
            from: db
                .committed_tail(db.store().own_log(FIXTURE_HOME).unwrap())
                .unwrap()
                .get()
                + 1,
            table: None,
        },
    );

    // The write to the other database comes first, so if the confinement did
    // nothing this is what would arrive.
    writer
        .run(
            "USE DATABASE elsewhere; CREATE secrets:1 = { held: 'not yours' };",
            None,
        )
        .unwrap();
    writer
        .run(
            "USE DATABASE orders; CREATE users:1 = { name: 'ada' };",
            None,
        )
        .unwrap();

    let change = within(&feed, "a change from the selected database");
    assert_eq!(change.table, "users");
}

#[test]
fn following_before_a_database_is_selected_says_so_even_watching_everything() {
    // "Everything" has to mean everything in *a* database, and a session that
    // has not said which cannot be given one.
    let db = Arc::new(Db::in_memory().unwrap());
    let (_node, address) = serving(Arc::clone(&db));
    Client::connect(&address).unwrap().run(READY, None).unwrap();

    let mut feed = Client::connect(&address)
        .unwrap()
        .follow(&Follow {
            from: 0,
            table: None,
        })
        .unwrap();
    let refused = feed.wait().expect_err("a refusal");
    assert!(refused.to_string().contains("no database"), "{refused}");
}

#[test]
#[ignore = "waits out the node's write timeout, which is 30 seconds by design"]
fn a_subscriber_that_stops_reading_is_cut_off_rather_than_buffered_and_loses_nothing() {
    // The backpressure story, observed rather than claimed, in both halves.
    //
    // A client that stops reading fills its socket; the node's write blocks; the
    // write timeout ends *that* connection. So the feed it comes back to read
    // **ends early** rather than delivering everything — which is the proof that
    // nothing was buffered here on its behalf.
    //
    // And nothing is lost by that, because the log is the buffer: a second
    // subscription starting one past the last change it did receive gets the
    // rest. That is the half that makes the first half acceptable.
    //
    // Ignored because it waits out a timeout that is deliberately long. Making
    // it short enough for a suite would mean a knob whose only caller is a test.
    // Run with `cargo test -p tessari-wire -- --ignored`.
    const WRITTEN: usize = 10_000;

    let db = Arc::new(Db::in_memory().unwrap());
    let (_node, address) = serving(Arc::clone(&db));
    let mut writer = Client::connect(&address).unwrap();
    writer.run(READY, None).unwrap();

    let mut deaf = selected(&address)
        .follow(&Follow {
            from: 0,
            table: None,
        })
        .unwrap();

    for batch in 0..(WRITTEN / 250) {
        let mut script = String::new();
        for held in 0..250 {
            script.push_str(&format!(
                "CREATE users:{} = {{ padding: '{}' }};",
                batch * 250 + held,
                "x".repeat(400)
            ));
        }
        writer.run(&script, None).unwrap();
    }

    // Long enough for the node to fill the socket, block, and give up.
    std::thread::sleep(Duration::from_secs(35));

    // Now read what it managed to send. It ends, and it ends short.
    let mut taken = 0;
    let mut last = 0;
    while let Ok(Some(change)) = deaf.wait() {
        taken += 1;
        last = change.sequence;
    }
    assert!(taken > 0, "the node sent nothing at all");
    assert!(
        taken < WRITTEN,
        "all {WRITTEN} arrived, so something buffered them"
    );

    // And the rest is still there, which is why cutting it off was allowed.
    let resumed = feeding(
        selected(&address),
        &Follow {
            from: last + 1,
            table: None,
        },
    );
    let next = within(&resumed, "the change after the last one delivered");
    assert!(
        next.sequence > last,
        "{} is not after {last}",
        next.sequence
    );

    // The node kept serving everybody else throughout.
    let answers = selected(&address)
        .run("SELECT * FROM users:1;", None)
        .unwrap();
    assert_eq!(answers.len(), 1);
}

#[test]
fn a_grant_governed_subscriber_is_told_only_about_tables_it_was_granted() {
    // A subscription reaches records without running a statement, so grants have
    // to be asked here or not at all — and not at all means a feed handing
    // somebody a table nobody granted them. This surface has produced that shape
    // of hole twice; the test is what makes the third time a failure rather than
    // a discovery.
    let db = Arc::new(Db::in_memory().unwrap());
    let (_node, address) = serving(Arc::clone(&db));
    let mut open = Client::connect(&address).unwrap();
    open.run(READY, None).unwrap();
    open.run(
        "DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        None,
    )
    .unwrap();

    let owner = Some(("root", "correct horse battery"));
    let mut root = Client::connect(&address).unwrap();
    root.run("USE NAMESPACE prod; USE DATABASE orders;", owner)
        .unwrap();
    root.run(
        "DEFINE USER ada ON prod.orders ROLE editor PASSWORD 'correct horse battery';",
        owner,
    )
    .unwrap();
    root.run("GRANT read, write ON orders TO ada;", owner)
        .unwrap();

    let scoped = Some(("ada", "correct horse battery"));
    let mut ada = Client::connect(&address).unwrap();
    ada.run("USE NAMESPACE prod; USE DATABASE orders;", scoped)
        .unwrap();
    let feed = feeding(
        ada,
        &Follow {
            from: db
                .committed_tail(db.store().own_log(FIXTURE_HOME).unwrap())
                .unwrap()
                .get()
                + 1,
            table: None,
        },
    );

    // The ungranted table is written to first, so a feed that filtered nothing
    // would deliver it.
    root.run("CREATE users:1 = { name: 'not yours' };", owner)
        .unwrap();
    root.run("CREATE orders:1 = { total: 3 };", owner).unwrap();

    let change = within(&feed, "a change from the granted table");
    assert_eq!(change.table, "orders");

    // And naming the ungranted table outright is refused rather than silent.
    let mut asking = Client::connect(&address).unwrap();
    asking
        .run("USE NAMESPACE prod; USE DATABASE orders;", scoped)
        .unwrap();
    let mut refused = asking
        .follow(&Follow {
            from: 0,
            table: Some("users".to_owned()),
        })
        .unwrap();
    let said = refused.wait().expect_err("a refusal");
    assert!(said.to_string().contains("granted"), "{said}");
}

#[test]
fn a_field_grant_reaches_the_feed_too() {
    // The feed pushes whole records and never passes through a session's read
    // path, so a field grant reaches it here or not at all. This surface has
    // produced that class of hole twice; the test is what makes a third a
    // failure rather than a discovery.
    let db = Arc::new(Db::in_memory().unwrap());
    let (_node, address) = serving(Arc::clone(&db));
    let mut open = Client::connect(&address).unwrap();
    open.run(READY, None).unwrap();
    open.run(
        "DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        None,
    )
    .unwrap();

    let owner = Some(("root", "correct horse battery"));
    let mut root = Client::connect(&address).unwrap();
    root.run("USE NAMESPACE prod; USE DATABASE orders;", owner)
        .unwrap();
    root.run(
        "DEFINE USER ada ON prod.orders ROLE editor PASSWORD 'correct horse battery';",
        owner,
    )
    .unwrap();
    root.run("GRANT read ON users FIELDS name TO ada;", owner)
        .unwrap();

    let scoped = Some(("ada", "correct horse battery"));
    let mut ada = Client::connect(&address).unwrap();
    ada.run("USE NAMESPACE prod; USE DATABASE orders;", scoped)
        .unwrap();
    let feed = feeding(
        ada,
        &Follow {
            from: db
                .committed_tail(db.store().own_log(FIXTURE_HOME).unwrap())
                .unwrap()
                .get()
                + 1,
            table: Some("users".to_owned()),
        },
    );

    root.run("CREATE users:1 = { name: 'ada', salary: 120000 };", owner)
        .unwrap();

    let change = within(&feed, "a change from the granted table");
    let Became::Written(Value::Object(held)) = change.became else {
        panic!("not a write");
    };
    assert!(held.contains_key("name"), "{held:?}");
    assert!(!held.contains_key("salary"), "{held:?}");
}
