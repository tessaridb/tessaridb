//! COPY both directions over the wire: the statement answers CopyInResponse
//! or CopyOutResponse, text rows ride as CopyData, the batch lands as ONE
//! engine INSERT, and every shape outside the stated reach refuses by name —
//! formats, directions, server-side files, malformed rows naming the line.

use super::{Client, SETUP, Seen, refused, rows, serving_with};

#[tokio::test]
async fn copy_from_lands_the_batch_and_answers_copy_n() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client.send_query("COPY orders FROM STDIN").await;
    assert!(
        matches!(client.message().await, Seen::CopyIn(2)),
        "the statement answers CopyInResponse naming the column count"
    );
    // Rows may be cut across CopyData frames; the codec reassembles on \n.
    client.copy_data(b"9\tnin").await;
    client.copy_data(b"th\n10\ttenth\n").await;
    client.copy_done().await;
    assert!(
        matches!(client.message().await, Seen::Command(tag) if tag == "COPY 2"),
        "the tag names the landed rows"
    );
    assert!(matches!(client.message().await, Seen::Ready(b'I')));

    let seen = client
        .query("SELECT band, name FROM orders WHERE band = 9")
        .await;
    assert_eq!(
        rows(&seen),
        vec![vec![Some("9".to_owned()), Some("ninth".to_owned())]],
        "{seen:?}"
    );
    stop.cancel();
}

#[tokio::test]
async fn copy_from_honors_the_null_marker_before_the_escapes() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client.send_query("COPY orders FROM STDIN").await;
    let _in = client.message().await;
    // `\N` alone is the null marker — matched before any backslash handling.
    // `\\N` is a data backslash followed by N, not the marker.
    client.copy_data(b"5\t\\N\n6\t\\\\N\n").await;
    client.copy_done().await;
    assert!(
        matches!(client.message().await, Seen::Command(tag) if tag == "COPY 2"),
        "both rows landed"
    );
    assert!(matches!(client.message().await, Seen::Ready(b'I')));

    let seen = client
        .query("SELECT band, name FROM orders WHERE band >= 5 ORDER BY band")
        .await;
    assert_eq!(
        rows(&seen),
        vec![
            vec![Some("5".to_owned()), None],
            vec![Some("6".to_owned()), Some("\\N".to_owned())],
        ],
        "the marker is null; the escaped form is two characters of data: {seen:?}"
    );
    stop.cancel();
}

#[tokio::test]
async fn copy_from_refuses_a_bad_row_naming_the_line_and_lands_nothing() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client.send_query("COPY orders FROM STDIN").await;
    let _in = client.message().await;
    // Line 2's band does not read as an int; line 3 never arrives.
    client.copy_data(b"7\tseventh\nabc\teighth\n").await;
    client.copy_done().await;
    let seen = client.message().await;
    let (code, message) = match &seen {
        Seen::Error(code, message) => (code.clone(), message.clone()),
        other => unreachable!("an error where the row refused: {other:?}"),
    };
    assert_eq!(code, "22P02", "a bad value is invalid text: {message}");
    assert!(
        message.contains("line 2"),
        "the refusal names the copy's own line: {message}"
    );
    assert!(matches!(client.message().await, Seen::Ready(b'I')));

    // The whole copy is one engine INSERT: nothing of it landed.
    let seen = client.query("SELECT count(*) AS n FROM orders").await;
    assert_eq!(rows(&seen), vec![vec![Some("3".to_owned())]], "{seen:?}");
    stop.cancel();
}

#[tokio::test]
async fn copy_from_refuses_an_unterminated_final_row() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client.send_query("COPY orders FROM STDIN").await;
    let _in = client.message().await;
    client.copy_data(b"7\tseventh").await;
    client.copy_done().await;
    let (code, message) = refused(&[client.message().await]).expect("the refusal");
    assert_eq!(
        code, "22P04",
        "a row without its newline is malformed: {message}"
    );
    assert!(message.contains("line 1"), "the line is named: {message}");
    assert!(matches!(client.message().await, Seen::Ready(b'I')));
    stop.cancel();
}

#[tokio::test]
async fn copy_from_abandons_on_copy_fail_with_the_clients_words() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client.send_query("COPY orders FROM STDIN").await;
    let _in = client.message().await;
    client.copy_data(b"7\tseventh\n").await;
    client.copy_fail("changed my mind").await;
    let seen = client.message().await;
    let (code, message) = match &seen {
        Seen::Error(code, message) => (code.clone(), message.clone()),
        other => unreachable!("an error where CopyFail was answered: {other:?}"),
    };
    assert_eq!(code, "22P04");
    assert_eq!(
        message, "changed my mind",
        "the client's words ride verbatim"
    );
    assert!(matches!(client.message().await, Seen::Ready(b'I')));

    let seen = client.query("SELECT count(*) AS n FROM orders").await;
    assert_eq!(rows(&seen), vec![vec![Some("3".to_owned())]], "{seen:?}");
    stop.cancel();
}

#[tokio::test]
async fn copy_from_skips_the_header_and_counts_it_in_the_lines() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client
        .send_query("COPY orders FROM STDIN WITH (FORMAT text, HEADER true)")
        .await;
    let _in = client.message().await;
    client.copy_data(b"band\tname\n8\teighth\n").await;
    client.copy_done().await;
    assert!(
        matches!(client.message().await, Seen::Command(tag) if tag == "COPY 1"),
        "the header is not a row"
    );
    assert!(matches!(client.message().await, Seen::Ready(b'I')));
    let seen = client.query("SELECT name FROM orders WHERE band = 8").await;
    assert_eq!(
        rows(&seen),
        vec![vec![Some("eighth".to_owned())]],
        "{seen:?}"
    );

    // A bad row under the header names the physical line — the header counts.
    client
        .send_query("COPY orders FROM STDIN WITH (HEADER true)")
        .await;
    let _in = client.message().await;
    client.copy_data(b"band\tname\nabc\tx\n").await;
    client.copy_done().await;
    let (_, message) = refused(&[client.message().await]).expect("the refusal");
    assert!(
        message.contains("line 2"),
        "the header is line 1: {message}"
    );
    assert!(matches!(client.message().await, Seen::Ready(b'I')));
    stop.cancel();
}

#[tokio::test]
async fn copy_from_refuses_null_into_a_strict_field_naming_the_line() {
    let setup = format!(
        "{SETUP} DEFINE TABLE stricts SCHEMALESS; \
         DEFINE FIELD code ON stricts TYPE string REQUIRED;"
    );
    let (address, stop, _db) = serving_with(&setup).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client.send_query("COPY stricts FROM STDIN").await;
    let _in = client.message().await;
    client.copy_data(b"\\N\n").await;
    client.copy_done().await;
    let (code, message) = refused(&[client.message().await]).expect("the refusal");
    assert_eq!(
        code, "23502",
        "a null into a strict field is a not-null violation: {message}"
    );
    assert!(message.contains("code"), "the column is named: {message}");
    assert!(matches!(client.message().await, Seen::Ready(b'I')));
    stop.cancel();
}

#[tokio::test]
async fn copy_to_streams_the_rows_as_copy_data() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client.send_query("COPY orders TO STDOUT").await;
    assert!(
        matches!(client.message().await, Seen::CopyOut(2)),
        "the statement answers CopyOutResponse naming the column count"
    );
    let mut data = Vec::new();
    loop {
        match client.message().await {
            Seen::CopyData(row) => data.push(row),
            Seen::CopyDone => break,
            other => unreachable!("a row or CopyDone, not {other:?}"),
        }
    }
    assert_eq!(
        data,
        vec![
            "1\tfirst\n".to_owned(),
            "3\tthird\n".to_owned(),
            "3\t\\N\n".to_owned(),
        ],
        "absent values cross as the null marker: {data:?}"
    );
    assert!(
        matches!(client.message().await, Seen::Command(tag) if tag == "COPY 3"),
        "the tag names the streamed rows"
    );
    assert!(matches!(client.message().await, Seen::Ready(b'I')));
    stop.cancel();
}

#[tokio::test]
async fn copy_to_honors_a_column_list_a_query_and_the_header() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client.send_query("COPY orders (name) TO STDOUT").await;
    assert!(matches!(client.message().await, Seen::CopyOut(1)));
    let mut data = Vec::new();
    loop {
        match client.message().await {
            Seen::CopyData(row) => data.push(row),
            Seen::CopyDone => break,
            other => unreachable!("a row or CopyDone, not {other:?}"),
        }
    }
    assert_eq!(
        data,
        vec![
            "first\n".to_owned(),
            "third\n".to_owned(),
            "\\N\n".to_owned()
        ],
        "{data:?}"
    );
    assert!(matches!(client.message().await, Seen::Command(tag) if tag == "COPY 3"));
    assert!(matches!(client.message().await, Seen::Ready(b'I')));

    // The query form reuses the SELECT translator; its projection names the
    // copy. Absent values order lowest, NULLS LAST puts the named row first.
    client
        .send_query(
            "COPY (SELECT name FROM orders WHERE band = 3 ORDER BY name NULLS LAST) TO STDOUT",
        )
        .await;
    assert!(matches!(client.message().await, Seen::CopyOut(1)));
    let mut data = Vec::new();
    loop {
        match client.message().await {
            Seen::CopyData(row) => data.push(row),
            Seen::CopyDone => break,
            other => unreachable!("a row or CopyDone, not {other:?}"),
        }
    }
    assert_eq!(
        data,
        vec!["third\n".to_owned(), "\\N\n".to_owned()],
        "{data:?}"
    );
    assert!(matches!(client.message().await, Seen::Command(tag) if tag == "COPY 2"));
    assert!(matches!(client.message().await, Seen::Ready(b'I')));

    // HEADER names the columns on the first line.
    client
        .send_query("COPY orders TO STDOUT WITH (FORMAT text, HEADER true)")
        .await;
    assert!(matches!(client.message().await, Seen::CopyOut(2)));
    assert!(
        matches!(client.message().await, Seen::CopyData(row) if row == "band\tname\n"),
        "the header rides first"
    );
    let mut count = 0;
    loop {
        match client.message().await {
            Seen::CopyData(_) => count += 1,
            Seen::CopyDone => break,
            other => unreachable!("a row or CopyDone, not {other:?}"),
        }
    }
    assert_eq!(count, 3, "three rows follow the header");
    assert!(matches!(client.message().await, Seen::Command(tag) if tag == "COPY 3"));
    assert!(matches!(client.message().await, Seen::Ready(b'I')));
    stop.cancel();
}

#[tokio::test]
async fn copy_refuses_the_unspoken_surface_by_name() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    let seen = client
        .query("COPY orders FROM STDIN WITH (FORMAT csv)")
        .await;
    let (code, message) = refused(&seen).expect("the refusal");
    assert_eq!(code, "0A000");
    assert!(message.contains("csv"), "the format is named: {message}");

    let seen = client
        .query("COPY orders FROM STDIN WITH (ON_ERROR ignore)")
        .await;
    let (code, message) = refused(&seen).expect("the refusal");
    assert_eq!(code, "0A000");
    assert!(
        message.to_uppercase().contains("ON_ERROR"),
        "the option is named: {message}"
    );

    let seen = client.query("COPY orders TO STDIN").await;
    let (code, message) = refused(&seen).expect("the refusal");
    assert_eq!(code, "0A000");
    assert!(
        message.contains("TO STDIN"),
        "the direction swap is named: {message}"
    );

    let seen = client.query("COPY orders FROM '/tmp/orders.txt'").await;
    let (code, message) = refused(&seen).expect("the refusal");
    assert_eq!(code, "0A000");
    assert!(
        message.contains("server-side"),
        "the file is named for what it is: {message}"
    );

    let seen = client
        .query("COPY (SELECT band FROM orders) (band) TO STDOUT")
        .await;
    let (code, message) = refused(&seen).expect("the refusal");
    assert_eq!(code, "0A000");
    assert!(
        message.contains("column"),
        "the column list on the query form is named: {message}"
    );
    stop.cancel();
}

#[tokio::test]
async fn copy_over_the_extended_protocol_refuses_by_name() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client.parse("", "COPY orders FROM STDIN", &[]).await;
    client.sync().await;
    let seen = [client.message().await, client.message().await];
    let (code, message) = refused(&seen).expect("the refusal");
    assert_eq!(code, "0A000");
    assert!(
        message.contains("extended"),
        "the extended path is named: {message}"
    );
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
        "the Sync owes the readiness: {seen:?}"
    );
    stop.cancel();
}
