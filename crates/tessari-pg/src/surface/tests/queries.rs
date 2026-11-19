//! The query path over a socket: startup, then the SELECT spine, then the
//! refusals — each read back exactly as a client of the dialect reads it.

use super::{Client, SETUP, Seen, params, refused, rows, serving_with};
use tokio::io::AsyncReadExt as _;

#[tokio::test(flavor = "multi_thread")]
async fn an_open_store_greets_a_client_without_a_user() {
    let (address, stop, _) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    let seen = client.start(None, "app.main", None).await;
    assert!(seen.contains(&Seen::Auth(0)), "AuthenticationOk: {seen:?}");
    assert!(matches!(seen.last(), Some(Seen::Ready(b'I'))));
    let greeting = params(&seen);
    assert!(greeting.iter().any(|(k, _)| k == "server_version"));
    assert!(seen.contains(&Seen::BackendKey));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn select_star_answers_the_tables_rows() {
    let (address, stop, _) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;
    let seen = client.query("SELECT * FROM orders").await;
    let desc = seen.iter().find_map(|m| match m {
        Seen::RowDesc(cols) => Some(cols.clone()),
        _ => None,
    });
    assert_eq!(
        desc,
        Some(vec!["band".to_owned(), "name".to_owned()]),
        "{seen:?}"
    );
    let rows = rows(&seen);
    assert_eq!(rows.len(), 3);
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(tag) if tag == "SELECT 3"))
    );
    assert!(matches!(seen.last(), Some(Seen::Ready(b'I'))));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn where_and_limit_run_with_literals_bound() {
    let (address, stop, _) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;
    let seen = client
        .query("SELECT name FROM orders WHERE band = 3 LIMIT 1")
        .await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::RowDesc(cols) if cols == &["name".to_owned()])),
        "{seen:?}"
    );
    assert_eq!(rows(&seen).len(), 1);
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn is_null_reads_the_stores_two_absences() {
    let (address, stop, _) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;
    let seen = client
        .query("SELECT id FROM orders WHERE name IS NULL")
        .await;
    let rows = rows(&seen);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0], vec![Some("orders:3".to_owned())]);
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn select_without_from_answers_one_column() {
    let (address, stop, _) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;
    let seen = client.query("SELECT 1").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::RowDesc(cols) if cols == &["?column?".to_owned()]))
    );
    assert_eq!(rows(&seen), vec![vec![Some("1".to_owned())]]);
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_query_answers_empty() {
    let (address, stop, _) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;
    let seen = client.query("   ").await;
    assert!(seen.contains(&Seen::Empty));
    assert!(matches!(seen.last(), Some(Seen::Ready(b'I'))));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_syntax_refusal_names_it_and_keeps_the_session() {
    let (address, stop, _) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;
    let seen = client.query("SELECT FROM").await;
    let (code, _) = refused(&seen).expect("a syntax refusal");
    assert_eq!(code, "42601");
    assert!(matches!(seen.last(), Some(Seen::Ready(b'I'))));
    // The session is usable after the refusal.
    let seen = client.query("SELECT id FROM orders LIMIT 1").await;
    assert_eq!(rows(&seen).len(), 1);
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unspoken_message_refuses_by_name_and_sync_recovers() {
    let (address, stop, _) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;
    // A FunctionCall, a message this build does not speak: refused, then Sync
    // answers with readiness and the session is whole.
    let mut call = vec![b'F'];
    let body = b"\0\0\0\0";
    call.extend_from_slice(
        &u32::try_from(body.len().saturating_add(4))
            .unwrap()
            .to_be_bytes(),
    );
    call.extend_from_slice(body);
    client.write(&call).await;
    let seen = client.message().await;
    assert!(matches!(seen, Seen::Error(code, _) if code == "0A000"));
    let sync = [b'S', 0, 0, 0, 4];
    client.write(&sync).await;
    assert!(matches!(client.message().await, Seen::Ready(b'I')));
    let seen = client.query("SELECT id FROM orders LIMIT 1").await;
    assert_eq!(rows(&seen).len(), 1);
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_store_refusal_carries_the_stores_words() {
    let (address, stop, _) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;
    let seen = client.query("SELECT id FROM nowhere").await;
    let (code, message) = refused(&seen).expect("the store's refusal");
    // The class decides the code; the store's own words travel in the message.
    assert_eq!(code, "42601");
    assert!(message.to_lowercase().contains("nowhere"), "{message}");
    assert!(matches!(seen.last(), Some(Seen::Ready(b'I'))));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_database_form_refuses_before_the_session_and_closes() {
    let (address, stop, _) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    let seen = client.start(None, "app.main.extra", None).await;
    let (code, _) = refused(&seen).expect("the catalog refusal");
    assert_eq!(code, "3D000");
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_request_is_answered_by_closing() {
    let (address, stop, _) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    let mut body = Vec::new();
    body.extend_from_slice(&crate::codec::CANCEL_REQUEST.to_be_bytes());
    body.extend_from_slice(&1_u32.to_be_bytes());
    body.extend_from_slice(&1_u32.to_be_bytes());
    let mut frame = Vec::new();
    frame.extend_from_slice(
        &u32::try_from(body.len().saturating_add(4))
            .unwrap()
            .to_be_bytes(),
    );
    frame.extend_from_slice(&body);
    client.write(&frame).await;
    // No reply, by the protocol's own rule; the closed read ends at zero.
    let mut at = [0_u8; 1];
    let read = tokio::time::Duration::from_secs(5);
    let got = tokio::time::timeout(read, client.stream.read(&mut at)).await;
    assert!(matches!(got, Ok(Ok(0))), "the connection closes: {got:?}");
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn offset_pages_the_ordered_read_exactly() {
    let (address, stop, _) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;
    // The control read: the whole ordered answer.
    let control = rows(&client.query("SELECT name FROM orders ORDER BY band").await);
    assert_eq!(control.len(), 3);
    // OFFSET 0 is the no-op, indistinguishable from no OFFSET.
    let zero = rows(
        &client
            .query("SELECT name FROM orders ORDER BY band LIMIT 3 OFFSET 0")
            .await,
    );
    assert_eq!(zero, control, "OFFSET 0 changes nothing");
    // One wider bounded read, the head dropped: the page equals the control's tail.
    let seen = client
        .query("SELECT name FROM orders ORDER BY band LIMIT 2 OFFSET 1")
        .await;
    assert_eq!(rows(&seen), control[1..], "the page is the tail: {seen:?}");
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "SELECT 2")),
        "the completion names the page: {seen:?}"
    );
    // Past the end the page is empty, honestly counted.
    let seen = client
        .query("SELECT name FROM orders ORDER BY band LIMIT 1 OFFSET 9")
        .await;
    assert_eq!(
        rows(&seen),
        Vec::<Vec<Option<String>>>::new(),
        "past the end: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "SELECT 0")),
        "the empty page counts zero: {seen:?}"
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn offset_without_order_by_is_the_walk_order_of_that_read() {
    let (address, stop, _) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;
    // Without ORDER BY the answer's order is the store's walk order for that
    // snapshot — the same unspecified-order contract the dialect itself
    // carries. The page is exact against the wider read's tail.
    let wide = rows(&client.query("SELECT id FROM orders LIMIT 2").await);
    let paged = rows(&client.query("SELECT id FROM orders LIMIT 1 OFFSET 1").await);
    assert_eq!(
        paged,
        wide[1..],
        "the page equals the wider read's tail: {paged:?}"
    );
    // OFFSET alone pages the whole read.
    let all = rows(&client.query("SELECT id FROM orders OFFSET 2").await);
    assert_eq!(
        all.len(),
        1,
        "OFFSET alone drops the head of the read: {all:?}"
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn like_ilike_casts_and_union_answer_over_the_wire() {
    let (address, stop, _) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;
    let seen = client
        .query("SELECT name FROM orders WHERE name LIKE 'f%'")
        .await;
    assert_eq!(
        rows(&seen),
        vec![vec![Some("first".to_owned())]],
        "{seen:?}"
    );
    let seen = client
        .query("SELECT name FROM orders WHERE name ILIKE 'F%'")
        .await;
    assert_eq!(
        rows(&seen),
        vec![vec![Some("first".to_owned())]],
        "{seen:?}"
    );
    let seen = client
        .query("SELECT name FROM orders WHERE name NOT LIKE 'f%'")
        .await;
    assert_eq!(rows(&seen).len(), 2, "{seen:?}");
    let seen = client
        .query("SELECT band::text FROM orders WHERE name = 'first'")
        .await;
    assert_eq!(rows(&seen), vec![vec![Some("1".to_owned())]], "{seen:?}");
    let seen = client
        .query("SELECT name FROM orders WHERE band = 1 UNION ALL SELECT name FROM orders WHERE band = 3")
        .await;
    // The third record holds no name — the compound answers the absent
    // field the same way one read does.
    assert_eq!(
        rows(&seen),
        vec![
            vec![Some("first".to_owned())],
            vec![Some("third".to_owned())],
            vec![None]
        ],
        "{seen:?}"
    );
    stop.cancel();
}

#[tokio::test]
async fn a_compound_tail_sorts_the_whole_answer() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    // The trailing ORDER BY binds to the whole compound, not the last
    // branch: the collected answer sorts as one set, the absent name
    // riding lowest by the declared order.
    let seen = client
        .query(
            "SELECT name FROM orders WHERE band = 3 \
             UNION ALL SELECT name FROM orders WHERE band = 1 ORDER BY name",
        )
        .await;
    assert_eq!(
        rows(&seen),
        vec![
            vec![None],
            vec![Some("first".to_owned())],
            vec![Some("third".to_owned())],
        ],
        "{seen:?}"
    );

    // The bound binds after the sort.
    let seen = client
        .query(
            "SELECT name FROM orders WHERE band = 3 \
             UNION ALL SELECT name FROM orders WHERE band = 1 ORDER BY name LIMIT 2",
        )
        .await;
    assert_eq!(
        rows(&seen),
        vec![vec![None], vec![Some("first".to_owned())]],
        "{seen:?}"
    );
    stop.cancel();
}
