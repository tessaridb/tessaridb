//! The extended query protocol over the socket: Parse/Bind/Describe/Execute/
//! Sync cycles, named statements and portals, and the error-skip-to-Sync
//! recovery a client of the dialect expects.

use super::{Client, SETUP, Seen, rows, serving_with};

/// A text parameter value, as the wire carries it.
fn text(value: &str) -> Option<Vec<u8>> {
    Some(value.as_bytes().to_vec())
}

#[tokio::test]
async fn parse_bind_describe_execute_answer_rows() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client
        .parse("", "SELECT name FROM orders WHERE band = $1", &[20])
        .await;
    client.bind("", "", &[], &[text("3")], &[]).await;
    client.describe(b'P', "").await;
    client.execute("", 0).await;
    client.sync().await;

    let mut seen = Vec::new();
    for _ in 0..7 {
        seen.push(client.message().await);
    }
    assert!(
        seen.contains(&Seen::ParseComplete),
        "Parse answers ParseComplete: {seen:?}"
    );
    assert!(
        seen.contains(&Seen::BindComplete),
        "Bind answers BindComplete: {seen:?}"
    );
    assert_eq!(
        seen.iter()
            .filter(|m| matches!(m, Seen::RowDesc(n) if n == &["name"]))
            .count(),
        1,
        "Describe(portal) answers one RowDescription: {seen:?}"
    );
    // The fixture's third row holds band 3 with no name: the condition's two
    // matches are the named row and the NULL one.
    let got = rows(&seen);
    assert_eq!(
        got,
        vec![vec![Some("third".to_owned())], vec![None]],
        "rows: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "SELECT 2")),
        "CommandComplete: {seen:?}"
    );
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
        "Sync owns the single ReadyForQuery: {seen:?}"
    );
}

#[tokio::test]
async fn bind_values_feed_the_condition() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    for (value, want) in [
        ("1", vec![vec![Some("first".to_owned())]]),
        // The fixture's third row holds band 3 with no name: the condition's
        // two matches are the named row and the NULL one.
        ("3", vec![vec![Some("third".to_owned())], vec![None]]),
    ] {
        client
            .parse("", "SELECT name FROM orders WHERE band = $1", &[20])
            .await;
        client.bind("", "", &[], &[text(value)], &[]).await;
        client.execute("", 0).await;
        client.sync().await;
        let mut seen = Vec::new();
        for _ in 0..5 {
            seen.push(client.message().await);
        }
        assert_eq!(rows(&seen), want, "band = {value}: {seen:?}");
    }
}

#[tokio::test]
async fn a_refused_parse_skips_everything_until_sync() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client.parse("", "SELECT FROM", &[]).await;
    client.bind("", "", &[], &[text("3")], &[]).await;
    client.execute("", 0).await;
    client.sync().await;

    let first = client.message().await;
    assert!(
        matches!(first, Seen::Error(_, _)),
        "the refused Parse answers one error: {first:?}"
    );
    let second = client.message().await;
    assert!(
        matches!(second, Seen::Ready(b'I')),
        "the skipped Bind and Execute answer nothing; Sync answers one readiness: {second:?}"
    );
}

#[tokio::test]
async fn describe_statement_carries_the_parameter_description() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client
        .parse("by_band", "SELECT name FROM orders WHERE band = $1", &[20])
        .await;
    client.describe(b'S', "by_band").await;
    client.sync().await;

    let mut seen = Vec::new();
    for _ in 0..4 {
        seen.push(client.message().await);
    }
    assert!(seen.contains(&Seen::ParseComplete), "{seen:?}");
    assert!(
        seen.contains(&Seen::ParamDesc(vec![20])),
        "Describe(statement) answers ParameterDescription: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::RowDesc(n) if n == &["name"])),
        "Describe(statement) answers the projection's RowDescription: {seen:?}"
    );
}

#[tokio::test]
async fn a_named_statement_survives_sync_and_closes_by_name() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client
        .parse("by_band", "SELECT name FROM orders WHERE band = $1", &[20])
        .await;
    client.sync().await;
    client.message().await; // ParseComplete
    client.message().await; // Ready

    client.bind("", "by_band", &[], &[text("1")], &[]).await;
    client.execute("", 0).await;
    client.sync().await;
    let mut seen = Vec::new();
    for _ in 0..4 {
        seen.push(client.message().await);
    }
    assert_eq!(rows(&seen), vec![vec![Some("first".to_owned())]]);

    client.close(b'S', "by_band").await;
    client.sync().await;
    assert_eq!(client.message().await, Seen::CloseComplete);
    assert!(matches!(client.message().await, Seen::Ready(b'I')));

    client.bind("", "by_band", &[], &[text("1")], &[]).await;
    client.sync().await;
    let error = client.message().await;
    assert!(
        matches!(&error, Seen::Error(code, _) if code == "26000"),
        "a closed statement refuses by name: {error:?}"
    );
}

#[tokio::test]
async fn bind_of_a_missing_statement_refuses_by_name() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client.bind("", "nope", &[], &[], &[]).await;
    client.sync().await;
    let error = client.message().await;
    assert!(
        matches!(&error, Seen::Error(code, _) if code == "26000"),
        "{error:?}"
    );
}

#[tokio::test]
async fn parameter_count_mismatch_refuses() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client
        .parse("", "SELECT name FROM orders WHERE band = $1", &[20])
        .await;
    client.bind("", "", &[], &[], &[]).await;
    client.sync().await;
    client.message().await; // ParseComplete
    let error = client.message().await;
    assert!(
        matches!(&error, Seen::Error(code, _) if code == "08P01"),
        "{error:?}"
    );
}

#[tokio::test]
async fn binary_formats_outside_the_subset_refuse_by_name() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    // A binary parameter of a type the subset does not speak refuses where
    // the Bind asks it.
    client
        .parse("", "SELECT name FROM orders WHERE name = $1", &[3802])
        .await;
    client
        .bind("", "", &[1], &[Some(8_i64.to_be_bytes().to_vec())], &[])
        .await;
    client.sync().await;
    client.message().await; // ParseComplete
    let error = client.message().await;
    assert!(
        matches!(&error, Seen::Error(code, _) if code == "0A000"),
        "a binary parameter format the subset does not speak refuses by name: {error:?}"
    );
    client.message().await; // Ready
}

#[tokio::test]
async fn parse_time_inference_reads_the_tables_catalog() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    // The Parse declares no types: Describe(statement) answers the type the
    // port inferred from `band`'s declared field — the dialect's servers
    // infer at Parse time, and the clients bind by the answer.
    client
        .parse("", "SELECT name FROM orders WHERE band = $1", &[])
        .await;
    client.describe(b'S', "").await;
    client.sync().await;
    let mut seen = Vec::new();
    for _ in 0..4 {
        seen.push(client.message().await);
    }
    assert!(
        seen.contains(&Seen::ParamDesc(vec![20])),
        "the inferred type is the declared field's, int8: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::RowDesc(cols) if cols == &["name".to_owned()])),
        "{seen:?}"
    );

    // The driver flow: a binary bind in the inferred shape decodes and reads.
    client
        .bind("", "", &[1], &[Some(3_i64.to_be_bytes().to_vec())], &[])
        .await;
    client.execute("", 0).await;
    client.sync().await;
    let mut seen = Vec::new();
    for _ in 0..5 {
        seen.push(client.message().await);
    }
    assert_eq!(
        rows(&seen),
        vec![vec![Some("third".to_owned())], vec![None]],
        "band 3 with and without a name: {seen:?}"
    );
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
        "the flow closed: {seen:?}"
    );

    // A table the store does not know refuses the Parse in the store's
    // words — the inference asked the catalog, as a real server fails here.
    client
        .parse("", "SELECT id FROM absent WHERE id = $1", &[])
        .await;
    client.sync().await;
    let error = client.message().await;
    assert!(
        matches!(&error, Seen::Error(code, _) if code == "42601"),
        "the store's unknown-table refusal rides the Parse: {error:?}"
    );
}

#[tokio::test]
async fn a_null_parameter_reads_as_the_dialects_null() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client.parse("", "SELECT $1", &[25]).await;
    client.bind("", "", &[], &[None], &[]).await;
    client.execute("", 0).await;
    client.sync().await;
    let mut seen = Vec::new();
    for _ in 0..6 {
        seen.push(client.message().await);
    }
    assert_eq!(
        rows(&seen),
        vec![vec![None]],
        "the two absences read as the dialect's one NULL: {seen:?}"
    );
}

#[tokio::test]
async fn describe_portal_runs_once_and_execute_replays() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client.parse("", "SELECT name FROM orders", &[]).await;
    client.bind("", "", &[], &[], &[]).await;
    client.describe(b'P', "").await;
    client.execute("", 0).await;
    client.execute("", 0).await;
    client.sync().await;
    let mut seen = Vec::new();
    for _ in 0..9 {
        seen.push(client.message().await);
    }
    let got = rows(&seen);
    assert_eq!(
        got,
        vec![
            vec![Some("first".to_owned())],
            vec![Some("third".to_owned())],
            vec![None],
        ],
        "Describe(portal) ran the read; the first Execute replays it: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "SELECT 0")),
        "the second Execute finds the portal spent: {seen:?}"
    );
}

#[tokio::test]
async fn a_row_limited_portal_suspends_and_resumes() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client
        .parse("stmt", "SELECT band FROM orders ORDER BY band", &[])
        .await;
    // A named portal: the unnamed one closes at Sync, a named one holds its
    // window across them.
    client.bind("p", "stmt", &[], &[], &[]).await;
    // Two rows per Execute: the first window suspends, the next answers the
    // rest and the count closes the portal.
    client.execute("p", 2).await;
    client.execute("p", 2).await;
    client.execute("p", 2).await;
    client.sync().await;
    let mut seen = Vec::new();
    for _ in 0..9 {
        seen.push(client.message().await);
    }
    assert!(
        seen.contains(&Seen::ParseComplete) && seen.contains(&Seen::BindComplete),
        "{seen:?}"
    );
    assert_eq!(
        rows(&seen),
        vec![
            vec![Some("1".to_owned())],
            vec![Some("3".to_owned())],
            vec![Some("3".to_owned())],
        ],
        "the windows resume where the last left: {seen:?}"
    );
    assert!(
        seen.iter().filter(|m| matches!(m, Seen::Suspended)).count() == 1,
        "the full window suspends once, the next completes: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "SELECT 3")),
        "the completion names the portal's count: {seen:?}"
    );

    // A spent portal answers an empty completion, never a re-run.
    client.execute("p", 2).await;
    client.sync().await;
    let mut seen = Vec::new();
    for _ in 0..2 {
        seen.push(client.message().await);
    }
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "SELECT 0")),
        "the spent portal answers empty: {seen:?}"
    );
}
