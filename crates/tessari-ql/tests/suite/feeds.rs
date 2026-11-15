//! `DEFINE FEED` — the declared capture of a table's writes into a topic
//! (FEAT-S1, plan `.bgv/projects/tessaridb/plans/2026-11-15/123001`).

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use tessari_ql::{FeedImage, StatementKind, parse};
use tessari_types::WriteKind;

fn define_feed(source: &str) -> StatementKind {
    let script = parse(source).unwrap_or_else(|error| panic!("{source}\n  failed: {error}"));
    match script
        .statements
        .into_iter()
        .next()
        .map(|statement| statement.kind)
    {
        Some(kind @ StatementKind::DefineFeed { .. }) => kind,
        other => panic!("{source} parsed as {other:?}"),
    }
}

#[test]
fn minimal_feed_parses() {
    match define_feed("DEFINE FEED orders_feed ON orders INTO order_events;") {
        StatementKind::DefineFeed {
            name,
            table,
            topic,
            on,
            image,
            when,
            initial_scan,
            if_not_exists,
        } => {
            assert_eq!(name.text, "orders_feed");
            assert_eq!(table.name.text, "orders");
            assert_eq!(topic.name.text, "order_events");
            assert_eq!(on, WriteKind::ALL);
            assert_eq!(image, FeedImage::Full);
            assert_eq!(when, None);
            assert!(!initial_scan);
            assert!(!if_not_exists);
        }
        other => panic!("parsed as {other:?}"),
    }
}

#[test]
fn full_feed_parses() {
    match define_feed(
        "DEFINE FEED IF NOT EXISTS f ON orders INTO order_events FOR CREATE, UPDATE \
         IMAGE KEYS WHEN $after.total > 100 WITH INITIAL SCAN;",
    ) {
        StatementKind::DefineFeed {
            on,
            image,
            when,
            initial_scan,
            if_not_exists,
            ..
        } => {
            assert_eq!(on, [WriteKind::Create, WriteKind::Update]);
            assert_eq!(image, FeedImage::Keys);
            assert_eq!(when.as_deref(), Some("$after.total > 100"));
            assert!(initial_scan);
            assert!(if_not_exists);
        }
        other => panic!("parsed as {other:?}"),
    }
}

#[test]
fn for_delete_only_parses() {
    match define_feed("DEFINE FEED f ON orders INTO order_events FOR DELETE;") {
        StatementKind::DefineFeed { on, .. } => assert_eq!(on, [WriteKind::Delete]),
        other => panic!("parsed as {other:?}"),
    }
}

#[test]
fn feed_is_a_contextual_word_a_table_keeps_its_name() {
    // `feed` is an ordinary table name, as `event` and `view` are: defining a
    // table called `feed` must not be claimed by the feed statement.
    let script =
        parse("DEFINE TABLE feed (name string);").unwrap_or_else(|error| panic!("failed: {error}"));
    match script.statements.into_iter().next().map(|s| s.kind) {
        Some(StatementKind::DefineTable { .. }) => {}
        other => panic!("parsed as {other:?}"),
    }
}

#[test]
fn image_without_a_mode_is_refused_where_written() {
    assert!(parse("DEFINE FEED f ON orders INTO order_events IMAGE;").is_err());
}

fn first(source: &str) -> StatementKind {
    let script = parse(source).unwrap_or_else(|error| panic!("{source}\n  failed: {error}"));
    match script.statements.into_iter().next() {
        Some(statement) => statement.kind,
        None => panic!("{source} parsed empty"),
    }
}

#[test]
fn drop_feed_parses() {
    match first("DROP FEED orders_feed ON orders;") {
        StatementKind::DropFeed { name, table } => {
            assert_eq!(name.text, "orders_feed");
            assert_eq!(table.name.text, "orders");
        }
        other => panic!("parsed as {other:?}"),
    }
}

#[test]
fn drop_feed_if_exists_parses() {
    match first("DROP FEED IF EXISTS orders_feed ON orders;") {
        StatementKind::DropIfExists(inner) => match *inner {
            StatementKind::DropFeed { name, .. } => assert_eq!(name.text, "orders_feed"),
            other => panic!("wrapped {other:?}"),
        },
        other => panic!("parsed as {other:?}"),
    }
}

#[test]
fn missing_into_is_refused_naming_it() {
    let error = parse("DEFINE FEED f ON orders;").unwrap_err();
    assert!(
        error.to_string().contains("INTO"),
        "refusal names INTO: {error}"
    );
}
