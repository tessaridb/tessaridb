//! G069 C3 — a subscription narrowed by a condition (ADR-0122 Part B).
//!
//! A mirror that applies a narrowed feed must end up holding exactly the
//! records that match: so a write that matches arrives, a record that stops
//! matching arrives as a removal, and nothing else does. The condition is
//! judged on what the subscriber may see, and a feed that skipped changes says
//! how far it read, so resuming neither loses nor repeats.

use std::time::Instant;

use tessaridb::feed::{Condition, Feed, FeedRefused, Following, PATIENCE_BETWEEN_ROUNDS, Round};
use tessaridb::{ChangeKind, Db, Parameters, Sequence, Value};

const PASSWORD: &str = "correct horse battery";

/// One delivered change: its sequence, its record and what it became.
type Given = (u64, String, Option<Value>);

fn chat(name: &str) -> Parameters {
    Parameters::from([("chat".to_owned(), Value::String(name.to_owned()))])
}

fn ready() -> Db {
    let db = Db::in_memory().unwrap();
    db.session()
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION msgs;",
        )
        .unwrap();
    db
}

fn session(db: &Db) -> tessaridb::Session<'_> {
    let mut session = db.session();
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

fn narrowed<'a>(from: u64, text: &'a str, parameters: &'a Parameters) -> Following<'a> {
    Following {
        from: Sequence::new(from),
        table: Some("msgs"),
        cursor: None,
        condition: Some(Condition { text, parameters }),
    }
}

/// Run rounds until the log is read to its end; what was given, and the
/// progress the feed offers a moment later.
fn drained(
    db: &Db,
    session: &mut tessaridb::Session<'_>,
    feed: &mut Feed,
) -> (Vec<Given>, Option<u64>) {
    let mut given = Vec::new();
    loop {
        let round = feed
            .round(db, session, &mut |change, _, allowed, _| {
                // As every surface does: what is sent is what may be seen.
                let became = match &change.kind {
                    ChangeKind::Written(value) => Some(tessaridb::seen(value.clone(), allowed)),
                    ChangeKind::Removed => None,
                };
                given.push((change.sequence.get(), change.id.to_string(), became));
                true
            })
            .unwrap();
        if round == Round::Empty {
            break;
        }
    }
    let later = Instant::now().checked_add(PATIENCE_BETWEEN_ROUNDS).unwrap();
    let progress = feed.progress(later).map(|at| at.sequence.get());
    (given, progress)
}

fn ids(given: &[Given]) -> Vec<(String, bool)> {
    given
        .iter()
        .map(|(_, id, became)| (id.clone(), became.is_some()))
        .collect()
}

#[test]
fn a_narrowed_feed_delivers_the_matching_writes_and_nothing_else() {
    let db = ready();
    let mut writer = session(&db);
    writer
        .run(
            "CREATE msgs:1 = { chat: 'a', text: 'hi' }; CREATE msgs:2 = { chat: 'b', text: 'no' };\n\
             CREATE msgs:3 = { chat: 'a', text: 'yo' };",
        )
        .unwrap();
    let mut reader = session(&db);
    let asked = chat("a");
    let mut feed = Feed::open(&db, &mut reader, &narrowed(0, "chat = $chat", &asked)).unwrap();
    let (given, _) = drained(&db, &mut reader, &mut feed);
    assert_eq!(
        ids(&given),
        [("1".to_owned(), true), ("3".to_owned(), true)]
    );
}

#[test]
fn a_record_that_leaves_the_condition_arrives_as_a_removal_and_only_once() {
    let db = ready();
    let mut writer = session(&db);
    writer
        .run(
            "CREATE msgs:1 = { chat: 'a' }; UPDATE msgs:1 SET chat = 'b'; DELETE msgs:1;\n\
             CREATE msgs:2 = { chat: 'a' }; DELETE msgs:2;\n\
             CREATE msgs:3 = { chat: 'b' }; UPDATE msgs:3 SET chat = 'c'; DELETE msgs:3;",
        )
        .unwrap();
    let mut reader = session(&db);
    let asked = chat("a");
    let mut feed = Feed::open(&db, &mut reader, &narrowed(0, "chat = $chat", &asked)).unwrap();
    let (given, _) = drained(&db, &mut reader, &mut feed);
    // msgs:1 matched, left (a removal), then its delete is about a record the
    // mirror no longer holds; msgs:2 was removed while matching; msgs:3 never
    // matched, so nothing about it is said at all.
    assert_eq!(
        ids(&given),
        [
            ("1".to_owned(), true),
            ("1".to_owned(), false),
            ("2".to_owned(), true),
            ("2".to_owned(), false),
        ]
    );
}

#[test]
fn a_narrowed_feed_says_how_far_it_read_and_resumes_without_loss_or_repeat() {
    let db = ready();
    let mut writer = session(&db);
    writer
        .run("CREATE msgs:1 = { chat: 'a' }; CREATE msgs:2 = { chat: 'b' }; CREATE msgs:3 = { chat: 'b' };")
        .unwrap();
    let mut reader = session(&db);
    let asked = chat("a");
    let mut feed = Feed::open(&db, &mut reader, &narrowed(0, "chat = $chat", &asked)).unwrap();
    let (first, progress) = drained(&db, &mut reader, &mut feed);
    assert_eq!(ids(&first), [("1".to_owned(), true)]);
    // It skipped msgs:2 and msgs:3 after its last delivery, so it says where
    // it reached: the sequence of the last change it read.
    let last_delivered = first.last().map(|(at, _, _)| *at).unwrap();
    let reached = progress.expect("a feed that skipped changes reports progress");
    assert!(reached > last_delivered, "{reached} <= {last_delivered}");
    // Nothing new: no progress repeated.
    let (none, again) = drained(&db, &mut reader, &mut feed);
    assert!(none.is_empty() && again.is_none(), "{none:?} {again:?}");

    writer
        .run("CREATE msgs:4 = { chat: 'a' }; CREATE msgs:5 = { chat: 'a' };")
        .unwrap();
    let mut resumed = Feed::open(
        &db,
        &mut reader,
        &narrowed(reached.checked_add(1).unwrap(), "chat = $chat", &asked),
    )
    .unwrap();
    let (rest, _) = drained(&db, &mut reader, &mut resumed);
    // The first change after the progress point is the first one resumed.
    assert_eq!(ids(&rest), [("4".to_owned(), true), ("5".to_owned(), true)]);
}

#[test]
fn progress_is_offered_only_once_the_feed_has_been_quiet_for_a_while() {
    let db = ready();
    let mut writer = session(&db);
    writer
        .run("CREATE msgs:1 = { chat: 'a' }; CREATE msgs:2 = { chat: 'b' };")
        .unwrap();
    let mut reader = session(&db);
    let asked = chat("a");
    let mut feed = Feed::open(&db, &mut reader, &narrowed(0, "chat = $chat", &asked)).unwrap();
    let opened = Instant::now();
    while feed
        .round(&db, &mut reader, &mut |_, _, _, _| true)
        .unwrap()
        != Round::Empty
    {}
    // Just delivered msgs:1: the subscriber already holds a position.
    assert!(feed.progress(opened).is_none());
}

#[test]
fn a_condition_on_a_field_the_subscriber_cannot_see_is_refused_at_open() {
    let db = ready();
    let mut owner = db.session();
    owner
        .run("DEFINE USER root ROLE owner PASSWORD 'correct horse battery';")
        .unwrap();
    let mut root = session(&db);
    root.sign_in("root", PASSWORD).unwrap();
    root.run(
        "USE NAMESPACE prod; USE DATABASE shop;\n\
         DEFINE USER narrow ON NAMESPACE prod AUTHORITIES read PASSWORD 'correct horse battery';\n\
         GRANT read ON msgs FIELDS chat TO narrow;\n\
         CREATE msgs:1 = { chat: 'a', secret: 1 };",
    )
    .unwrap();
    let mut narrow = db.session();
    narrow.sign_in("narrow", PASSWORD).unwrap();
    narrow
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    let none = Parameters::new();
    let refused = Feed::open(&db, &mut narrow, &narrowed(0, "secret = 1", &none)).err();
    assert!(
        matches!(&refused, Some(FeedRefused::FieldNotVisible { field }) if field == "secret"),
        "{refused:?}"
    );
    // A visible field opens, and what arrives is what the subscriber may see.
    let asked = chat("a");
    let mut feed = Feed::open(&db, &mut narrow, &narrowed(0, "chat = $chat", &asked)).unwrap();
    let (given, _) = drained(&db, &mut narrow, &mut feed);
    let [(_, _, Some(Value::Object(fields)))] = given.as_slice() else {
        panic!("{given:?}");
    };
    assert!(!fields.contains_key("secret"), "{fields:?}");
}

#[test]
fn a_feed_whose_condition_field_is_hidden_while_it_runs_ends() {
    let db = ready();
    let mut owner = db.session();
    owner
        .run("DEFINE USER root ROLE owner PASSWORD 'correct horse battery';")
        .unwrap();
    let mut root = session(&db);
    root.sign_in("root", PASSWORD).unwrap();
    root.run(
        "USE NAMESPACE prod; USE DATABASE shop;\n\
         DEFINE USER narrow ON NAMESPACE prod AUTHORITIES read PASSWORD 'correct horse battery';\n\
         GRANT read ON msgs FIELDS chat, text TO narrow;",
    )
    .unwrap();
    let mut narrow = db.session();
    narrow.sign_in("narrow", PASSWORD).unwrap();
    narrow
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    let asked = chat("a");
    let mut feed = Feed::open(&db, &mut narrow, &narrowed(0, "chat = $chat", &asked)).unwrap();
    root.run(
        // A grant says what the result is: this one takes `chat` away.
        "GRANT read ON msgs FIELDS text TO narrow;\n\
         CREATE msgs:1 = { chat: 'a', text: 'hi' };",
    )
    .unwrap();
    let ended = feed.round(&db, &mut narrow, &mut |_, _, _, _| true);
    assert!(
        matches!(&ended, Err(FeedRefused::FieldNotVisible { field }) if field == "chat"),
        "{ended:?}"
    );
}

#[test]
fn a_condition_is_refused_when_it_cannot_be_judged_from_the_change() {
    let db = ready();
    let mut reader = session(&db);
    let none = Parameters::new();
    let reads = Feed::open(
        &db,
        &mut reader,
        &narrowed(0, "chat IN (SELECT chat FROM msgs)", &none),
    )
    .err();
    assert!(
        matches!(reads, Some(FeedRefused::ConditionReadsTheStore)),
        "{reads:?}"
    );
    let everything = Feed::open(
        &db,
        &mut reader,
        &Following {
            from: Sequence::new(0),
            table: None,
            cursor: None,
            condition: Some(Condition {
                text: "chat = 'a'",
                parameters: &none,
            }),
        },
    )
    .err();
    assert!(
        matches!(everything, Some(FeedRefused::ConditionWithoutTable)),
        "{everything:?}"
    );
    let unbound = Feed::open(&db, &mut reader, &narrowed(0, "chat = $chat", &none)).err();
    assert!(
        matches!(
            &unbound,
            Some(FeedRefused::Store(tessaridb::Error::Script(
                tessari_ql::Error::UnboundParameter { name, .. }
            ))) if name == "chat"
        ),
        "{unbound:?}"
    );
}

#[test]
fn a_bound_value_is_compared_and_never_read_as_syntax() {
    let db = ready();
    let mut writer = session(&db);
    writer
        .run(
            "CREATE msgs:1 = { chat: 'a' }; CREATE msgs:2 = { chat: \"a' OR true OR chat = 'z\" };",
        )
        .unwrap();
    let mut reader = session(&db);
    let asked = chat("a' OR true OR chat = 'z");
    let mut feed = Feed::open(&db, &mut reader, &narrowed(0, "chat = $chat", &asked)).unwrap();
    let (given, _) = drained(&db, &mut reader, &mut feed);
    assert_eq!(ids(&given), [("2".to_owned(), true)]);
}

#[test]
fn a_record_created_with_a_generated_identity_reaches_the_feed() {
    let db = ready();
    let mut writer = session(&db);
    writer
        .run("CREATE msgs = { chat: 'a' }; CREATE msgs:7 = { chat: 'a' };")
        .unwrap();
    let mut reader = session(&db);
    let mut feed = Feed::open(
        &db,
        &mut reader,
        &Following {
            from: Sequence::new(0),
            table: Some("msgs"),
            cursor: None,
            condition: None,
        },
    )
    .unwrap();
    let (given, _) = drained(&db, &mut reader, &mut feed);
    assert_eq!(given.len(), 2, "{given:?}");
}
