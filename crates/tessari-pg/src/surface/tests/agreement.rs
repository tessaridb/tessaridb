//! C2 and C3 together: the type surface round-trips through the port with
//! the mapping table's documented forms (pinned exactly where the table
//! spells them), and the same reads through the port and through TessariQL
//! agree row for row.

use super::{Client, SETUP, rows, serving_with};

/// One record holding a spread of the store's value kinds.
const SPREAD: &str = "DEFINE TABLE spread SCHEMALESS; \
     CREATE spread:1 = { b: true, i: 42, f: 4.5, d: dec 12.34, s: 'héllo', \
     bytes: 0x00ff, at: datetime '2026-11-16T12:00:00Z', span: 1h30m, \
     u: uuid '550e8400-e29b-41d4-a716-446655440000', obj: { x: 1 }, arr: [1, 2] };";

#[tokio::test]
async fn round_trip_vectors_carry_the_mapping_tables_forms() {
    let (address, _stop, _db) = serving_with(&format!("{SETUP} {SPREAD}")).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    let seen = client
        .query("SELECT b, i, f, d, s, bytes, at, span, u, obj, arr FROM spread")
        .await;
    let got = rows(&seen);
    assert_eq!(got.len(), 1, "{seen:?}");
    let row = &got[0];
    // The forms the mapping table spells, pinned exactly.
    assert_eq!(row[0].as_deref(), Some("t"), "bool");
    assert_eq!(row[1].as_deref(), Some("42"), "int answers as the widest");
    assert_eq!(row[3].as_deref(), Some("12.34"), "decimal keeps its digits");
    assert_eq!(row[4].as_deref(), Some("héllo"), "string verbatim");
    assert_eq!(row[5].as_deref(), Some("\\x00ff"), "bytea hex");
    assert_eq!(
        row[8].as_deref(),
        Some("550e8400-e29b-41d4-a716-446655440000"),
        "uuid hyphenated"
    );
    assert_eq!(row[9].as_deref(), Some("{\"x\":1}"), "object as JSON");
    assert_eq!(row[10].as_deref(), Some("[1,2]"), "array as JSON");
    // The float reads back as the same value.
    let back: f64 = row[2].as_deref().unwrap_or_default().parse().unwrap_or(0.0);
    assert!((back - 4.5).abs() < 1e-9, "float: {row:?}");
    // The instant and the span cross as text and re-parse by shape.
    assert!(
        row[6]
            .as_deref()
            .is_some_and(|at| at.chars().next().is_some_and(|c| c.is_ascii_digit())),
        "datetime text: {row:?}"
    );
    assert!(
        row[7]
            .as_deref()
            .is_some_and(|span| span.ends_with('s') || span.contains('h')),
        "duration text: {row:?}"
    );
}

#[tokio::test]
async fn binary_vectors_carry_the_fixed_scalars_exactly() {
    let (address, _stop, _db) = serving_with(&format!("{SETUP} {SPREAD}")).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client
        .parse("", "SELECT b, i, f, bytes FROM spread", &[16, 20, 701, 17])
        .await;
    client.bind("", "", &[], &[], &[1, 1, 1, 1]).await;
    client.execute("", 0).await;
    client.sync().await;

    // Binary cells are not UTF-8: the row is read raw, byte for byte.
    let mut data = Vec::new();
    loop {
        let head = client.read(5).await;
        let len = u32::from_be_bytes(head[1..].try_into().expect("four bytes"));
        let len = usize::try_from(len.saturating_sub(4)).expect("a frame length");
        let body = client.read(len).await;
        match head[0] {
            b'D' => data = raw_cells(&body),
            b'C' => break,
            b'E' => {
                assert_eq!(0, 1, "an error where the row was expected: {body:?}");
            }
            _ => {}
        }
    }
    let cell = |n: usize| data[n].clone().unwrap_or_default();
    assert_eq!(cell(0), vec![1], "bool true is one byte 0x01");
    assert_eq!(cell(1), 42_i64.to_be_bytes().to_vec(), "int8 big-endian");
    assert_eq!(cell(2), 4.5_f64.to_be_bytes().to_vec(), "float8 big-endian");
    assert_eq!(cell(3), vec![0x00, 0xff], "bytea raw");
}

/// One DataRow body decoded into cells that stay bytes.
fn raw_cells(body: &[u8]) -> Vec<Option<Vec<u8>>> {
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

/// The differential core: one read through the port and through TessariQL
/// must agree. Direct rows render through the same correspondence the port
/// uses — two independent paths, one pinned at the vectors above.
async fn agree(setup: &str, sql: &str) {
    let (address, stop, db) = serving_with(setup).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;
    let port = rows(&client.query(sql).await);

    let mut direct = db.session();
    let outcomes = direct
        .run(&format!("USE NAMESPACE app; USE DATABASE main; {sql}"))
        .await
        .expect("the same read runs directly");
    // The port's star expands a record's fields into columns; the direct
    // read renders through the same correspondence — it IS the mapping —
    // so the comparison is the two read paths, not two renderers.
    let mut direct: Vec<Vec<Option<String>>> = Vec::new();
    for outcome in &outcomes {
        if let tessaridb::Outcome::Records { records, .. } = outcome {
            let (_, values) =
                super::super::rows(records, &super::super::translate::Columns::Star, None);
            for row in &values {
                direct.push(
                    row.iter()
                        .map(|cell| cell.as_ref().and_then(super::super::types::text_of))
                        .collect(),
                );
            }
        }
    }
    stop.cancel();
    assert_eq!(
        port.len(),
        direct.len(),
        "the port and the direct read answer the same statements: {sql}"
    );
    for (left, right) in port.iter().zip(&direct) {
        assert_eq!(
            left, right,
            "the port and TessariQL agree on every row: {sql}"
        );
    }
}

#[tokio::test]
async fn the_port_and_tessariql_agree_on_the_bi_baseline() {
    let people = "DEFINE TABLE people SCHEMALESS; \
         CREATE people:1 = { name: 'first', city: 'lyon' }; \
         CREATE people:2 = { name: 'third', city: 'paris' };";
    let setup = format!("{SETUP} {people}");

    agree(&setup, "SELECT * FROM orders").await;
    agree(
        &setup,
        "SELECT band, count(*) AS n FROM orders GROUP BY band ORDER BY band",
    )
    .await;
    agree(
        &setup,
        "SELECT people.city AS city, orders.name AS name FROM people \
         JOIN orders ON people.name = orders.name ORDER BY city",
    )
    .await;
    agree(
        &setup,
        "SELECT name FROM orders WHERE band IN (SELECT band FROM orders WHERE band = 3) ORDER BY name NULLS LAST",
    )
    .await;
    agree(&setup, "SELECT band FROM orders ORDER BY band DESC LIMIT 2").await;
    agree(
        &setup,
        "SELECT band, count(*) AS n FROM orders GROUP BY band HAVING count(*) > 1",
    )
    .await;
}
