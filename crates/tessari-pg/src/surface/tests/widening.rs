//! The wider SQL shape (D7): joins on a value, the aggregate baseline,
//! GROUP BY/HAVING, ORDER BY, and the subquery IN — each mapped onto the
//! engine's own spelling, each shape outside it refused by name.

use super::{Client, Seen, rows, serving_with};

/// Customers beside the orders fixture: two match the orders by name, one
/// matches nothing, so both join directions have something to say.
const PEOPLE: &str = "DEFINE TABLE people SCHEMALESS; \
     CREATE people:1 = { name: 'first', city: 'lyon' }; \
     CREATE people:2 = { name: 'third', city: 'paris' }; \
     CREATE people:3 = { name: 'nobody', city: 'rome' };";

#[tokio::test]
async fn aggregates_fold_the_table() {
    let (address, _stop, _db) = serving_with(super::SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    let seen = client
        .query("SELECT count(*) AS n, sum(band) AS total, min(band) AS low, max(band) AS high FROM orders")
        .await;
    assert_eq!(
        rows(&seen),
        vec![vec![
            Some("3".to_owned()),
            Some("7".to_owned()),
            Some("1".to_owned()),
            Some("3".to_owned()),
        ]],
        "{seen:?}"
    );
}

#[tokio::test]
async fn avg_reads_as_the_engine_mean() {
    let (address, _stop, _db) = serving_with(super::SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    let seen = client.query("SELECT avg(band) AS a FROM orders").await;
    let got = rows(&seen);
    assert_eq!(got.len(), 1, "{seen:?}");
    let cell = got[0][0].clone().unwrap_or_default();
    let parsed: f64 = cell.parse().expect("a float's text");
    assert!(
        (parsed - 7.0_f64 / 3.0).abs() < 1e-9,
        "mean of 1,3,3: {cell}"
    );
}

#[tokio::test]
async fn group_by_having_and_order_by() {
    let (address, _stop, _db) = serving_with(super::SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    let seen = client
        .query("SELECT band, count(*) AS n FROM orders GROUP BY band HAVING count(*) > 1 ORDER BY band DESC")
        .await;
    assert_eq!(
        rows(&seen),
        vec![vec![Some("3".to_owned()), Some("2".to_owned())]],
        "{seen:?}"
    );

    let seen = client
        .query("SELECT band FROM orders ORDER BY band ASC")
        .await;
    assert_eq!(
        rows(&seen),
        vec![
            vec![Some("1".to_owned())],
            vec![Some("3".to_owned())],
            vec![Some("3".to_owned())],
        ],
        "{seen:?}"
    );
}

#[tokio::test]
async fn count_distinct_reads() {
    let (address, _stop, _db) = serving_with(super::SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    let seen = client
        .query("SELECT count(DISTINCT band) AS kinds FROM orders")
        .await;
    assert_eq!(rows(&seen), vec![vec![Some("2".to_owned())]], "{seen:?}");
}

#[tokio::test]
async fn a_join_matches_on_the_value() {
    let (address, _stop, _db) = serving_with(&format!("{} {}", super::SETUP, PEOPLE)).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    let seen = client
        .query(
            "SELECT people.city AS city, orders.name AS name FROM people \
             JOIN orders ON people.name = orders.name ORDER BY people.city",
        )
        .await;
    assert_eq!(
        rows(&seen),
        vec![
            vec![Some("lyon".to_owned()), Some("first".to_owned())],
            vec![Some("paris".to_owned()), Some("third".to_owned())],
        ],
        "{seen:?}"
    );
}

#[tokio::test]
async fn a_left_join_keeps_what_nothing_matched() {
    let (address, _stop, _db) = serving_with(&format!("{} {}", super::SETUP, PEOPLE)).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    let seen = client
        .query(
            "SELECT people.city AS city, orders.name AS name FROM people \
             LEFT JOIN orders ON people.name = orders.name ORDER BY people.city",
        )
        .await;
    assert_eq!(
        rows(&seen),
        vec![
            vec![Some("lyon".to_owned()), Some("first".to_owned())],
            vec![Some("paris".to_owned()), Some("third".to_owned())],
            vec![Some("rome".to_owned()), None],
        ],
        "the unmatched side reads as the dialect's NULL: {seen:?}"
    );
}

#[tokio::test]
async fn in_takes_the_subquery_and_refuses_the_value_list() {
    let (address, _stop, _db) = serving_with(&format!("{} {}", super::SETUP, PEOPLE)).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    let seen = client
        .query("SELECT name FROM orders WHERE name IN (SELECT name FROM people) ORDER BY name")
        .await;
    assert_eq!(
        rows(&seen),
        vec![
            vec![Some("first".to_owned())],
            vec![Some("third".to_owned())]
        ],
        "{seen:?}"
    );

    let seen = client
        .query("SELECT name FROM orders WHERE band IN (1, 3)")
        .await;
    let (code, _) = super::refused(&seen).unwrap_or_default();
    assert_eq!(code, "0A000", "a value list is not spoken: {seen:?}");
}

#[tokio::test]
async fn order_by_nulls_last() {
    let (address, _stop, _db) = serving_with(super::SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    let seen = client
        .query("SELECT name FROM orders ORDER BY name ASC NULLS LAST")
        .await;
    assert_eq!(
        rows(&seen),
        vec![
            vec![Some("first".to_owned())],
            vec![Some("third".to_owned())],
            vec![None],
        ],
        "{seen:?}"
    );
}

#[tokio::test]
async fn a_subquery_source_refuses_by_name() {
    let (address, _stop, _db) = serving_with(super::SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    let seen = client
        .query("SELECT * FROM (SELECT band FROM orders) AS t")
        .await;
    let (code, _) = super::refused(&seen).unwrap_or_default();
    assert_eq!(code, "0A000", "{seen:?}");
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
        "the refusal still answers with readiness: {seen:?}"
    );
}
