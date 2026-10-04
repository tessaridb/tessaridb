use tessari_types::Sequence;

use super::*;

const ONE_NODE: [u8; NODE_ID_LEN] = [1; NODE_ID_LEN];
const ANOTHER_NODE: [u8; NODE_ID_LEN] = [2; NODE_ID_LEN];
const A_THIRD_NODE: [u8; NODE_ID_LEN] = [3; NODE_ID_LEN];
const A_FOURTH_NODE: [u8; NODE_ID_LEN] = [4; NODE_ID_LEN];

/// Two writes to one record, made on two nodes with no clock agreement.
///
/// This is the criterion the goal is willing to be killed by: if the third
/// answer cannot be produced here, detection was never distinct from picking
/// a winner.
#[test]
fn two_writes_neither_of_which_saw_the_other_are_concurrent() {
    let mut ours = CausalStamp::new();
    ours.advance(ONE_NODE);

    let mut theirs = CausalStamp::new();
    theirs.advance(ANOTHER_NODE);

    assert_eq!(ours.compare(&theirs), CausalOrder::Concurrent);
    assert_eq!(theirs.compare(&ours), CausalOrder::Concurrent);
}

/// The falsification arm: compare what the nodes know locally instead.
///
/// Each node's own log has advanced at its own rate, so the two positions
/// are drawn from unrelated counters and their order says nothing about what
/// happened. It is nonetheless a definite order — which is precisely one
/// write winning in silence, and precisely what the stamp exists to refuse.
#[test]
fn comparing_the_node_local_versions_instead_names_a_winner_that_does_not_exist() {
    let our_version = Sequence::new(41);
    let their_version = Sequence::new(17);

    assert_eq!(our_version.cmp(&their_version), Ordering::Greater);
    assert_ne!(our_version.cmp(&their_version), Ordering::Equal);

    let mut ours = CausalStamp::new();
    ours.advance(ONE_NODE);
    let mut theirs = CausalStamp::new();
    theirs.advance(ANOTHER_NODE);

    assert_eq!(ours.compare(&theirs), CausalOrder::Concurrent);

    // Nothing about what either writer had seen has changed. Only our own
    // log's unrelated position has — and the local comparison hands the
    // record to the other node instead, just as confidently. The stamp does
    // not move, because nothing it measures moved.
    let our_position_had_the_log_been_quieter = Sequence::new(4);

    assert_eq!(
        our_position_had_the_log_been_quieter.cmp(&their_version),
        Ordering::Less
    );
    assert_eq!(ours.compare(&theirs), CausalOrder::Concurrent);
}

#[test]
fn a_writer_that_had_seen_the_other_write_stands_after_it() {
    let mut earlier = CausalStamp::new();
    earlier.advance(ONE_NODE);

    let mut later = earlier.clone();
    later.advance(ANOTHER_NODE);

    assert_eq!(later.compare(&earlier), CausalOrder::After);
    assert_eq!(earlier.compare(&later), CausalOrder::Before);
}

#[test]
fn the_same_writes_compare_as_the_same() {
    let mut ours = CausalStamp::new();
    ours.advance(ONE_NODE);
    ours.advance(ANOTHER_NODE);

    let theirs = ours.clone();

    assert_eq!(ours.compare(&theirs), CausalOrder::Same);
}

#[test]
fn a_node_the_stamp_does_not_name_counts_as_zero() {
    let mut ours = CausalStamp::new();
    ours.advance(ONE_NODE);

    assert_eq!(ours.count(&ONE_NODE), 1);
    assert_eq!(ours.count(&ANOTHER_NODE), 0);
    assert_eq!(ours.compare(&CausalStamp::new()), CausalOrder::After);
    assert_eq!(CausalStamp::new().compare(&ours), CausalOrder::Before);
}

#[test]
fn repeated_writes_from_one_node_stay_one_entry_and_stay_ordered() {
    let mut ours = CausalStamp::new();
    ours.advance(A_THIRD_NODE);
    ours.advance(ONE_NODE);
    ours.advance(ONE_NODE);

    assert_eq!(ours.len(), 2);
    assert_eq!(ours.count(&ONE_NODE), 2);
    assert!(
        ours.entries().windows(2).all(|pair| {
            pair.first().map(|(node, _)| *node) < pair.get(1).map(|(node, _)| *node)
        })
    );
}

/// S1.2 property (a): the entry count is bounded by the NODE set.
///
/// Falsification, stated because the test cannot contain it: key the entries
/// on the write rather than on the node and this count grows with every
/// write instead of standing still. That is the client-keyed vector clock
/// whose recorded field failure is sibling explosion, and it is the reason
/// the key here is the node.
#[test]
fn writing_again_from_the_same_node_adds_no_entry() {
    let mut stamp = CausalStamp::new();
    for _ in 0..8 {
        stamp.advance(ONE_NODE);
    }

    assert_eq!(stamp.len(), 1);
    assert_eq!(stamp.count(&ONE_NODE), 8);

    for _ in 0..8 {
        stamp.advance(ANOTHER_NODE);
        stamp.advance(A_THIRD_NODE);
    }

    assert_eq!(stamp.len(), 3, "three nodes have written, so three entries");
}

/// S1.2 property (b): a write that saw every version leaves exactly one.
///
/// Falsification: remove the `retain` in `record` and the count keeps
/// growing every round, which is the sibling explosion this is here to
/// refuse.
#[test]
fn a_write_that_saw_every_version_leaves_exactly_one() {
    let mut ours = CausalStamp::new();
    ours.advance(ONE_NODE);
    let mut theirs = CausalStamp::new();
    theirs.advance(ANOTHER_NODE);
    let mut a_third = CausalStamp::new();
    a_third.advance(A_THIRD_NODE);

    let mut versions = CausalVersions::new();
    versions.record(ours.clone());
    versions.record(theirs.clone());
    versions.record(a_third.clone());

    assert_eq!(versions.len(), 3, "three writers saw none of each other");
    assert!(versions.is_contested());

    // A fourth writer reads all three, so its stamp carries all three counts
    // and then its own.
    let mut having_seen_all_three = CausalStamp::new();
    having_seen_all_three.advance(ONE_NODE);
    having_seen_all_three.advance(ANOTHER_NODE);
    having_seen_all_three.advance(A_THIRD_NODE);
    having_seen_all_three.advance(A_FOURTH_NODE);

    versions.record(having_seen_all_three);

    assert_eq!(versions.len(), 1);
    assert!(!versions.is_contested());
}

#[test]
fn a_version_the_record_has_already_moved_past_is_not_re_admitted() {
    let mut earlier = CausalStamp::new();
    earlier.advance(ONE_NODE);
    let mut later = earlier.clone();
    later.advance(ANOTHER_NODE);

    let mut versions = CausalVersions::new();
    versions.record(later);
    versions.record(earlier);

    assert_eq!(versions.len(), 1);
}

#[test]
fn descends_agrees_with_compare_on_every_relation() {
    let mut earlier = CausalStamp::new();
    earlier.advance(ONE_NODE);
    let mut later = earlier.clone();
    later.advance(ANOTHER_NODE);
    let mut elsewhere = CausalStamp::new();
    elsewhere.advance(A_THIRD_NODE);

    assert!(later.descends(&earlier));
    assert!(earlier.descends(&earlier));
    assert!(!earlier.descends(&later));
    assert!(!elsewhere.descends(&earlier));
    assert!(!earlier.descends(&elsewhere));
}

#[test]
fn a_fresh_stamp_names_nobody() {
    let stamp = CausalStamp::new();

    assert!(stamp.is_empty());
    assert_eq!(stamp.len(), 0);
    assert_eq!(stamp.compare(&CausalStamp::new()), CausalOrder::Same);
}
