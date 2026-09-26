//! A subscription answers to the authority it is *currently* reading on.
//!
//! # Why this is a test and not a comment
//!
//! Because the failure it guards against has no symptom. A feed that resolved
//! its permissions once and then pushed for an hour looks exactly like a feed
//! that is entitled to: records arrive, nothing errors, and the operator who
//! ran the `REVOKE` has every reason to believe it worked — the same user's
//! `SELECT` is refused a moment later, so the half they can see agrees with
//! them.
//!
//! The test therefore holds a subscription **open across** the revocation, and
//! requires the feed itself to end. Revoking first and subscribing afterwards
//! would pass with the defect present.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::cell::Cell;

use tessaridb::feed::{Commits, Following, follow};
use tessaridb::{Db, Sequence};

const PASSWORD: &str = "correct horse battery";

/// How many rounds the feed is allowed before the test gives up on it.
///
/// A bound rather than a wait: if the revocation never reaches the loop this
/// ends the test with a failed assertion instead of hanging a suite forever.
/// Rounds are only slow when nothing is happening, and something is.
const ROUNDS_ALLOWED: u32 = 20;

#[test]
fn revoking_the_read_ends_a_subscription_that_is_already_running() {
    let db = Db::in_memory().unwrap();
    let mut owner = db.session();
    owner
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION orders;\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();
    let mut root = db.session();
    root.sign_in("root", PASSWORD).unwrap();
    root.run("USE NAMESPACE prod; USE DATABASE shop;").unwrap();
    root.run(
        "DEFINE USER kim ON NAMESPACE prod AUTHORITIES read PASSWORD 'correct horse battery';",
    )
    .unwrap();
    root.run("CREATE orders:1 = { total: 5 };").unwrap();

    let mut kim = db.session();
    kim.sign_in("kim", PASSWORD).unwrap();
    kim.run("USE NAMESPACE prod; USE DATABASE shop;").unwrap();

    let rounds = Cell::new(0_u32);
    let delivered = Cell::new(0_u32);
    let taken = Cell::new(None);
    let committed = Commits::default();
    let following = Following {
        from: Sequence::new(0),
        table: None,
        cursor: None,
    };

    let outcome = follow(
        &db,
        &mut kim,
        &following,
        &committed,
        &|| {
            rounds.set(rounds.get().saturating_add(1));
            rounds.get() > ROUNDS_ALLOWED
        },
        &mut |_change, _name, _allowed, _cursor| {
            // The revocation happens *inside* the feed, on the first change it
            // pushes, so the subscription is unambiguously already running when
            // the authority goes away.
            if delivered.get() == 0 {
                let mut taking = db.session();
                taking.sign_in("root", PASSWORD).unwrap();
                taking
                    .run("USE NAMESPACE prod; USE DATABASE shop; REVOKE read ON NAMESPACE prod FROM kim;")
                    .unwrap();
                taken.set(Some(std::time::Instant::now()));
            }
            delivered.set(delivered.get().saturating_add(1));
            true
        },
    );

    assert!(delivered.get() >= 1, "the feed pushed nothing to revoke");
    let refusal = outcome.expect_err("the revocation did not reach the running feed");
    assert!(refusal.contains("read"), "{refusal}");
    assert!(
        rounds.get() <= ROUNDS_ALLOWED,
        "the feed ended because the test gave up, not because the read was revoked"
    );

    // The published bound for this point. It is one poll round, and a round is
    // at most the quarter of a second the feed blocks for when nothing is
    // happening — this measures the busy case, where the loop comes round at
    // once. Printed rather than only asserted: the number is what goes in the
    // readiness checklist.
    let bound = taken
        .get()
        .expect("the revocation was never taken")
        .elapsed();
    println!("revocation → running feed ended: {bound:?}");
    assert!(
        bound < std::time::Duration::from_secs(5),
        "one poll round took {bound:?}"
    );
}

#[test]
fn a_subscription_is_never_told_about_a_tenancy_the_session_could_not_select() {
    // The crossing at the feed. It is not expressible as "ask for the other
    // tenancy", because a scoped session cannot select one — so the crossing a
    // feed can actually make is to be *handed* it: the change log is the whole
    // store's, and a subscription that did not confine itself would push every
    // write in every namespace to whoever was watching.
    let db = Db::in_memory().unwrap();
    let mut owner = db.session();
    owner
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION orders;\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();
    let mut root = db.session();
    root.sign_in("root", PASSWORD).unwrap();
    root.run(
        "DEFINE NAMESPACE staging; USE NAMESPACE staging; DEFINE DATABASE shop; \
         USE DATABASE shop; DEFINE COLLECTION orders;",
    )
    .unwrap();
    root.run(
        "USE NAMESPACE prod; USE DATABASE shop; \
         DEFINE USER nina ON prod.shop ROLE owner PASSWORD 'correct horse battery';",
    )
    .unwrap();

    // One write in each tenancy, the other one first, so a feed that ignored
    // the tenancy would deliver it before the record nina is entitled to.
    root.run("USE NAMESPACE staging; USE DATABASE shop; CREATE orders:1 = { total: 9 };")
        .unwrap();
    root.run("USE NAMESPACE prod; USE DATABASE shop; CREATE orders:1 = { total: 5 };")
        .unwrap();

    let mut nina = db.session();
    nina.sign_in("nina", PASSWORD).unwrap();
    nina.run("USE NAMESPACE prod; USE DATABASE shop;").unwrap();

    let mine = db.tenancy_in("prod", "shop").unwrap().expect("her tenancy");
    let seen = Cell::new(Vec::new());
    let rounds = Cell::new(0_u32);
    let committed = Commits::default();
    let following = Following {
        from: Sequence::new(0),
        table: None,
        cursor: None,
    };
    drop(follow(
        &db,
        &mut nina,
        &following,
        &committed,
        &|| {
            rounds.set(rounds.get().saturating_add(1));
            // Two rounds: the first drains what is already in the log, the
            // second is where anything wrongly queued would still arrive.
            rounds.get() > 2
        },
        &mut |change, _name, _allowed, _cursor| {
            let mut held = seen.take();
            held.push((change.namespace, change.database));
            seen.set(held);
            true
        },
    ));

    let delivered = seen.take();
    assert!(
        !delivered.is_empty(),
        "the feed delivered nothing at all, so it proves nothing"
    );
    for reached in &delivered {
        assert_eq!(
            *reached, mine,
            "a write from another tenancy reached the subscriber"
        );
    }
}

/// What a feed delivered: each change's record, its `total` (or `gone`) and the
/// cursor it carried, over at most three rounds.
fn fed(
    db: &Db,
    session: &mut tessaridb::Session<'_>,
    from: u64,
    cursor: Option<&str>,
    table: Option<&str>,
) -> Result<Vec<(String, String, Option<String>)>, String> {
    let rounds = Cell::new(0_u32);
    let mut given = Vec::new();
    follow(
        db,
        session,
        &Following {
            from: Sequence::new(from),
            table,
            cursor,
        },
        &Commits::default(),
        &|| {
            rounds.set(rounds.get().saturating_add(1));
            rounds.get() > 3
        },
        &mut |change, _, _, cursor| {
            let total = match &change.kind {
                tessaridb::ChangeKind::Written(tessaridb::Value::Object(fields)) => {
                    format!("{:?}", fields.get("total"))
                }
                _ => "gone".to_owned(),
            };
            given.push((change.id.to_string(), total, cursor.map(str::to_owned)));
            true
        },
    )?;
    Ok(given)
}

/// A feed over a split table follows its shards' logs and its database's, in
/// the order the writer committed them, and resumes from the cursor its last
/// change carried with nothing lost and nothing given twice (G037 S7.1, Q-791).
///
/// Single-shard commits land in shard logs and the two-shard one in the
/// database's, so read a log at a time the `total: 3`/`total: 9` commit would
/// arrive before or after both shard writes rather than between them.
#[test]
fn a_feed_over_a_split_table_merges_its_logs_in_commit_order_and_resumes_from_its_cursor() {
    let db = Db::in_memory().unwrap();
    let mut session = db.session();
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE orders (total int) IDENTITY uuid SPLIT AT 'g';\n\
             DEFINE COLLECTION notes;\n\
             CREATE orders:'h' = { total: 1 }; CREATE orders:'a' = { total: 0 };\n\
             BEGIN; UPDATE orders:'h' MERGE { total: 3 }; UPDATE orders:'a' MERGE { total: 9 }; COMMIT;\n\
             CREATE notes:1 = { total: 5 }; DELETE orders:'a';",
        )
        .unwrap();
    let given = fed(&db, &mut session, 0, None, Some("orders")).unwrap();
    let seen: Vec<(&str, &str)> = given
        .iter()
        .map(|(id, total, _)| (id.as_str(), total.as_str()))
        .collect();
    let (one, three, nine) = (
        "Some(Number(Integer(1)))",
        "Some(Number(Integer(3)))",
        "Some(Number(Integer(9)))",
    );
    let zero = "Some(Number(Integer(0)))";
    assert_eq!(
        seen,
        vec![
            ("h", one),
            ("a", zero),
            ("a", nine),
            ("h", three),
            ("a", "gone")
        ],
        "the writer's order"
    );
    assert!(
        given.iter().all(|(_, _, cursor)| cursor.is_some()),
        "every change carries its cursor"
    );

    // Resume after the two-shard commit, then write more: exactly what came
    // after it arrives, once.
    let after = given[3].2.clone().unwrap();
    session
        .run("CREATE orders:'b' = { total: 7 }; CREATE orders:'q' = { total: 8 };")
        .unwrap();
    let resumed = fed(&db, &mut session, 0, Some(&after), Some("orders")).unwrap();
    let ids: Vec<&str> = resumed.iter().map(|(id, _, _)| id.as_str()).collect();
    assert_eq!(ids, vec!["a", "b", "q"], "resumed from {after}");

    // Watching everything follows the same logs and the unsplit table's changes
    // in the database log beside them.
    let all = fed(&db, &mut session, 0, None, None).unwrap();
    assert_eq!(all.len(), 8, "{all:?}");
    assert!(
        all.iter().any(|(id, _, _)| id == "1"),
        "the unsplit table still feeds"
    );

    // A cursor this feed never carried is refused by name, not resumed from.
    let refusal = fed(&db, &mut session, 0, Some("d=x"), Some("orders")).unwrap_err();
    assert!(refusal.contains("is not a cursor"), "{refusal}");

    // A feed over the unsplit table is the single-log feed it always was: no
    // cursor on its changes.
    let notes = fed(&db, &mut session, 0, None, Some("notes")).unwrap();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].2.is_none());

    // A cursor is its own feed's. Sent to a feed over another split table, or to
    // one over an unsplit table, it is refused rather than resumed from a guess.
    session
        .run("DEFINE TABLE lines (total int) IDENTITY uuid SPLIT AT 'm';")
        .unwrap();
    let refusal = fed(&db, &mut session, 0, Some(&after), Some("lines")).unwrap_err();
    assert!(
        refusal.contains("counts a log this feed does not follow"),
        "{refusal}"
    );
    let refusal = fed(&db, &mut session, 0, Some(&after), Some("notes")).unwrap_err();
    assert!(
        refusal.contains("this feed follows no split table"),
        "{refusal}"
    );
}

/// A split table's log holding another node's writes is refused by name: one
/// writer's order is the only order there is to merge by, and a shard led
/// elsewhere is not in this node's logs at all.
#[test]
fn a_split_feed_over_another_writers_log_is_refused_by_name() {
    let db = Db::in_memory().unwrap();
    let mut session = db.session();
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE orders (total int) IDENTITY uuid SPLIT AT 'g';",
        )
        .unwrap();
    let (namespace, database) = db.tenancy_in("prod", "shop").unwrap().unwrap();
    let table = db.table_in("prod", "shop", "orders").unwrap().unwrap();
    let elsewhere = tessaridb::LogId::new(
        tessaridb::Reach::Shard(namespace, database, table, tessari_types::ShardId::new(2)),
        tessaridb::Writer::new([7; 16]),
    );
    db.store()
        .apply_from_stream(
            elsewhere,
            Sequence::new(1),
            tessari_types::Epoch::ZERO,
            &tessari_encoding::LogRecord::at(tessari_types::Epoch::new(1), Vec::new()),
        )
        .unwrap();
    let refusal = fed(&db, &mut session, 0, None, Some("orders")).unwrap_err();
    assert!(
        refusal.contains("shard 2 of `orders` holds another node's writes"),
        "{refusal}"
    );
}

/// A split table declared while a feed over its database is running would write
/// into shard logs the feed is not reading, so the feed ends with a refusal
/// rather than going quietly deaf to that table (Q-791).
#[test]
fn a_feed_ends_when_a_split_table_appears_in_its_scope() {
    let db = Db::in_memory().unwrap();
    let mut session = db.session();
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION notes; CREATE notes:1 = { total: 5 };",
        )
        .unwrap();
    let rounds = Cell::new(0_u32);
    let delivered = Cell::new(0_u32);
    let ended = follow(
        &db,
        &mut session,
        &Following {
            from: Sequence::new(0),
            table: None,
            cursor: None,
        },
        &Commits::default(),
        &|| {
            rounds.set(rounds.get().saturating_add(1));
            rounds.get() > 3
        },
        &mut |_, _, _, _| {
            if delivered.replace(delivered.get().saturating_add(1)) == 0 {
                let mut other = db.session();
                other
                    .run(
                        "USE NAMESPACE prod; USE DATABASE shop;\n\
                         DEFINE TABLE orders (total int) IDENTITY uuid SPLIT AT 'g';",
                    )
                    .unwrap();
            }
            true
        },
    );
    assert!(
        delivered.get() > 0,
        "the feed delivered nothing, so it proves nothing"
    );
    let refusal = ended.unwrap_err();
    assert!(refusal.contains("was split after it began"), "{refusal}");
}
