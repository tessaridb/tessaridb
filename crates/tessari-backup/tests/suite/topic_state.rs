//! A topic's positions and append instants survive a state copy (ADR-0133 D3,
//! Q-G079-12).
//!
//! A position is a fact a reader has already acted on — a Kafka consumer's
//! committed offset is one — so a restored or re-seeded store that numbered the
//! same messages differently would hand that reader other messages under the
//! numbers it holds. Positions are given in commit order and a state is read in
//! key order, so these topics are written with identities that sort in another
//! order than the one they were appended in, and with retention that has removed
//! the front: a copy that numbered again from what it holds gets both wrong.

use tessari_session::{RunNow, Session};
use tessari_storage::{Now, RestoredTopics, Store};
use tessari_types::{Number, Reach, Value};

use crate::restore::store;

/// The topic every case writes: small enough that its front is retained away.
const TOPIC: &str = "DEFINE NAMESPACE n; USE NAMESPACE n; DEFINE DATABASE d; USE DATABASE d; \
                     DEFINE TOPIC t RETAIN BYTES 60;";

/// Identities in the order they are appended — which is not the order they sort in.
const APPENDED: [&str; 8] = [
    "zulu", "alpha", "mike", "bravo", "yankee", "charlie", "xray", "delta",
];

/// Each message as `READ` answers it: identity, position, and when it was
/// appended — checked against the topic's records, since `READ` walks the
/// positions and a message holding none would simply not be answered.
fn messages(store: &Store) -> Vec<(String, u64, String)> {
    let mut session = Session::new(store);
    let answer = session
        .run_now("USE NAMESPACE n; USE DATABASE d; READ FROM t; SELECT * FROM t;")
        .unwrap();
    let held = answer[3].records().unwrap().len();
    assert_eq!(
        held,
        answer[2].records().unwrap().len(),
        "a message the topic holds has no position"
    );
    answer[2]
        .records()
        .unwrap()
        .iter()
        .map(|(id, body)| {
            let Value::Object(fields) = body else {
                panic!("a message answered {body:?}");
            };
            let Some(Value::Number(Number::Integer(position))) = fields.get("position") else {
                panic!("no position in {fields:?}");
            };
            (
                format!("{id:?}"),
                u64::try_from(*position).unwrap(),
                format!("{:?}", fields.get("appended")),
            )
        })
        .collect()
}

/// Append `ids` to the topic, one commit each.
fn append(store: &Store, ids: &[&str]) {
    let mut session = Session::new(store);
    session.run_now("USE NAMESPACE n; USE DATABASE d;").unwrap();
    for (n, id) in ids.iter().enumerate() {
        session
            .run_now(&format!("CREATE t:'{id}' = {{ n: {n} }};"))
            .unwrap();
    }
}

/// The position the next message appended to `store` is given.
fn next_position(store: &Store, id: &str) -> u64 {
    append(store, &[id]);
    let held = messages(store);
    let (_, position, _) = held
        .iter()
        .find(|(held, _, _)| held.contains(id))
        .unwrap_or_else(|| panic!("{id} is not in {held:?}"));
    *position
}

/// A source whose topic has given eight positions and retained the last few.
fn source() -> Store {
    let (_, source) = store();
    Session::new(&source).run_now(TOPIC).unwrap();
    append(&source, &APPENDED);
    source
}

/// The source's messages, checked to be the shape these cases need: the front
/// retained away and the survivors in append order, numbered densely to 8.
fn as_appended(source: &Store) -> Vec<(String, u64, String)> {
    let held = messages(source);
    let first = held.first().map_or(0, |(_, position, _)| *position);
    assert!(first > 1, "retention removed nothing: {held:?}");
    let ids: Vec<&str> = held
        .iter()
        .map(|(id, _, _)| {
            APPENDED
                .iter()
                .find(|appended| id.contains(**appended))
                .copied()
                .unwrap()
        })
        .collect();
    let skipped = usize::try_from(first.saturating_sub(1)).unwrap();
    assert_eq!(
        ids,
        APPENDED[skipped..],
        "the messages are not in append order"
    );
    let positions: Vec<u64> = held.iter().map(|(_, position, _)| *position).collect();
    assert_eq!(positions, (first..=8).collect::<Vec<_>>());
    assert!(
        held.iter()
            .all(|(_, _, appended)| appended.contains("Datetime")),
        "a message carries no append instant: {held:?}"
    );
    held
}

/// Copy `source`'s state onto `onto` the way a follower re-seeds, with
/// `between` run after the state's version is fixed and before the rest of
/// it — its index entries and topic heads — is read.
fn copied(source: &Store, onto: &Store, between: impl FnOnce()) {
    let before = onto.committed_version().now().unwrap();
    let mut reader = source.read_state_within(Reach::Store).now().unwrap();
    let positions = reader.positions().to_vec();
    between();
    while let Some(chunk) = reader.next_chunk(500).now().unwrap() {
        onto.restore_state_chunk(&chunk).now().unwrap();
    }
    let mut restored = RestoredTopics::default();
    loop {
        let entries = reader.topic_entries(3).now().unwrap();
        if entries.is_empty() {
            break;
        }
        onto.restore_topic_entries(&entries, &mut restored)
            .now()
            .unwrap();
    }
    let topics = restored.heads(reader.topic_heads().now().unwrap());
    drop(reader);
    onto.sweep_unreplaced(Reach::Store, before).now().unwrap();
    onto.finish_topics(Reach::Store, &restored).now().unwrap();
    onto.finish_state(&positions, &topics).now().unwrap();
}

#[test]
fn a_restored_snapshot_keeps_each_messages_position_and_instant() {
    let source = source();
    let expected = as_appended(&source);
    let mut file = Vec::new();
    tessari_backup::write_state(&source, &mut file)
        .now()
        .unwrap();
    let (_, target) = store();
    tessari_backup::read_state(&target, || Ok(file.as_slice()))
        .now()
        .unwrap();
    assert_eq!(messages(&target), expected);
    assert_eq!(next_position(&target, "echo"), 9);
}

#[test]
fn a_follower_copied_again_keeps_its_leaders_positions() {
    let source = source();
    let (_, follower) = store();
    copied(&source, &follower, || {});
    // The leader moves on and retention removes what the follower holds.
    append(&source, &["foxtrot", "golf", "hotel", "india"]);
    copied(&source, &follower, || {});
    let expected = messages(&source);
    assert_eq!(
        expected.last().map(|(_, position, _)| *position),
        Some(12),
        "{expected:?}"
    );
    assert_eq!(messages(&follower), expected);
    assert_eq!(next_position(&follower, "juliet"), 13);
}

/// A message appended after the state's version is not in the state, though
/// its entry and the topic's raised head are in the index when they are read.
/// The copy holds neither, and numbers on from where the state stood, so the
/// log above it gives that message the position it has on the source. The
/// append retained the topic's oldest message away, which the state still
/// holds with no entry left to place it: the copy removes it, as that log does.
#[test]
fn a_message_appended_while_the_state_is_read_is_left_to_the_log_above() {
    let source = source();
    let at_the_cut = as_appended(&source);
    let (_, target) = store();
    copied(&source, &target, || append(&source, &["late"]));
    let now = messages(&source);
    let (late, before_late) = now.split_last().unwrap();
    assert!(late.0.contains("late"), "{now:?}");
    assert_eq!(late.1, 9);
    assert!(
        before_late.len() < at_the_cut.len(),
        "the append retained nothing away: {now:?}"
    );
    assert_eq!(messages(&target), before_late);
    assert_eq!(next_position(&target, "echo"), late.1);
}
