//! The wire transaction block: BEGIN/COMMIT/ROLLBACK walk the connection's
//! state machine, the readiness carries I/T/E, a failure inside a block
//! aborts it, and a multi-statement query runs as one implicit block.

use super::{Client, SETUP, Seen, refused, rows, serving_with};

#[tokio::test]
async fn a_block_holds_the_readiness_until_it_closes() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    let seen = client.query("BEGIN").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "BEGIN")),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'T'))),
        "the readiness carries the open block: {seen:?}"
    );

    let seen = client.query("SELECT name FROM orders LIMIT 1").await;
    assert_eq!(rows(&seen), vec![vec![Some("first".to_owned())]]);
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'T'))),
        "a statement inside keeps the block open: {seen:?}"
    );

    let seen = client.query("COMMIT").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "COMMIT")),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
        "{seen:?}"
    );
}

#[tokio::test]
async fn a_verb_outside_a_block_refuses_by_name() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    for verb in ["COMMIT", "ROLLBACK"] {
        let seen = client.query(verb).await;
        let (code, _) = refused(&seen).unwrap_or_default();
        assert_eq!(code, "25P01", "{verb}: {seen:?}");
        assert!(
            seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
            "{seen:?}"
        );
    }
}

#[tokio::test]
async fn a_failure_inside_aborts_the_block() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client.query("BEGIN").await;
    let seen = client.query("SELECT FROM").await;
    assert!(refused(&seen).is_some(), "{seen:?}");
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'E'))),
        "the aborted block rides E: {seen:?}"
    );

    let seen = client.query("SELECT name FROM orders LIMIT 1").await;
    let (code, _) = refused(&seen).unwrap_or_default();
    assert_eq!(
        code, "25P02",
        "nothing runs inside an aborted block: {seen:?}"
    );

    // COMMIT of an aborted block answers ROLLBACK, as the dialect does.
    let seen = client.query("COMMIT").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "ROLLBACK")),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
        "{seen:?}"
    );
    let seen = client.query("SELECT name FROM orders LIMIT 1").await;
    assert_eq!(rows(&seen), vec![vec![Some("first".to_owned())]]);
}

#[tokio::test]
async fn rollback_closes_an_open_block() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client.query("BEGIN").await;
    let seen = client.query("ROLLBACK").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "ROLLBACK")),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
        "{seen:?}"
    );
}

#[tokio::test]
async fn a_block_holds_its_writes_until_rollback() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client.query("BEGIN;").await;
    let seen = client
        .query("UPDATE orders SET name = 'changed' WHERE name = 'first'")
        .await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "UPDATE 1")),
        "{seen:?}"
    );
    // Its own write is its own read, inside the block.
    let seen = client.query("SELECT name FROM orders LIMIT 1").await;
    assert_eq!(rows(&seen), vec![vec![Some("changed".to_owned())]]);

    let seen = client.query("ROLLBACK;").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "ROLLBACK")),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
        "{seen:?}"
    );
    // The undo is the store's own: the old value stands.
    let seen = client.query("SELECT name FROM orders LIMIT 1").await;
    assert_eq!(rows(&seen), vec![vec![Some("first".to_owned())]]);
}

#[tokio::test]
async fn a_block_lands_its_writes_at_commit() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client.query("BEGIN").await;
    client
        .query("UPDATE orders SET name = 'landed' WHERE name = 'first'")
        .await;
    let seen = client.query("COMMIT").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "COMMIT")),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
        "{seen:?}"
    );

    // A later statement on the same connection reads what the block landed.
    let seen = client.query("SELECT name FROM orders LIMIT 1").await;
    assert_eq!(rows(&seen), vec![vec![Some("landed".to_owned())]]);
}

#[tokio::test]
async fn verbs_in_one_string_run_as_one_block() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    // The whole block in one simple query: the verbs answer under their own
    // tags, the statement between them under its count, and the readiness
    // reports the block closed inside the run.
    let seen = client
        .query("BEGIN; UPDATE orders SET name = 'one string' WHERE name = 'first'; COMMIT")
        .await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "BEGIN")),
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "UPDATE 1")),
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "COMMIT")),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
        "the block closed inside the string: {seen:?}"
    );

    let seen = client.query("SELECT name FROM orders LIMIT 1").await;
    assert_eq!(rows(&seen), vec![vec![Some("one string".to_owned())]]);

    // The same block with ROLLBACK answers ROLLBACK and lands nothing.
    let seen = client
        .query("BEGIN; UPDATE orders SET name = 'gone' WHERE name = 'first'; ROLLBACK")
        .await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "ROLLBACK")),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
        "{seen:?}"
    );
    let seen = client.query("SELECT name FROM orders LIMIT 1").await;
    assert_eq!(rows(&seen), vec![vec![Some("one string".to_owned())]]);
}

#[tokio::test]
async fn a_multi_statement_query_runs_as_one_block() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    let seen = client
        .query("SELECT name FROM orders LIMIT 1; SELECT band FROM orders LIMIT 1")
        .await;
    assert_eq!(
        rows(&seen),
        vec![vec![Some("first".to_owned())], vec![Some("1".to_owned())]],
        "both statements answer, in order: {seen:?}"
    );
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
        "the implicit block closed inside the run: {seen:?}"
    );

    // Two bare projections in one block hit the store's own rule — a script
    // answers with one value — and its words travel.
    let seen = client.query("SELECT 1; SELECT 2").await;
    let (code, message) = refused(&seen).unwrap_or_default();
    assert_eq!(code, "42601", "{seen:?}");
    assert!(
        message.contains("one value"),
        "the store's own contract speaks: {seen:?}"
    );

    // One refusing statement refuses the whole block: the first statement's
    // rows never leak.
    let seen = client.query("SELECT name FROM orders; SELECT FROM").await;
    assert!(refused(&seen).is_some(), "{seen:?}");
    assert!(
        rows(&seen).is_empty(),
        "no half-answered rows from a refused block: {seen:?}"
    );
}

#[tokio::test]
async fn savepoints_ride_across_queries_in_a_block() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    // Every query crosses the bridge; the savepoint must ride it.
    client.query("BEGIN").await;
    let seen = client.query("SAVEPOINT one").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "SAVEPOINT")),
        "{seen:?}"
    );
    client
        .query("UPDATE orders SET name = 'changed' WHERE name = 'first'")
        .await;
    let seen = client.query("ROLLBACK TO SAVEPOINT one").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "ROLLBACK")),
        "{seen:?}"
    );
    client.query("COMMIT").await;

    // The undo is the store's own: the block rolled back to the mark.
    let seen = client.query("SELECT name FROM orders LIMIT 1").await;
    assert_eq!(
        rows(&seen),
        vec![vec![Some("first".to_owned())]],
        "{seen:?}"
    );
    stop.cancel();
}

#[tokio::test]
async fn a_released_savepoint_refuses_the_rollback_by_name() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client.query("BEGIN").await;
    client.query("SAVEPOINT one").await;
    let seen = client.query("RELEASE SAVEPOINT one").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "RELEASE")),
        "{seen:?}"
    );
    let seen = client.query("ROLLBACK TO one").await;
    let (code, message) = refused(&seen).expect("the refusal");
    assert_eq!(
        code, "42601",
        "the engine's name rides the class: {message}"
    );
    assert!(
        message.to_lowercase().contains("savepoint") || message.contains("one"),
        "the missing mark is named: {message}"
    );
    client.query("ROLLBACK").await;
    stop.cancel();
}

#[tokio::test]
async fn savepoints_are_a_connections_own() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut first = Client::to(&address).await;
    first.start(None, "app.main", None).await;
    let mut second = Client::to(&address).await;
    second.start(None, "app.main", None).await;

    first.query("BEGIN").await;
    first.query("SAVEPOINT one").await;
    // The other connection's block knows nothing of it: its own verb state
    // is untouched, and the name is free for its own use.
    second.query("BEGIN").await;
    let seen = second.query("SAVEPOINT one").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "SAVEPOINT")),
        "{seen:?}"
    );
    second.query("ROLLBACK").await;
    first.query("ROLLBACK").await;
    stop.cancel();
}

#[tokio::test]
async fn savepoints_ride_the_extended_protocol() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    for verb in [
        "BEGIN",
        "SAVEPOINT one",
        "UPDATE orders SET name = 'changed' WHERE name = 'first'",
        "ROLLBACK TO one",
        "COMMIT",
    ] {
        client.parse("", verb, &[]).await;
        client.bind("", "", &[], &[], &[]).await;
        client.execute("", 0).await;
        client.sync().await;
        let mut seen = Vec::new();
        loop {
            let message = client.message().await;
            let ready = matches!(message, Seen::Ready(_));
            seen.push(message);
            if ready {
                break;
            }
        }
        assert!(
            !seen.iter().any(|m| matches!(m, Seen::Error(..))),
            "{verb} refused: {seen:?}"
        );
    }
    let seen = client.query("SELECT name FROM orders LIMIT 1").await;
    assert_eq!(
        rows(&seen),
        vec![vec![Some("first".to_owned())]],
        "{seen:?}"
    );
    stop.cancel();
}

// ---------------------------------------------------------------------------
// Row claims across connections: a FOR UPDATE read holds its rows to the
// block's end; another connection's write is refused 55P03 (the dialect's
// lock-not-available — the wait a NOWAIT read will not do), and SKIP LOCKED
// passes the claimed row over.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_claimed_row_refuses_another_connections_write_until_the_commit() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut first = Client::to(&address).await;
    first.start(None, "app.main", None).await;
    let mut second = Client::to(&address).await;
    second.start(None, "app.main", None).await;

    first.query("BEGIN").await;
    let held = first
        .query("SELECT name FROM orders WHERE band = 1 FOR UPDATE")
        .await;
    assert_eq!(
        rows(&held),
        vec![vec![Some("first".to_owned())]],
        "{held:?}"
    );

    // The claim holds: the second connection's write is refused, with the
    // dialect's lock-not-available code, and the words name the claim.
    let seen = second
        .query("UPDATE orders SET name = 'taken' WHERE band = 1")
        .await;
    let (code, message) = refused(&seen).expect("a refusal: {seen:?}");
    assert_eq!(code, "55P03", "{seen:?}");
    assert!(
        message.contains("claimed"),
        "the refusal names the claim: {message}"
    );

    // The block ends and the claim with it; the write applies.
    first.query("COMMIT").await;
    let done = second
        .query("UPDATE orders SET name = 'taken' WHERE band = 1")
        .await;
    assert!(
        done.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "UPDATE 1")),
        "{done:?}"
    );
    stop.cancel();
}

#[tokio::test]
async fn skip_locked_passes_the_claimed_row_over() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut first = Client::to(&address).await;
    first.start(None, "app.main", None).await;
    let mut second = Client::to(&address).await;
    second.start(None, "app.main", None).await;

    first.query("BEGIN").await;
    first
        .query("SELECT * FROM orders WHERE band = 1 FOR UPDATE")
        .await;

    // The second connection sees neither the claim nor the claimed row: the
    // row behind it answers instead.
    let seen = second
        .query("SELECT id FROM orders ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED")
        .await;
    assert_eq!(
        rows(&seen),
        vec![vec![Some("orders:2".to_owned())]],
        "{seen:?}"
    );

    first.query("ROLLBACK").await;
    // The claim died with the block: the head answers again.
    let after = second
        .query("SELECT id FROM orders ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED")
        .await;
    assert_eq!(
        rows(&after),
        vec![vec![Some("orders:1".to_owned())]],
        "{after:?}"
    );
    stop.cancel();
}

#[tokio::test]
async fn nowait_refuses_a_locking_read_over_a_claimed_row() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut first = Client::to(&address).await;
    first.start(None, "app.main", None).await;
    let mut second = Client::to(&address).await;
    second.start(None, "app.main", None).await;

    first.query("BEGIN").await;
    first
        .query("SELECT * FROM orders WHERE band = 1 FOR UPDATE")
        .await;

    // NOWAIT is the default: the read refuses rather than waits.
    let seen = second
        .query("SELECT * FROM orders WHERE band = 1 FOR UPDATE")
        .await;
    let (code, message) = refused(&seen).expect("a refusal: {seen:?}");
    assert_eq!(code, "55P03", "{seen:?}");
    assert!(
        message.contains("never waits"),
        "the refusal names the wait it will not do: {message}"
    );

    first.query("ROLLBACK").await;
    let free = second
        .query("SELECT * FROM orders WHERE band = 1 FOR UPDATE")
        .await;
    assert_eq!(rows(&free).len(), 1, "{free:?}");
    stop.cancel();
}
