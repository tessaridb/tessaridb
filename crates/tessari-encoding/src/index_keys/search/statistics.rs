use super::*;

/// What one search index knows about one term.
///
/// # One number, and where the second one goes
///
/// `documents` is the term's document frequency — how many records hold it. It
/// is maintained rather than counted for the reason the collection statistics
/// are: counting it means walking the term's whole posting range, and a ranked
/// read does that once per query term, per query.
///
/// The other two numbers are the **postings' own extremes**, and they are the
/// upper bound safe top-k pruning prunes against.
///
/// # Why extremes rather than the impact the format specification named
///
/// The specification placed a single `max_impact` here: the largest contribution
/// any one posting can make to a score. A contribution is
/// `idf × saturation(occurrences, length, average_length)`, and
/// `average_length` belongs to the **collection**, which moves on every write.
/// So a stored impact is a number about a collection that no longer exists — and
/// the direction is the fatal part. When the average grows, the same posting
/// scores *higher*, so a stored impact becomes an **under**estimate. An
/// underestimated upper bound is not loose, it is unsound: the term is pruned and
/// the records it would have won are silently missing from the answer.
///
/// These two are properties of the postings alone and are sound at every
/// collection state, because saturation is increasing in occurrences and
/// decreasing in length. So for any posting `p` of this term and any average,
/// `saturation(f_p, dl_p) ≤ saturation(max_frequency, min_length)`. The pairing
/// takes the frequency from one record and the length from another, which is why
/// the bound is looser than a true maximum — and loose in the safe direction.
/// **ADR-0050.**
///
/// # The maintenance rule
///
/// `max_frequency` never falls and `min_length` never rises, for the reason the
/// specification already gave: an extreme cannot move inward without knowing the
/// second one, so neither is relaxed when a posting leaves. Both drift loose over
/// time, which costs pruning efficiency and never correctness, and a rebuild
/// recomputes them from the postings it writes. Both are integers, so they are
/// exact, they accumulate no float error across a release, and they compare with
/// `>` rather than against a tolerance.
///
/// # Zero means no bound, which means do not prune
///
/// A real posting has at least one occurrence and its record at least one token,
/// so neither number is ever legitimately zero. An entry written before these
/// existed decodes as `0, 0`, and that pair means **this term has no usable
/// bound** — a reader must decline to prune it rather than treat the bound as
/// zero, which would prune everything. [`Self::bound`] is the only way to ask,
/// and it answers `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TermStatistics {
    /// How many records hold this term.
    pub documents: u64,
    /// The most occurrences any one posting of this term records.
    ///
    /// `0` when no bound has been recorded — see the type documentation.
    pub max_frequency: u32,
    /// The fewest tokens held by any record posting this term.
    ///
    /// `0` when no bound has been recorded — see the type documentation.
    pub min_length: u32,
}

impl TermStatistics {
    /// State the frequency, with no bound recorded.
    #[must_use]
    pub const fn new(documents: u64) -> Self {
        Self {
            documents,
            max_frequency: 0,
            min_length: 0,
        }
    }

    /// State the frequency and the postings' extremes.
    #[must_use]
    pub const fn bounded(documents: u64, max_frequency: u32, min_length: u32) -> Self {
        Self {
            documents,
            max_frequency,
            min_length,
        }
    }

    /// The extremes to score an upper bound from, when there are any.
    ///
    /// `None` is not "no records" — it is **this build cannot bound this term**,
    /// which obliges a caller to score the term's postings rather than prune
    /// them. Returning a zero pair instead would be an upper bound of zero, and
    /// a term that can contribute nothing is precisely a term to prune away.
    /// Every entry written before the bound existed is in this state, so the
    /// distinction is what lets an older index be read at all.
    #[must_use]
    pub const fn bound(&self) -> Option<(u32, u32)> {
        if self.max_frequency == 0 || self.min_length == 0 {
            return None;
        }
        Some((self.max_frequency, self.min_length))
    }
}

impl StoreValue for TermStatistics {
    fn encode(&self) -> Value {
        let mut writer = KeyWriter::new();
        writer.put_u64(self.documents);
        // Written unconditionally, including as the zero pair. Omitting them
        // when unset would make the payload's length mean two things — an entry
        // from an older build, and a current one with nothing to bound — and a
        // reader cannot tell those apart from the bytes. They are the same
        // *answer* (do not prune), but making one shape carry both meanings is
        // how the next field to arrive here becomes ambiguous.
        writer.put_u32(self.max_frequency);
        writer.put_u32(self.min_length);
        let body = writer.finish();
        let mut buffer = with_header(0, body.len());
        buffer.extend_from_slice(&body);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        let mut reader = KeyReader::new(KeyKind::SearchTerm, payload);
        let documents = reader.take_u64()?;
        // The payload dispatches on its own length, which is what the earlier
        // comment here anticipated: an entry written before the bound existed
        // ends after the count, and reads back as the zero pair — no bound, so
        // do not prune. Trailing bytes beyond the pair are still refused, so the
        // NEXT field to arrive is a payload this build declines rather than one
        // it misreads as this shape.
        let (max_frequency, min_length) = if reader.remaining() > 0 {
            (reader.take_u32()?, reader.take_u32()?)
        } else {
            (0, 0)
        };
        reader.finish()?;
        Ok(Self {
            documents,
            max_frequency,
            min_length,
        })
    }
}
