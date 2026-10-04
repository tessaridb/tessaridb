#![allow(clippy::unwrap_used)]

use super::*;

fn text(value: &str) -> RecordId {
    RecordId::Text(value.to_owned())
}

fn map(points: &[&str]) -> ShardMap {
    let points: Vec<_> = points.iter().map(|point| text(point)).collect();
    ShardMap::declared(&points).unwrap().unwrap()
}

#[test]
fn no_points_is_no_map() {
    assert_eq!(ShardMap::declared(&[]).unwrap(), None);
}

#[test]
fn two_points_make_three_shards_numbered_in_key_order() {
    let spans: Vec<_> = map(&["g", "p"]).spans().map(|span| span.id.get()).collect();
    assert_eq!(spans, vec![1, 2, 3]);
}

#[test]
fn a_boundary_belongs_to_the_shard_it_begins() {
    let shards = map(&["g", "p"]);
    assert_eq!(shards.shard_of(&text("a")).get(), 1);
    assert_eq!(shards.shard_of(&text("f")).get(), 1);
    assert_eq!(
        shards.shard_of(&text("g")).get(),
        2,
        "a lower bound is inclusive"
    );
    assert_eq!(shards.shard_of(&text("o")).get(), 2);
    assert_eq!(shards.shard_of(&text("p")).get(), 3);
    assert_eq!(shards.shard_of(&text("zzz")).get(), 3);
}

#[test]
fn identity_kinds_order_as_their_keys_do() {
    // Every integer sorts before every text, and every text before every
    // uuid — the discriminant order the key grammar fixes.
    let points = [RecordId::Int(100), text("m")];
    let shards = ShardMap::declared(&points).unwrap().unwrap();
    assert_eq!(shards.shard_of(&RecordId::Int(-5)).get(), 1);
    assert_eq!(shards.shard_of(&RecordId::Int(100)).get(), 2);
    assert_eq!(shards.shard_of(&text("a")).get(), 2);
    assert_eq!(shards.shard_of(&RecordId::Uuid([0; 16])).get(), 3);
}

#[test]
fn points_out_of_order_or_repeated_are_named_not_sorted() {
    assert_eq!(
        ShardMap::declared(&[text("p"), text("g")]),
        Err(Unsplittable::OutOfOrder { position: 1 })
    );
    assert_eq!(
        ShardMap::declared(&[text("a"), text("g"), text("g")]),
        Err(Unsplittable::OutOfOrder { position: 2 })
    );
}

#[test]
fn a_map_round_trips_through_the_value_the_catalog_holds() {
    let points = [
        RecordId::Int(7),
        text("m"),
        RecordId::Uuid([3; 16]),
        RecordId::Bytes(vec![1, 2]),
    ];
    let shards = ShardMap::declared(&points).unwrap().unwrap();
    assert_eq!(ShardMap::from_value(&shards.to_value()).unwrap(), shards);
}

#[test]
fn spans_report_both_ends_with_the_open_ends_as_none() {
    let shards = map(&["g"]);
    let spans: Vec<_> = shards.spans().collect();
    assert_eq!(spans[0].from, None);
    assert_eq!(spans[0].to, Some(&text("g")));
    assert_eq!(spans[1].from, Some(&text("g")));
    assert_eq!(spans[1].to, None);
}

#[test]
fn a_stored_map_that_is_not_one_map_is_refused_whole() {
    let bad = |value: Value| ShardMap::from_value(&value).is_err();
    assert!(bad(Value::Array(Vec::new())), "empty");
    let shard = |id: i64, from: Option<&str>| {
        let mut fields = BTreeMap::from([("id".to_owned(), Value::from(id))]);
        if let Some(from) = from {
            fields.insert("from".to_owned(), Value::from(from));
        }
        Value::Object(fields)
    };
    assert!(
        bad(Value::Array(vec![shard(1, Some("a"))])),
        "first has a bound"
    );
    assert!(
        bad(Value::Array(vec![shard(1, None), shard(2, None)])),
        "second has none"
    );
    assert!(
        bad(Value::Array(vec![
            shard(1, None),
            shard(2, Some("p")),
            shard(3, Some("g"))
        ])),
        "out of order"
    );
    assert!(
        bad(Value::Array(vec![shard(1, None), shard(1, Some("g"))])),
        "repeated id"
    );
    assert!(
        bad(Value::Array(vec![shard(0, None)])),
        "zero is not a shard"
    );
}
