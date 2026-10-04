/// How quickly repeating a term stops improving a BM25 score.
///
/// Unit: dimensionless.
///
/// A relevance score that counted occurrences linearly could be lifted without
/// limit by repeating one word, which is both wrong about language and an open
/// invitation to anyone writing the documents. `k1` is the point at which each
/// further occurrence buys noticeably less than the last: the term's
/// contribution approaches `k1 + 1` times its weight and never exceeds it.
///
/// `1.2` is the value the literature settled on across TREC collections, and it
/// is the value nearly every production engine ships. It is a starting value
/// here for the same reason it is elsewhere — nothing in this project has been
/// measured against a labelled relevance set, and a number chosen without one
/// would be a guess dressed as a decision.
pub const BM25_K1: f64 = 1.2;

/// How much a document's length is held against it in a BM25 score.
///
/// Unit: dimensionless, in `0.0..=1.0`.
///
/// At `0.0` length is ignored entirely, so a long document holding a term once
/// ranks with a short one that does — which favours whichever document happens
/// to be longest. At `1.0` length is fully normalised away, which over-punishes
/// the long document that is genuinely the better answer.
///
/// `0.75` is the standard compromise and the same starting value `BM25_K1` is.
/// Both become options on the index definition when there is a measurement to
/// justify a different value; until then, changing either in a release changes
/// ranking order without any statement changing, which is stated in
/// `docs/tessariql.md` rather than left to be discovered.
pub const BM25_B: f64 = 0.75;

/// The constant a fused read adds to every rank before dividing a branch's
/// weight by it (`ORDER BY FUSE`, G038).
///
/// Unit: ranks.
///
/// It decides how much the first few places of one branch outweigh agreement
/// lower down: at `0` a first place is worth twice a second and swamps
/// everything else, and a large value flattens the ranks toward a vote count.
/// `60` is the value reciprocal rank fusion was published with (Cormack, Clarke
/// and Büttcher, 2009) and the one production engines ship. A constant, like
/// `BM25_K1`, because nothing here has been measured against a relevance set —
/// and changing it changes fused order without any statement changing.
pub const FUSION_K: u64 = 60;

/// How far down each branch's own order a record may be and still count in a
/// fused read that names no `DEPTH`.
///
/// Unit: records per branch.
pub const FUSION_DEPTH: u64 = 100;

/// How far past its bound an ordered read under a condition may walk the index
/// before giving the order up and taking the scan.
///
/// Unit: multiples of the bound the statement asked for.
///
/// An index-served order under a `WHERE` walks in the sort's order and re-tests
/// each record against the whole condition, because the index narrows and the
/// condition decides. So filling a bound of ten may take more than ten entries,
/// and how many more depends on how selective the condition is over the order —
/// which is exactly the distribution statistic this store deliberately does not
/// keep (`docs/tessariql.md` §8).
///
/// The ceiling is what turns that unknown into a cost rather than a risk. Past
/// it the condition is not selective enough for the order to be worth serving
/// from the index, and the read falls back to the scan it would have taken
/// anyway. **It bounds the cost and never the answer**: every exit is either an
/// ordered answer that filled the bound or the scan.
///
/// `32` because the retries double, so reaching the ceiling costs about twice
/// the ceiling in entries — a few hundred for a bound of ten, against a scan of
/// the whole table. A condition matching one row in thirty-two is still served;
/// one matching one in a thousand is not, and paying a full scan for it is the
/// right answer rather than a walk that reads most of the index in batches.
pub const ORDERED_FILTER_REACH: usize = 32;

/// The shortest prefix `MATCHES PREFIX` will accept.
///
/// Unit: characters, after the field's non-stemming filters have been applied.
///
/// **Contract, not tuning.** A prefix below this is refused by name and the
/// refusal states the limit; it is not merely served slowly. The reason is that
/// the cost of a prefix is the size of its expansion, and the expansion of a
/// short prefix is a large fraction of the whole vocabulary — `a` reaches every
/// word beginning with `a`, which on English prose is roughly one word in
/// fourteen. A reader gains nothing from that answer and the store pays for all
/// of it, on the query a frustrated reader retries.
///
/// Three rather than two, because two is where the fraction stops being small:
/// `th` alone reaches a tenth of an English vocabulary. Three is also the
/// conventional floor in the engines that offer this, which matters less than
/// the reason but is worth not contradicting without one.
pub const SEARCH_PREFIX_MINIMUM: usize = 3;

/// How many distinct terms one prefix may expand to before the index declines
/// to serve it.
///
/// Unit: terms.
///
/// **Not a refusal.** A prefix expanding past this is answered by the scan
/// instead, and `EXPLAIN` reports `scan` — the answer is identical either way,
/// which is the rule this store holds everywhere: *which access path runs is
/// decided by what exists; the answer is not.* A cap that refused would make a
/// query succeed without an index and fail once somebody added one.
///
/// What the cap protects is the **index** path, where an expansion is a union of
/// that many posting lists. Sixty-four is enough for every prefix a person
/// actually types at three characters or more and small enough that the union
/// stays cheaper than the scan it replaces.
pub const SEARCH_PREFIX_EXPANSION_CAP: usize = 64;

/// How many dictionary terms a scored prefix reads before it ranks them by
/// document frequency and keeps the [`SEARCH_PREFIX_EXPANSION_CAP`] most held
/// (ADR-0104).
///
/// Unit: terms examined.
///
/// The ranking is the point — a cut taken in dictionary order keeps the rare
/// words that happen to sort first and drops the common one the reader was
/// typing — and ranking needs the candidates in hand. A three-letter beginning
/// reaching more than this many distinct words is past what anybody types, and
/// the ranking is then over the first this-many in dictionary order: the score
/// is still an exact function of the terms it names. Each one examined costs one count read, the same trade
/// [`SEARCH_FUZZY_EXAMINATION_CAP`] makes.
pub const SEARCH_PREFIX_SCORE_EXAMINATION_CAP: usize = 1024;

/// The most edits `MATCHES FUZZY` will look through.
///
/// Unit: single-character insertions, deletions and substitutions — Levenshtein,
/// not Damerau: a transposition costs two.
///
/// **Contract, not tuning.** A query asking for more is refused by name and the
/// refusal states the limit. Two is the number because the candidate set grows
/// with the alphabet raised to the edit count: at one edit a word of length `n`
/// has about `53n` neighbours over lowercase letters and digits, at two about
/// `1400n`, and at three the neighbourhood is larger than most vocabularies —
/// at which point every query matches something and the operator has stopped
/// discriminating rather than started being generous.
///
/// It is also the point where a match stops being a plausible reading of what
/// somebody meant. `cat` and `dog` are three edits apart.
pub const SEARCH_FUZZY_MAX_EDITS: usize = 2;

/// How many leading characters of a fuzzy query are **not** fuzzy.
///
/// Unit: characters, after the field's non-stemming filters have been applied —
/// the same footing as [`SEARCH_PREFIX_MINIMUM`].
///
/// **Semantics, not an index-side bound**, and this is the distinction that
/// matters. A stored term satisfies a typed word only if it is within
/// [`SEARCH_FUZZY_MAX_EDITS`] edits **and** shares this many first characters,
/// and the scan applies exactly that rule. Implementing the prefix only as a
/// dictionary-walk bound would be cheaper to write and would break the rule this
/// store holds everywhere: *which access path runs is decided by what exists;
/// the answer is not.* The same statement would return one set on a table with
/// no index and a smaller set once somebody added one (ADR-0046).
///
/// The cost is real and belongs in the documentation rather than in a surprise:
/// a mistake inside the first `SEARCH_FUZZY_PREFIX` characters is not found.
/// `xector` does not reach `vector`. That is accepted because the alternative is
/// a walk of the whole term dictionary per word, which is the denial of service
/// this operator would otherwise be — and because a first letter is the part of
/// a word people mistype least, having usually just read it.
///
/// Two, not the three it was until G055 (Q-867): with three, a swap of the
/// second and third letters — `anlayzer` — was never found, and two letters
/// still start the walk deep enough inside the dictionary that the examination
/// ceiling bounds it.
pub const SEARCH_FUZZY_PREFIX: usize = 2;

/// How many distinct terms one fuzzy word may match before the index declines to
/// serve it.
///
/// Unit: terms **matched**, not terms examined — the two differ here, which they
/// do not for a prefix walk, and the difference is what G022's S3 measures.
///
/// **Not a refusal**, for the same reason [`SEARCH_PREFIX_EXPANSION_CAP`] is not
/// one: only the index can evaluate it, so a cap that refused would make a
/// statement succeed without an index and fail once somebody added one. Past it
/// the candidate is not offered and the scan answers, which is the identical
/// answer by a different path.
///
/// Sixteen rather than the prefix cap's sixty-four. A prefix expansion is a set
/// a reader chose the size of by typing fewer letters; a fuzzy expansion is a
/// set the *store* chose, and a word with sixteen near-spellings in the corpus
/// is one where the union has stopped being cheaper than reading the records.
pub const SEARCH_FUZZY_EXPANSION_CAP: usize = 16;

/// How many terms one fuzzy word's dictionary walk may **read** before the index
/// declines to serve it.
///
/// Unit: terms examined — the other side of [`SEARCH_FUZZY_EXPANSION_CAP`], and
/// the reason the two exist separately.
///
/// This ceiling is what the design did not have and the implementation needed.
/// Intersecting an edit-distance automaton with a dictionary by **walking a
/// range** reads every term sharing the mandatory prefix and keeps the few
/// inside the budget, so the work is bounded by the popularity of a three-letter
/// beginning rather than by the number of matches. Without a ceiling, a common
/// beginning in a large vocabulary is a denial of service costing the caller one
/// request — which is precisely the failure G022's S3 exists to catch.
///
/// **Not a refusal**, on the same reasoning as the two caps above: past it the
/// candidate is not offered and the scan answers with the identical result.
///
/// A thousand and twenty-four, because a term read is a key decode and a bounded
/// character comparison while the alternative it defers to is a record decode —
/// perhaps two orders of magnitude more work. Examining a thousand terms to
/// avoid reading tens of records is the trade this number makes, and it stops
/// being a good one somewhere past here.
pub const SEARCH_FUZZY_EXAMINATION_CAP: usize = 1024;

/// How many tokens a `search::snippet()` window spans (ADR-0105).
///
/// Unit: tokens of the field's analysed text. The window is chosen by how many
/// **distinct** query words it holds, then by how many matches, then by being
/// earliest — so a passage covering the whole query beats one repeating a
/// single word. Long enough to read a sentence around a match, short enough to
/// sit in a result list.
pub const SEARCH_SNIPPET_TOKENS: usize = 24;
