use super::*;

/// Something the store did on the way to an answer that the answer does not say.
///
/// A third channel, beside the records and the error. It exists because the two
/// it sits between cannot carry this: an error would refuse an answer that is
/// correct, and the records are correct, so silence is the only other option and
/// silence is what makes a fallback folklore. Every variant here is a case where
/// the store knows something the reader would otherwise have to guess at or
/// measure.
///
/// A note never changes the answer. A caller that ignores every note gets
/// exactly the records it would have got before notes existed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// The planner chose one path and the read took another.
    ///
    /// An ordered index that cannot fill the statement's bound leaves the answer
    /// needing records it does not hold, so the read scans instead. That is
    /// correct and it is linear, and the difference between the two is the whole
    /// reason anybody builds the index.
    FellBack {
        /// What the planner chose.
        from: AccessPath,
        /// What the read did instead.
        to: AccessPath,
    },
    /// The answer is the best the walk found, not provably the best there is.
    ///
    /// The one read in this store an index answers differently from a scan.
    /// Without this note the difference is invisible: an approximate answer and
    /// an exact one are the same shape, the same length, and usually the same
    /// records.
    Approximate,
    /// The read compared values of two different kinds.
    ///
    /// A schemaless store lets one record hold a number where the next holds the
    /// text of one, and `WHERE age = 30` then matches some of them. Nothing goes
    /// wrong — the comparison is well defined and the answer is right for the
    /// values that are there — and the read quietly answers a narrower question
    /// than the one that was asked.
    ///
    /// An absence never raises this. A record without the field is how a
    /// schemaless read narrows rather than fails, and a note on it would fire on
    /// nearly every read in the language.
    ComparedAcrossKinds {
        /// One kind, whichever sorts first, so the note reads the same way
        /// whichever side it was written on.
        left: &'static str,
        /// The other.
        right: &'static str,
    },
    /// A cursor was applied to the records rather than sought to.
    ///
    /// `AFTER` exists to make a deep page cost what a shallow one costs, and it
    /// does that by starting the read past the anchor's own key — but only a
    /// read answering in the store's own key order has a key to start past. Any
    /// other read has to reach the records first and then keep the ones after
    /// the anchor, which is the work an offset does, spelled better.
    ///
    /// The answer is the same either way. The cost is not, and without this note
    /// the difference is invisible: a page that sought and a page that walked are
    /// the same records in the same order.
    ///
    /// What a walked page gives is the cursor's **correctness** — a page that
    /// does not shift when a record is inserted behind it — and not its cost.
    /// Measured on this store, a sought page is flat at about 13 µs from the
    /// first record to the hundred-thousandth while the offset it replaces grows
    /// from 10 µs to 39 ms; a walked page is the cost of the read it sits on,
    /// which is what the same statement paying an offset would have cost too.
    CursorWalked,
    /// A materialised source produced as many records as its ceiling allows.
    ///
    /// Its answer is therefore a prefix of what the inner read would have
    /// answered unbounded, and the outer statement asked its question of that
    /// prefix. `LIMIT` is the caller's own word, so this is not a mistake — but
    /// a bound that was reached and a bound that was not are different answers
    /// and look identical.
    SubqueryCeiling {
        /// The ceiling, which is also how many records it held.
        rows: u64,
    },
    /// A held read is most of the way to the ceiling that will refuse it.
    ///
    /// A view naming no `LIMIT` runs under the ceiling every held read runs
    /// under, and past it the read is refused rather than shortened. That is the
    /// right failure and it arrives with no warning: a view sitting just below
    /// the line reads perfectly today and stops working on an ordinary week's
    /// growth, with nothing having said so.
    ///
    /// Unlike every other note here, this one reports a **state** rather than
    /// something that happened during the read — so it fires on every read while
    /// the condition holds. That is deliberate: the condition is persistent, and
    /// a warning that appeared once and then went quiet would be worse than
    /// none.
    NearingCeiling {
        /// How many records the read held.
        rows: u64,
        /// The ceiling it is approaching, past which the read is refused.
        most: u64,
    },
    /// Part of the answer was fetched from other nodes (G033, ADR-0083).
    ///
    /// This node holds some of a split table's shards, and the others were read
    /// from their leaders, each when it was asked. The answer is complete —
    /// a shard nobody answered for refuses the read — and it is not one
    /// snapshot, which nothing in its shape shows.
    Gathered {
        /// The table.
        table: String,
        /// The shards fetched, in key order.
        shards: Vec<u32>,
    },
    /// Messages of a topic passed their retention before this read reached
    /// them (G037).
    ///
    /// Positions are dense, so the count is exact: a reader told nothing would
    /// carry on from the first message still held and never learn that it had
    /// been given less than everything.
    Lapsed {
        /// The topic.
        topic: String,
        /// How many positions the read passed over.
        missed: u64,
    },
    /// The answer is a shortest path, and this is what it costs (G055 W6).
    ///
    /// The records are the path, start to end; the note says how many steps
    /// it took and what they cost — the sum of the weights a weighted path
    /// read, or the steps again.
    Path {
        /// Edges walked.
        steps: u64,
        /// Their total weight; the step count for an unweighted path.
        cost: Number,
    },
    /// Windows a `FILL` answered with although nothing was written in them
    /// (ADR-0088 §2). Their `count` is `0`, so each row can be told apart too.
    Filled {
        /// How many windows were filled.
        windows: u64,
    },
    /// A search index this read met holds terms this build's tokenizer may not
    /// make (G058 C3, Q-911): it recorded another generation, or none.
    ///
    /// A field's index is then not answered from — the read scans, which is
    /// exact — and a search's member, which no scan stands in for, is read and
    /// can miss records. Either way the fix is one statement, and the note names
    /// it, because nothing else in the answer would.
    NeedsRebuild {
        /// The field index, or the search the member belongs to.
        index: String,
        /// The table it is on.
        table: String,
        /// The generation it recorded, if any.
        built: Option<u32>,
        /// Whether it is a search's member rather than a field's index.
        member: bool,
    },
}

impl Note {
    /// A short stable name, for a client that groups or filters notes.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::FellBack { .. } => "fell-back",
            Self::Approximate => "approximate",
            Self::ComparedAcrossKinds { .. } => "compared-across-kinds",
            Self::CursorWalked => "cursor-walked",
            Self::SubqueryCeiling { .. } => "subquery-ceiling",
            Self::NearingCeiling { .. } => "nearing-ceiling",
            Self::Gathered { .. } => "gathered",
            Self::Lapsed { .. } => "lapsed",
            Self::Filled { .. } => "filled",
            Self::Path { .. } => "path",
            Self::NeedsRebuild { .. } => "needs-rebuild",
        }
    }

    /// The note in the words a reader would want it in.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::FellBack { from, to } => format!(
                "the {} path could not fill the bound, so the read took the {} path instead",
                from.name(),
                to.name(),
            ),
            Self::Approximate => GRAPH_WALK_IS_APPROXIMATE.to_owned(),
            Self::ComparedAcrossKinds { left, right } => format!(
                "this read compared {} {left} with {} {right}, \
                 so it answered about the records whose kinds happened to line up",
                article(left),
                article(right),
            ),
            Self::CursorWalked => "this page was reached by reading the records rather \
                 than seeking to the anchor, so it cost what the read costs and not \
                 what the page costs"
                .to_owned(),
            Self::SubqueryCeiling { rows } => format!(
                "the materialised source reached its ceiling of {rows}, \
                 so this answers about a prefix of what it would hold unbounded",
            ),
            Self::NearingCeiling { rows, most } => format!(
                "this held read holds {rows} records of the {most} it may hold, \
                 past which it is refused rather than shortened",
            ),
            Self::Lapsed { topic, missed } => format!(
                "{missed} message{} of `{topic}` passed {} retention before this read reached \
                 {}, and will not be given to anyone",
                if *missed == 1 { "" } else { "s" },
                "its",
                if *missed == 1 { "it" } else { "them" },
            ),
            Self::Path { steps, cost } => format!(
                "this is the shortest path within the bound: {steps} step{} costing {cost}",
                if *steps == 1 { "" } else { "s" },
            ),
            Self::Filled { windows } => format!(
                "{windows} window{} of this answer held no records and {} filled as the \
                 statement asked; {} count is 0",
                if *windows == 1 { "" } else { "s" },
                if *windows == 1 { "was" } else { "were" },
                if *windows == 1 { "its" } else { "their" },
            ),
            Self::NeedsRebuild {
                index,
                table,
                built,
                member,
            } => {
                let by = built.map_or_else(
                    || "an engine that recorded no tokenizer generation".to_owned(),
                    |generation| format!("tokenizer generation {generation}"),
                );
                let now = tessari_types::TOKENIZER_GENERATION;
                if *member {
                    format!(
                        "the search `{index}`'s member on `{table}` was built by {by}, not this \
                         build's generation {now}, so its terms may not be the ones this build \
                         makes and this answer can miss records; DROP SEARCH and DEFINE SEARCH \
                         again to rebuild it"
                    )
                } else {
                    format!(
                        "the search index `{index}` on `{table}` was built by {by}, not this \
                         build's generation {now}, so this read did not answer from its terms \
                         (a score is still measured against its statistics); \
                         REBUILD INDEX {index} ON {table} rebuilds it"
                    )
                }
            }
            Self::Gathered { table, shards } => format!(
                "shard{} {} of `{table}` {} read from {} leader{} on other nodes, each when \
                 it was asked, so this answer is complete and not one snapshot",
                if shards.len() == 1 { "" } else { "s" },
                shards
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
                if shards.len() == 1 { "was" } else { "were" },
                if shards.len() == 1 { "its" } else { "their" },
                if shards.len() == 1 { "" } else { "s" },
            ),
        }
    }
}
