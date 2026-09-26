#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use tessari_types::{DatabaseId, NamespaceId, Reach, Sequence, ShardId, TableId};

use super::{read, spell};

const NS: NamespaceId = NamespaceId::new(3);
const DB: DatabaseId = DatabaseId::new(4);

#[test]
fn a_cursor_reads_back_as_the_positions_it_spelled() {
    let positions = BTreeMap::from([
        (Reach::Database(NS, DB), Sequence::new(7)),
        (
            Reach::Shard(NS, DB, TableId::new(9), ShardId::new(1)),
            Sequence::new(2),
        ),
        (
            Reach::Shard(NS, DB, TableId::new(9), ShardId::new(2)),
            Sequence::new(0),
        ),
    ]);
    let text = spell(NS, DB, &positions);
    assert_eq!(text, "3.4:d=7,9.1=2,9.2=0");
    assert_eq!(read(&text, NS, DB).unwrap(), positions);
}

#[test]
fn a_cursor_this_build_cannot_read_is_refused_by_name() {
    for text in [
        "",
        "d=1",
        "3.4:",
        "3.4:d",
        "3.4:d=x",
        "3.4:9=1",
        "3.4:9.1.2=1",
        "3.4:d=1,d=2",
    ] {
        let why = read(text, NS, DB).expect_err(text);
        assert!(why.contains("is not a cursor"), "{text}: {why}");
    }
}

#[test]
fn a_cursor_from_another_database_is_refused_rather_than_resumed_from() {
    for text in ["3.5:d=7", "2.4:d=7"] {
        let why = read(text, NS, DB).expect_err(text);
        assert!(
            why.contains("a feed over another database"),
            "{text}: {why}"
        );
    }
}
