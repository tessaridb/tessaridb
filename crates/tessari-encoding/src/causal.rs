//! Whether two writes to one record saw each other.
//!
//! A single-leader range never needs this: every write passes through one
//! leader, so the order they were accepted in *is* the order they happened in,
//! and one number expresses it. The moment two nodes may both accept a write to
//! the same record, that stops being true — two writes can each be made in
//! ignorance of the other, and no number drawn from either node's own counter
//! can say so. Comparing those counters still *answers*, which is the danger:
//! it returns a confident before-or-after for a pair that has neither.
//!
//! So the stamp carries what the writer had **seen**, not when it wrote. A node
//! writing to a record increments its own count and carries every other node's
//! count unchanged; that carrying is the whole mechanism. Two stamps then stand
//! in one of four relations, and the fourth — [`CausalOrder::Concurrent`] — is
//! the one this type exists to make expressible.
//!
//! # Nothing here reads a clock
//!
//! Deliberately. The store already ruled that time is a logged value — the
//! session reads the clock once and writes the result, so a replica applies what
//! was written rather than what its own clock says. A stamp that asked a node
//! for the time would reintroduce exactly the disagreement that rule removed,
//! and would do it on the path where the disagreement decides whose data
//! survives.
//!
//! # Why the answer cannot be a tie-break in disguise
//!
//! [`CausalOrder`] has no total order to fall back on. When neither stamp
//! dominates, there is nothing in the type to rank them by, so `Concurrent` is
//! not a verdict the comparison chose over ranking them — it is the only thing
//! it can say. A comparison that could always produce a winner would have been
//! last-writer-wins wearing a detector's name.

use core::cmp::Ordering;

use crate::error::{Error, Result};
use crate::node::NODE_ID_LEN;

/// How one stamp stands to another.
///
/// Four answers rather than three, and the fourth is the point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CausalOrder {
    /// Both stamps record the same writes.
    Same,
    /// Every write this stamp records is also in the other, and the other holds
    /// at least one more — so the other saw this one.
    Before,
    /// The reverse: this stamp saw the other.
    After,
    /// Neither saw the other. Each records a write the other does not.
    ///
    /// There is no correct winner between them, which is why the engine refuses
    /// and names both rather than picking one.
    Concurrent,
}

/// What a record's writer had seen when it wrote.
///
/// One count per node that has ever written this record, and no entry at all for
/// a node that has not. Entries are held in node order so that two stamps
/// compare in one pass without allocating, and so that a stamp encodes the same
/// bytes wherever it was built.
///
/// A stored count is never zero: it starts at one when [`Self::advance`] first
/// names a node. That is what lets an absent node be read as zero without the
/// two cases ever meaning different things — never having written and having
/// written no times are the same fact about causality.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CausalStamp {
    /// `(node, count)`, ordered by node.
    entries: Vec<([u8; NODE_ID_LEN], u64)>,
}

impl CausalStamp {
    /// A stamp for a record nothing has written yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that this node writes, having seen everything the stamp holds.
    ///
    /// The node's own count rises by one and every other count is carried
    /// unchanged. Carrying them is not bookkeeping: it is the claim that this
    /// writer had those writes in hand, and it is the only reason a later
    /// comparison can tell ignorance from sequence.
    pub fn advance(&mut self, node: [u8; NODE_ID_LEN]) {
        match self.entries.binary_search_by(|(held, _)| held.cmp(&node)) {
            Ok(at) => {
                if let Some((_, count)) = self.entries.get_mut(at) {
                    *count = count.saturating_add(1);
                }
            }
            Err(at) => self.entries.insert(at, (node, 1)),
        }
    }

    /// How many times this node has written the record, as this stamp knows it.
    ///
    /// Zero for a node the stamp does not name.
    #[must_use]
    pub fn count(&self, node: &[u8; NODE_ID_LEN]) -> u64 {
        self.entries
            .binary_search_by(|(held, _)| held.cmp(node))
            .ok()
            .and_then(|at| self.entries.get(at))
            .map_or(0, |(_, count)| *count)
    }

    /// How many nodes the stamp names.
    ///
    /// The number that must not grow without bound; keeping it small is what
    /// stops a contested record from accumulating one entry per writer forever.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the stamp names nobody.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entries, in node order.
    #[must_use]
    pub fn entries(&self) -> &[([u8; NODE_ID_LEN], u64)] {
        &self.entries
    }

    /// Rebuild a stamp from entries read back out of a stored value.
    ///
    /// The ordering is checked rather than restored. Sorting here would accept a
    /// stamp whose bytes are wrong and hand back one that is right, which hides
    /// a broken writer behind a forgiving reader; and a duplicate node has no
    /// correct repair at all, because the two counts disagree about what one
    /// node had seen and nothing in the bytes says which is current.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StampOutOfOrder`] when an entry does not follow its
    /// predecessor strictly.
    pub fn from_entries(entries: Vec<([u8; NODE_ID_LEN], u64)>) -> Result<Self> {
        for (at, pair) in entries.windows(2).enumerate() {
            let ordered = pair
                .first()
                .zip(pair.get(1))
                .is_some_and(|((left, _), (right, _))| left < right);
            if !ordered {
                return Err(Error::StampOutOfOrder {
                    at: at.saturating_add(1),
                });
            }
        }
        Ok(Self { entries })
    }

    /// Whether this stamp has seen everything the other has.
    ///
    /// Expressed through [`Self::compare`] rather than by walking the entries
    /// again: two routines answering one question is how they come to disagree,
    /// and a disagreement here would be a superseded version quietly surviving
    /// or a live one quietly dropped.
    #[must_use]
    pub fn descends(&self, other: &Self) -> bool {
        matches!(self.compare(other), CausalOrder::After | CausalOrder::Same)
    }

    /// How this stamp stands to another.
    ///
    /// One pass over both, since both are in node order. A node named by only
    /// one of them counts as zero on the other side, so its presence alone puts
    /// that side ahead.
    #[must_use]
    pub fn compare(&self, other: &Self) -> CausalOrder {
        let mut ours = self.entries.iter().peekable();
        let mut theirs = other.entries.iter().peekable();
        let mut we_are_ahead = false;
        let mut they_are_ahead = false;

        loop {
            match (ours.peek(), theirs.peek()) {
                (None, None) => break,
                (Some(_), None) => {
                    we_are_ahead = true;
                    ours.next();
                }
                (None, Some(_)) => {
                    they_are_ahead = true;
                    theirs.next();
                }
                (Some((our_node, our_count)), Some((their_node, their_count))) => {
                    match our_node.cmp(their_node) {
                        Ordering::Equal => {
                            match our_count.cmp(their_count) {
                                Ordering::Greater => we_are_ahead = true,
                                Ordering::Less => they_are_ahead = true,
                                Ordering::Equal => {}
                            }
                            ours.next();
                            theirs.next();
                        }
                        Ordering::Less => {
                            we_are_ahead = true;
                            ours.next();
                        }
                        Ordering::Greater => {
                            they_are_ahead = true;
                            theirs.next();
                        }
                    }
                }
            }
        }

        match (we_are_ahead, they_are_ahead) {
            (false, false) => CausalOrder::Same,
            (true, false) => CausalOrder::After,
            (false, true) => CausalOrder::Before,
            (true, true) => CausalOrder::Concurrent,
        }
    }
}

/// The versions of one record that nothing has superseded.
///
/// A record under multi-master does not have *a* version; it has whichever
/// versions no later write has seen. Most of the time that is one, and the set
/// exists for the times it is not.
///
/// # What keeps it bounded
///
/// A new write **drops every version it descends**, because a writer that had a
/// version in hand has superseded it. Three concurrent writes leave three; a
/// fourth write that saw all three leaves one. The set does not shrink on its
/// own and is not supposed to — what survives is a fact about what writers
/// actually saw, and discarding it on a size threshold would be dropping a
/// user's write to save a few bytes.
///
/// The *stamps* are bounded separately and by a different thing: one entry per
/// node that has written the record, however many times it writes. Those are two
/// structures and they are bounded by two arguments; asking one number to report
/// both is what the goal's first wording did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CausalVersions {
    /// Stamps no other stamp in the set descends.
    stamps: Vec<CausalStamp>,
}

impl CausalVersions {
    /// A record nothing has written yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Admit a write, superseding every version it had seen.
    ///
    /// A stamp the set already descends is dropped rather than added: it is a
    /// version this record has moved past, and re-admitting one is how a set
    /// that looks bounded grows anyway.
    pub fn record(&mut self, stamp: CausalStamp) {
        if self.stamps.iter().any(|held| held.descends(&stamp)) {
            return;
        }
        self.stamps.retain(|held| !stamp.descends(held));
        self.stamps.push(stamp);
    }

    /// How many versions survive.
    ///
    /// One whenever the record is settled. More than one means writers
    /// disagreed, which is the condition the engine refuses on rather than
    /// resolves.
    #[must_use]
    pub fn len(&self) -> usize {
        self.stamps.len()
    }

    /// Whether the record has no version at all.
    ///
    /// Present because a public `len` without it is a lint error, and the lint
    /// is right: a caller that can ask how many should not have to compare to
    /// zero to ask whether there are any.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stamps.is_empty()
    }

    /// Whether more than one version survives.
    #[must_use]
    pub fn is_contested(&self) -> bool {
        self.stamps.len() > 1
    }

    /// The surviving stamps themselves.
    ///
    /// A refusal has to NAME what it refused over — both versions and the node
    /// that wrote the other one (G027 S3.1) — and a count cannot be named. The
    /// order is the order they were recorded in, which is the order the store
    /// read them back, because nothing here has an opinion about which of two
    /// concurrent versions comes first and inventing one would be the ranking
    /// this type exists to refuse.
    #[must_use]
    pub fn stamps(&self) -> &[CausalStamp] {
        &self.stamps
    }
}

#[cfg(test)]
mod tests {
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
        assert!(ours.entries().windows(2).all(|pair| {
            pair.first().map(|(node, _)| *node) < pair.get(1).map(|(node, _)| *node)
        }));
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
}
