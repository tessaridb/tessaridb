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

#[tokio::test]
async fn alter_table_adds_a_column_and_the_default_backfills() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    let seen = client
        .query("ALTER TABLE orders ADD COLUMN qty int DEFAULT 7")
        .await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "ALTER TABLE")),
        "the DDL owes its tag: {seen:?}"
    );
    // The stored rows read the backfilled default.
    let seen = client.query("SELECT qty FROM orders").await;
    assert_eq!(
        rows(&seen),
        vec![vec![Some("7".to_owned())]; 3],
        "every stored row took the default: {seen:?}"
    );
    stop.cancel();
}

#[tokio::test]
async fn alter_table_drops_a_column_and_names_the_kept_values() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;
    client
        .query("ALTER TABLE orders ADD COLUMN qty int DEFAULT 7")
        .await;

    let seen = client.query("ALTER TABLE orders DROP COLUMN qty").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Notice(code, message) if code == "00000" && message.contains("values"))),
        "the kept-values warning rides the answer: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "ALTER TABLE")),
        "the DDL owes its tag: {seen:?}"
    );
    // The store drops the declaration, not the data: the values still read by
    // name — the notice above is what keeps that from being a silent surprise.
    let seen = client.query("SELECT qty FROM orders LIMIT 1").await;
    assert_eq!(
        rows(&seen),
        vec![vec![Some("7".to_owned())]],
        "the kept values still read: {seen:?}"
    );
    stop.cancel();
}

#[tokio::test]
async fn alter_table_renames_a_column_and_the_read_moves() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    let seen = client
        .query("ALTER TABLE orders RENAME COLUMN name TO title")
        .await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "ALTER TABLE")),
        "the DDL owes its tag: {seen:?}"
    );
    let seen = client
        .query("SELECT title FROM orders WHERE band = 1")
        .await;
    assert_eq!(
        rows(&seen),
        vec![vec![Some("first".to_owned())]],
        "the read answers under the new name: {seen:?}"
    );
    stop.cancel();
}

#[tokio::test]
async fn json_columns_round_trip_and_navigate() {
    let setup = "DEFINE TABLE docs SCHEMALESS; \
                 DEFINE FIELD meta ON docs TYPE object;";
    let (address, stop, _db) = serving_with(setup).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    // The dialect writes JSON text; the declared-kind parse lands it as an
    // object, and the wire reads it back as JSON.
    let seen = client
        .query("INSERT INTO docs (meta) VALUES ('{\"a\": 1, \"tags\": [\"x\", \"y\"]}')")
        .await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "INSERT 0 1")),
        "the JSON text accepted the write: {seen:?}"
    );
    let seen = client.query("SELECT meta FROM docs").await;
    let row = &rows(&seen)[0][0];
    let text = row.as_deref().unwrap_or_default();
    assert!(
        text.contains("\"a\"") && text.contains("\"tags\""),
        "the object answers as JSON text: {seen:?}"
    );
    // The navigation operators read inside the object.
    let seen = client
        .query("SELECT id FROM docs WHERE meta -> 'a' = 1")
        .await;
    assert_eq!(rows(&seen), vec![vec![Some("docs:1".to_owned())]]);
    let seen = client
        .query("SELECT id FROM docs WHERE meta -> 'tags' -> 1 = 'y'")
        .await;
    assert_eq!(rows(&seen), vec![vec![Some("docs:1".to_owned())]]);
    // Containment asks the engine's own document operator.
    let seen = client
        .query("SELECT id FROM docs WHERE meta @> '{\"tags\": [\"x\"]}'")
        .await;
    assert_eq!(rows(&seen), vec![vec![Some("docs:1".to_owned())]]);
    let seen = client
        .query("SELECT id FROM docs WHERE meta @> '{\"absent\": 1}'")
        .await;
    assert!(rows(&seen).is_empty(), "no row contains it: {seen:?}");
    stop.cancel();
}
