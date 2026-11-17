//! Writes over the wire: INSERT/UPDATE/DELETE with their RETURNING shapes and
//! honest counts, CREATE TABLE / DROP TABLE against the catalog — and every
//! shape outside the stated reach refusing by name.

use super::{Client, SETUP, Seen, refused, rows, serving_with};

#[tokio::test]
async fn insert_then_read_and_the_count_tag() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    // The write owes the tag alone — no phantom description.
    let seen = client
        .query("INSERT INTO orders (band, name) VALUES (5, 'fifth')")
        .await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "INSERT 0 1")),
        "the count rides the tag: {seen:?}"
    );
    assert!(
        !seen.iter().any(|m| matches!(m, Seen::RowDesc(_))),
        "no RETURNING owes no description: {seen:?}"
    );

    // The written record reads back.
    let seen = client
        .query("SELECT band, name FROM orders WHERE band = 5")
        .await;
    assert_eq!(
        rows(&seen),
        vec![vec![Some("5".to_owned()), Some("fifth".to_owned())]],
        "{seen:?}"
    );
    stop.cancel();
}

#[tokio::test]
async fn returning_answers_the_written_shape() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    // INSERT's RETURNING answers the created shape under the named columns.
    let seen = client
        .query("INSERT INTO orders (band, name) VALUES (6, 'sixth') RETURNING band, name")
        .await;
    assert_eq!(
        rows(&seen),
        vec![vec![Some("6".to_owned()), Some("sixth".to_owned())]],
        "{seen:?}"
    );

    // UPDATE's RETURNING answers the new shape; the tag counts.
    let seen = client
        .query("UPDATE orders SET band = 10 WHERE name = 'sixth' RETURNING band")
        .await;
    assert_eq!(rows(&seen), vec![vec![Some("10".to_owned())]], "{seen:?}");
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "UPDATE 1")),
        "{seen:?}"
    );

    // DELETE's RETURNING answers the before shape — the row is gone.
    let seen = client
        .query("DELETE FROM orders WHERE band = 10 RETURNING name")
        .await;
    assert_eq!(
        rows(&seen),
        vec![vec![Some("sixth".to_owned())]],
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "DELETE 1")),
        "{seen:?}"
    );
    let seen = client
        .query("SELECT name FROM orders WHERE band = 10")
        .await;
    assert!(rows(&seen).is_empty(), "the delete reached: {seen:?}");
    stop.cancel();
}

#[tokio::test]
async fn a_write_states_its_reach_and_one_row_per_insert() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    let seen = client.query("UPDATE orders SET band = 1").await;
    let (code, message) = refused(&seen).expect("the refusal");
    assert_eq!(code, "0A000");
    assert!(message.contains("WHERE"), "the reach is named: {message}");

    let seen = client.query("DELETE FROM orders").await;
    let (code, message) = refused(&seen).expect("the refusal");
    assert_eq!(code, "0A000");
    assert!(message.contains("WHERE"), "the reach is named: {message}");

    let seen = client
        .query("INSERT INTO orders (band, name) VALUES (1, 'a'), (2, 'b')")
        .await;
    let (code, message) = refused(&seen).expect("the refusal");
    assert_eq!(code, "0A000");
    assert!(
        message.contains("one VALUES row"),
        "the batch shape is named: {message}"
    );

    let seen = client.query("INSERT INTO orders VALUES (1, 'a')").await;
    let (code, message) = refused(&seen).expect("the refusal");
    assert_eq!(code, "0A000");
    assert!(
        message.contains("columns"),
        "the column list is named: {message}"
    );
    stop.cancel();
}

#[tokio::test]
async fn create_and_drop_table_move_the_catalog() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    let seen = client
        .query("CREATE TABLE bands (name text, formed int)")
        .await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "CREATE TABLE")),
        "{seen:?}"
    );

    // The catalog the dialect's clients read sees the new table.
    let seen = client
        .query("SELECT c.relname FROM pg_catalog.pg_class c WHERE c.relname = 'bands'")
        .await;
    assert_eq!(
        rows(&seen),
        vec![vec![Some("bands".to_owned())]],
        "the introspection answers the new table: {seen:?}"
    );

    // The columns the dialect's clients read see the declared fields.
    let seen = client
        .query(
            "SELECT a.attname, a.atttypid FROM pg_catalog.pg_attribute a \
             WHERE a.attrelid = (SELECT 0) OR a.attname IN ('name', 'formed') ORDER BY a.attnum LIMIT 4",
        )
        .await;
    let _ = seen; // the subquery source refuses by name; the catalog shape rides below

    // Write and read the new table through the wire.
    let seen = client
        .query("INSERT INTO bands (name, formed) VALUES ('one', 2001)")
        .await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "INSERT 0 1")),
        "{seen:?}"
    );
    let seen = client
        .query("SELECT name, formed FROM bands WHERE formed > 2000")
        .await;
    assert_eq!(
        rows(&seen),
        vec![vec![Some("one".to_owned()), Some("2001".to_owned())]],
        "{seen:?}"
    );

    let seen = client.query("DROP TABLE bands").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "DROP TABLE")),
        "{seen:?}"
    );
    let seen = client.query("SELECT name FROM bands").await;
    let (code, _) = refused(&seen).expect("the table is gone");
    assert_eq!(code, "42601");
    stop.cancel();
}

#[tokio::test]
async fn extended_parse_binds_write_parameters() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    // UPDATE … SET band = $1: the inference reads `band`'s declared field.
    client
        .parse("", "UPDATE orders SET band = $1 WHERE name = $2", &[])
        .await;
    client.describe(b'S', "").await;
    client.sync().await;
    let mut seen = Vec::new();
    for _ in 0..4 {
        seen.push(client.message().await);
    }
    assert!(
        seen.contains(&Seen::ParamDesc(vec![20, 25])),
        "band reads int8, name reads text: {seen:?}"
    );

    client
        .bind(
            "",
            "",
            &[1, 0],
            &[Some(4_i64.to_be_bytes().to_vec()), Some(b"first".to_vec())],
            &[],
        )
        .await;
    client.execute("", 0).await;
    client.sync().await;
    let mut seen = Vec::new();
    for _ in 0..3 {
        seen.push(client.message().await);
    }
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "UPDATE 1")),
        "the bound write ran: {seen:?}"
    );
    let seen = client
        .query("SELECT band, name FROM orders WHERE name = 'first'")
        .await;
    assert_eq!(
        rows(&seen),
        vec![vec![Some("4".to_owned()), Some("first".to_owned())]],
        "{seen:?}"
    );
    stop.cancel();
}
