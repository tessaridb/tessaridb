#![allow(clippy::unwrap_used)]

use super::*;
use crate::error::Error;

fn key(id: RecordId, version: u64) -> RecordKey {
    RecordKey::new(
        NamespaceId::new(1),
        DatabaseId::new(2),
        TableId::new(3),
        id,
        Sequence::new(version),
    )
}

/// A log of `home` whose writer nobody named — the migrated shape.
fn logged(home: Reach) -> LogId {
    LogId::unattributed(home)
}

/// A log of `home` held by a distinguishable writer.
fn by(home: Reach, writer: u8) -> LogId {
    LogId::new(home, Writer::new([writer; NODE_ID_LEN]))
}

#[test]
fn a_record_key_round_trips_every_id_variant() {
    let ids = [
        RecordId::Int(-42),
        RecordId::Int(i64::MAX),
        RecordId::from("user"),
        RecordId::from(""),
        RecordId::Uuid([0x5a; 16]),
        RecordId::Bytes(vec![0x00, 0xff, 0x00]),
    ];
    for id in ids {
        let original = key(id, 7);
        let encoded = original.encode();
        assert_eq!(RecordKey::decode(encoded.as_slice()).unwrap(), original);
    }
}

#[test]
fn the_table_prefix_is_fixed_width_and_leads_every_record_key() {
    let prefix = RecordKey::table_prefix(NamespaceId::new(1), DatabaseId::new(2), TableId::new(3));
    assert_eq!(prefix.len(), TABLE_PREFIX_LEN);
    let encoded = key(RecordId::from("x"), 1).encode();
    assert!(encoded.as_slice().starts_with(&prefix));
}

#[test]
fn every_version_of_a_record_shares_the_versions_prefix() {
    let id = RecordId::from("same");
    let prefix = RecordKey::versions_prefix(
        NamespaceId::new(1),
        DatabaseId::new(2),
        TableId::new(3),
        &id,
    );
    for version in [0, 1, u64::MAX] {
        let encoded = key(id.clone(), version).encode();
        assert!(encoded.as_slice().starts_with(&prefix));
        assert_eq!(encoded.len(), prefix.len().saturating_add(8));
    }
}

#[test]
fn newer_versions_of_a_record_sort_first() {
    let older = key(RecordId::from("r"), 5).encode();
    let newer = key(RecordId::from("r"), 9).encode();
    assert!(newer.as_slice() < older.as_slice());
}

#[test]
fn distinct_records_stay_ordered_despite_the_version_suffix() {
    // Without a terminator on the record id, "a" followed by a version whose
    // first byte exceeds 'b' would sort after "ab". The version chosen here
    // is the one that would trigger it.
    let short = key(RecordId::from("a"), !0x6200_0000_0000_0000_u64).encode();
    let long = key(RecordId::from("ab"), 0).encode();
    assert!(short.as_slice() < long.as_slice());
}

#[test]
fn decoding_a_record_key_as_a_meta_key_is_refused() {
    let encoded = key(RecordId::Int(1), 1).encode();
    let error = FormatVersionKey::decode(encoded.as_slice()).unwrap_err();
    assert!(matches!(error, Error::UnexpectedKind { .. }));
}

#[test]
fn the_format_version_key_is_one_byte_and_round_trips() {
    let format = FormatVersionKey.encode();
    assert_eq!(format.len(), 1);
    assert_eq!(
        FormatVersionKey::decode(format.as_slice()).unwrap(),
        FormatVersionKey
    );
    assert_ne!(
        format.as_slice(),
        AppliedPositionKey::new(logged(Reach::Store))
            .encode()
            .as_slice()
    );
}

#[test]
fn each_log_has_its_own_applied_position() {
    // Not a singleton any more, and that is the whole of the per-range log:
    // a position counts in one log, so the record of how far it has been
    // applied is one per log. A single value would be the counter two
    // leaders both allocate from — and one per HOME would be that same
    // counter again the moment a home admits two writers, which is why the
    // last two entries here share a home and differ only by writer.
    let logs = [
        logged(Reach::Store),
        logged(Reach::Namespace(NamespaceId::new(1))),
        logged(Reach::Database(NamespaceId::new(1), DatabaseId::new(2))),
        logged(Reach::Database(NamespaceId::new(1), DatabaseId::new(3))),
        by(Reach::Database(NamespaceId::new(1), DatabaseId::new(3)), 1),
        by(Reach::Database(NamespaceId::new(1), DatabaseId::new(3)), 2),
    ];
    let mut seen = Vec::new();
    for log in logs {
        let key = AppliedPositionKey::new(log);
        let encoded = key.encode();
        assert_eq!(
            encoded.len(),
            26,
            "the kind tag, the fixed reach and the fixed writer"
        );
        assert_eq!(AppliedPositionKey::decode(encoded.as_slice()).unwrap(), key);
        assert!(!seen.contains(&encoded), "two logs share a position key");
        seen.push(encoded);
    }
}

#[test]
fn a_homes_applied_position_prefix_covers_its_writers_and_no_other_home() {
    let home = Reach::Database(NamespaceId::new(1), DatabaseId::new(2));
    let prefix = AppliedPositionKey::prefix_for_home(home);
    assert_eq!(prefix.len(), 10, "the kind tag plus the fixed reach");
    for writer in [0, 1, 255] {
        let mine = AppliedPositionKey::new(by(home, writer)).encode();
        assert!(mine.as_slice().starts_with(&prefix));
    }
    let sibling = AppliedPositionKey::new(by(
        Reach::Database(NamespaceId::new(1), DatabaseId::new(3)),
        1,
    ))
    .encode();
    assert!(!sibling.as_slice().starts_with(&prefix));
}

#[test]
fn a_singleton_key_with_trailing_bytes_is_refused() {
    let bytes = [KeyKind::FormatVersion.tag(), 0x00];
    assert!(matches!(
        FormatVersionKey::decode(&bytes).unwrap_err(),
        Error::TrailingBytes { extra: 1, .. }
    ));
}

#[test]
fn each_key_type_reports_its_keyspace() {
    assert_eq!(RecordKey::keyspace(), Keyspace::DATA);
    assert_eq!(FormatVersionKey::keyspace(), Keyspace::META);
    assert_eq!(AppliedPositionKey::keyspace(), Keyspace::META);
    assert_eq!(LogKey::keyspace(), Keyspace::LOG);
}

#[test]
fn log_entries_sort_oldest_first_which_is_the_opposite_of_record_versions() {
    let older = LogKey::new(logged(Reach::Store), Sequence::new(5)).encode();
    let newer = LogKey::new(logged(Reach::Store), Sequence::new(9)).encode();
    assert!(
        older.as_slice() < newer.as_slice(),
        "a log reader resumes at a position and walks forward"
    );

    // The same two sequences, as versions of one record, sort the other way.
    let older_version = key(RecordId::from("r"), 5).encode();
    let newer_version = key(RecordId::from("r"), 9).encode();
    assert!(newer_version.as_slice() < older_version.as_slice());
}

#[test]
fn a_log_key_round_trips_and_is_fixed_width() {
    let homes = [
        Reach::Store,
        Reach::Namespace(NamespaceId::new(3)),
        Reach::Database(NamespaceId::new(3), DatabaseId::new(4)),
    ];
    for home in homes {
        for sequence in [0, 1, u64::MAX] {
            let original = LogKey::new(by(home, 9), Sequence::new(sequence));
            let encoded = original.encode();
            assert_eq!(
                encoded.len(),
                34,
                "tag, nine reach bytes, sixteen writer bytes, eight sequence"
            );
            assert_eq!(LogKey::decode(encoded.as_slice()).unwrap(), original);
        }
    }
}

#[test]
fn two_homes_hold_the_same_position_without_colliding() {
    // The whole of what the per-range log buys: two leaders allocate from
    // independent counters, so the same number arrives twice and must land
    // in two places. Before the home was part of the key these two were one
    // key, and the second write silently replaced the first.
    let position = Sequence::new(7);
    let one = LogKey::new(
        logged(Reach::Database(NamespaceId::new(1), DatabaseId::new(2))),
        position,
    );
    let other = LogKey::new(
        logged(Reach::Database(NamespaceId::new(1), DatabaseId::new(3))),
        position,
    );
    assert_ne!(one.encode(), other.encode());
    assert_eq!(LogKey::decode(one.encode().as_slice()).unwrap(), one);
    assert_eq!(LogKey::decode(other.encode().as_slice()).unwrap(), other);
}

#[test]
fn every_log_key_carries_the_log_prefix() {
    let prefix = LogKey::prefix();
    assert_eq!(prefix.len(), 1);
    for sequence in [0, 42, u64::MAX] {
        let encoded = LogKey::new(logged(Reach::Store), Sequence::new(sequence)).encode();
        assert!(encoded.as_slice().starts_with(&prefix));
    }
}

#[test]
fn a_logs_prefix_leads_its_own_entries_and_no_others() {
    let home = Reach::Database(NamespaceId::new(1), DatabaseId::new(2));
    let prefix = LogKey::prefix_for(logged(home));
    assert_eq!(
        prefix.len(),
        26,
        "the kind tag, the fixed reach and the fixed writer"
    );
    for sequence in [0, 42, u64::MAX] {
        let mine = LogKey::new(logged(home), Sequence::new(sequence)).encode();
        assert!(mine.as_slice().starts_with(&prefix));
    }
    // The neighbours a scan over that prefix must not reach: the namespace
    // above it, a sibling database, and the store.
    let strangers = [
        logged(Reach::Namespace(NamespaceId::new(1))),
        logged(Reach::Database(NamespaceId::new(1), DatabaseId::new(3))),
        logged(Reach::Store),
        // And the one the writer adds: the same home, another writer. This
        // is the neighbour a home-wide prefix would have swept in.
        by(home, 1),
    ];
    for stranger in strangers {
        let theirs = LogKey::new(stranger, Sequence::new(42)).encode();
        assert!(!theirs.as_slice().starts_with(&prefix));
    }
}

#[test]
fn two_writers_in_one_home_hold_the_same_position_without_colliding() {
    // The per-range log stopped two RANGES sharing a counter. This is the
    // same failure one level in: two masters on ONE range allocate the same
    // number, and before the writer was part of the key the second write
    // replaced the first with nothing in an error state.
    let home = Reach::Database(NamespaceId::new(1), DatabaseId::new(2));
    let position = Sequence::new(7);
    let one = LogKey::new(by(home, 1), position);
    let other = LogKey::new(by(home, 2), position);
    assert_ne!(one.encode(), other.encode());
    assert_eq!(LogKey::decode(one.encode().as_slice()).unwrap(), one);
    assert_eq!(LogKey::decode(other.encode().as_slice()).unwrap(), other);
}

#[test]
fn a_homes_prefix_covers_every_writer_of_that_home_and_no_other_home() {
    // Which logs a range has is asked of the store, and this is the bound
    // that asks it (Q-632).
    let home = Reach::Database(NamespaceId::new(1), DatabaseId::new(2));
    let prefix = LogKey::prefix_for_home(home);
    assert_eq!(prefix.len(), 10, "the kind tag plus the fixed reach");
    for writer in [0, 1, 255] {
        let mine = LogKey::new(by(home, writer), Sequence::new(3)).encode();
        assert!(mine.as_slice().starts_with(&prefix));
    }
    let sibling = LogKey::new(
        by(Reach::Database(NamespaceId::new(1), DatabaseId::new(3)), 1),
        Sequence::new(3),
    )
    .encode();
    assert!(!sibling.as_slice().starts_with(&prefix));
}

#[test]
fn one_writers_entries_stay_contiguous_and_ascending() {
    // Two properties in one assertion because they are one requirement: a
    // log reader resumes at a position and walks forward, and it must not
    // walk into another writer's numbering on the way.
    let home = Reach::Namespace(NamespaceId::new(4));
    let mine_early = LogKey::new(by(home, 1), Sequence::new(1)).encode();
    let mine_late = LogKey::new(by(home, 1), Sequence::new(u64::MAX)).encode();
    let theirs_early = LogKey::new(by(home, 2), Sequence::new(1)).encode();
    assert!(mine_early.as_slice() < mine_late.as_slice());
    assert!(
        mine_late.as_slice() < theirs_early.as_slice(),
        "a writer's whole log sorts before the next writer's first entry"
    );
}

#[test]
fn the_three_reaches_before_shards_keep_the_bytes_they_always_had() {
    // G031 S2.2: a shard reach is wider, and the others must not move, or
    // every log key already on disk would stop naming its own home.
    let prefix = |home| LogKey::prefix_for_home(home);
    let tag = KeyKind::LogEntry.tag();
    assert_eq!(prefix(Reach::Store), vec![tag, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(
        prefix(Reach::Namespace(NamespaceId::new(0x0102_0304))),
        vec![tag, 1, 1, 2, 3, 4, 0, 0, 0, 0]
    );
    assert_eq!(
        prefix(Reach::Database(NamespaceId::new(5), DatabaseId::new(6))),
        vec![tag, 2, 0, 0, 0, 5, 0, 0, 0, 6]
    );
}

#[test]
fn a_shard_home_round_trips_and_is_its_own_prefix() {
    let shard = |n| {
        Reach::Shard(
            NamespaceId::new(5),
            DatabaseId::new(6),
            TableId::new(7),
            ShardId::new(n),
        )
    };
    let tag = KeyKind::LogEntry.tag();
    assert_eq!(
        LogKey::prefix_for_home(shard(8)),
        vec![tag, 3, 0, 0, 0, 5, 0, 0, 0, 6, 0, 0, 0, 7, 0, 0, 0, 8]
    );
    let mine = LogKey::new(by(shard(8), 1), Sequence::new(3));
    assert_eq!(LogKey::decode(mine.encode().as_slice()).unwrap(), mine);
    let prefix = LogKey::prefix_for_home(shard(8));
    assert!(mine.encode().as_slice().starts_with(&prefix));
    for other in [
        shard(9),
        Reach::Database(NamespaceId::new(5), DatabaseId::new(6)),
    ] {
        let theirs = LogKey::new(by(other, 1), Sequence::new(3)).encode();
        assert!(
            !theirs.as_slice().starts_with(&prefix),
            "{other:?} fell inside the shard's prefix"
        );
    }
    // And the database's own prefix does not reach into its shards' logs:
    // they are separate homes, asked for by name.
    assert!(
        !mine
            .encode()
            .as_slice()
            .starts_with(&LogKey::prefix_for_home(Reach::Database(
                NamespaceId::new(5),
                DatabaseId::new(6)
            )))
    );
}

#[test]
fn an_unknown_reach_variant_is_refused_rather_than_read_as_the_store() {
    // Widening it to the store would file a record this build cannot place
    // into the one log every subscriber reads.
    let mut bytes = LogKey::new(logged(Reach::Store), Sequence::new(1))
        .encode()
        .into_bytes();
    bytes[1] = 0x7f;
    assert!(matches!(
        LogKey::decode(&bytes).unwrap_err(),
        Error::UnknownReach { found: 0x7f, .. }
    ));
}

#[test]
fn a_record_key_is_never_decodable_as_a_log_key() {
    let encoded = key(RecordId::Int(1), 1).encode();
    assert!(matches!(
        LogKey::decode(encoded.as_slice()).unwrap_err(),
        Error::UnexpectedKind { .. }
    ));
}
