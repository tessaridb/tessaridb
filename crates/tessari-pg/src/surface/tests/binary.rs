//! The binary half of the type correspondence (D6/D8): the seven fixed
//! scalars cross in both directions — Bind parameters decode from their
//! declared type's binary form, DataRow cells encode into it. The text
//! family crosses unchanged: its binary form IS the text bytes.

use super::super::tests::Client;
use super::{SETUP, Seen, serving_with};

/// One raw backend message, kind byte and body.
async fn raw(client: &mut Client) -> (u8, Vec<u8>) {
    let head = client.read(5).await;
    let len = u32::from_be_bytes(head[1..].try_into().expect("four bytes"));
    let len = usize::try_from(len.saturating_sub(4)).expect("a frame length");
    let body = client.read(len).await;
    (head[0], body)
}

/// One DataRow body decoded into cells that stay bytes.
fn cells(body: &[u8]) -> Vec<Option<Vec<u8>>> {
    let count = u16::from_be_bytes(body[..2].try_into().expect("two bytes"));
    let mut at = 2_usize;
    let mut out = Vec::new();
    for _ in 0..count {
        let len = i32::from_be_bytes(
            body[at..at.saturating_add(4)]
                .try_into()
                .expect("four bytes"),
        );
        at = at.saturating_add(4);
        if len < 0 {
            out.push(None);
        } else {
            let n = usize::try_from(len).expect("a positive length");
            out.push(Some(body[at..at.saturating_add(n)].to_vec()));
            at = at.saturating_add(n);
        }
    }
    out
}

/// Skip messages until a DataRow or Error arrives; answers the row's cells.
async fn row_of(client: &mut Client) -> Vec<Option<Vec<u8>>> {
    loop {
        let (kind, body) = raw(client).await;
        match kind {
            b'D' => return cells(&body),
            b'E' => {
                assert_eq!(0, 1, "an error where a row was expected: {body:?}");
                return Vec::new();
            }
            b'Z' => {
                assert_eq!(0, 1, "readiness where a row was expected");
                return Vec::new();
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn a_binary_int_parameter_decodes_and_a_binary_result_encodes() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    // band = $1 sent as binary int8(20): 8 big-endian bytes.
    client
        .parse("", "SELECT band FROM orders WHERE band = $1", &[20])
        .await;
    client
        .bind("", "", &[1], &[Some(3_i64.to_be_bytes().to_vec())], &[1])
        .await;
    client.describe(b'P', "").await;
    client.execute("", 0).await;
    client.sync().await;
    // ParseComplete, BindComplete, RowDescription — then the rows, binary.
    let got = row_of(&mut client).await;
    assert_eq!(got, vec![Some(3_i64.to_be_bytes().to_vec())]);
}

#[tokio::test]
async fn the_text_family_crosses_binary_unchanged() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client
        .parse("", "SELECT name FROM orders WHERE band = 1", &[])
        .await;
    client.bind("", "", &[], &[], &[1]).await;
    client.execute("", 0).await;
    client.sync().await;
    client.message().await; // ParseComplete
    client.message().await; // BindComplete
    let got = row_of(&mut client).await;
    assert_eq!(got, vec![Some(b"first".to_vec())]);
}

#[tokio::test]
async fn a_binary_float_and_bytea_round_trip() {
    let setup = "DEFINE TABLE blobs SCHEMALESS; \
         CREATE blobs:1 = { payload: 0x00ff10, weight: 1.5 };";
    let (address, _stop, _db) = serving_with(&format!("{SETUP} {setup}")).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client
        .parse("", "SELECT weight, payload FROM blobs", &[701, 17])
        .await;
    client.bind("", "", &[], &[], &[1, 1]).await;
    client.execute("", 0).await;
    client.sync().await;
    client.message().await; // ParseComplete
    client.message().await; // BindComplete
    // The row description names the binary formats before the rows.
    let (kind, body) = raw(&mut client).await;
    assert_eq!(kind, b'T', "RowDescription first: {kind}");
    let format_at = body.len().saturating_sub(2);
    let format = u16::from_be_bytes(
        body[format_at..]
            .try_into()
            .expect("the trailing format word"),
    );
    assert_eq!(format, 1, "the column answers binary: {body:?}");
    let row = row_of(&mut client).await;
    assert_eq!(
        row,
        vec![
            Some(1.5_f64.to_be_bytes().to_vec()),
            Some(vec![0x00, 0xff, 0x10]),
        ]
    );
}

#[tokio::test]
async fn a_type_outside_the_subset_refuses_by_name() {
    let setup = "DEFINE TABLE moments SCHEMALESS; \
         CREATE moments:1 = { at: datetime '2026-11-16T12:00:00Z' };";
    let (address, _stop, _db) = serving_with(&format!("{SETUP} {setup}")).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    // A binary parameter of a type the subset does not speak.
    client
        .parse("", "SELECT at FROM moments WHERE at = $1", &[1184])
        .await;
    client.bind("", "", &[1], &[Some(vec![0; 8])], &[]).await;
    client.sync().await;
    client.message().await; // ParseComplete
    let error = client.message().await;
    assert!(
        matches!(&error, Seen::Error(code, _) if code == "0A000"),
        "{error:?}"
    );

    // A binary result over a column the subset does not speak.
    client.parse("", "SELECT at FROM moments", &[]).await;
    client.bind("", "", &[], &[], &[1]).await;
    client.execute("", 0).await;
    client.sync().await;
    client.message().await; // Ready, closing the first half's Sync

    // A binary result over a column the subset does not speak refuses at
    // encoding — before any row description, so nothing half-answered leaks.
    client.parse("", "SELECT at FROM moments", &[]).await;
    client.bind("", "", &[], &[], &[1]).await;
    client.execute("", 0).await;
    client.sync().await;
    client.message().await; // ParseComplete
    client.message().await; // BindComplete
    let (kind, body) = raw(&mut client).await;
    assert_eq!(kind, b'E', "the refusal names the type: {body:?}");
}

#[tokio::test]
async fn an_sslrequest_without_a_certificate_answers_n_and_the_session_proceeds() {
    let (address, _stop, _db) = serving_with(super::SETUP).await;
    let mut client = Client::to(&address).await;
    // SSLRequest: len 8, code 80877103.
    let mut ssl = Vec::new();
    ssl.extend_from_slice(&8_u32.to_be_bytes());
    ssl.extend_from_slice(&80877103_u32.to_be_bytes());
    client.write(&ssl).await;
    let answer = client.read(1).await;
    assert_eq!(answer, vec![b'N'], "no certificate: the dialect continues");
    let seen = client.start(None, "app.main", None).await;
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
        "startup after the N answers: {seen:?}"
    );
    let seen = client.query("SELECT name FROM orders LIMIT 1").await;
    assert_eq!(super::rows(&seen), vec![vec![Some("first".to_owned())]]);
}
