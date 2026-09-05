//! How far apart two words are, asked with a budget.
//!
//! The only question a fuzzy search has for this module is *is this stored term
//! within `n` edits of what was typed* — never *how far apart are they*. That is
//! worth taking seriously rather than treating as a detail of the caller,
//! because the bounded question is much cheaper than the unbounded one and the
//! whole operator's cost is this function multiplied by the size of a dictionary
//! walk.

/// Whether `candidate` is within `budget` single-character edits of `typed`.
///
/// Edits are insertion, deletion and substitution — Levenshtein. A transposition
/// therefore costs two, not one: `hte` is two edits from `the`. Damerau's
/// variant charges one for that case and would be a defensible choice, but it is
/// not the one made here, and the difference is stated in `docs/tessariql.md`
/// rather than left for a reader to discover from a miss.
///
/// # Why this is not a distance function with a comparison afterwards
///
/// Computing the full distance and then testing it against a budget does work
/// the answer never needs. Two bounds cut that away, and both matter at the
/// scale this runs at — once per term in a dictionary walk, per word, per query:
///
/// **The length gate.** Two words whose lengths differ by more than the budget
/// cannot be within it, because every edit changes the length by at most one.
/// That is a subtraction, and it rejects most of a dictionary before any matrix
/// exists.
///
/// **The band.** Only cells within `budget` of the diagonal can hold a value at
/// or below the budget, so each row is computed across a window of `2·budget+1`
/// cells rather than the whole width. With a budget of two that is five cells
/// per row whatever the words' length, which turns the cost from quadratic into
/// linear.
///
/// A third bound falls out of the second: if every cell in a completed row
/// exceeds the budget, no later row can come back under it, since a row's values
/// never decrease by more than one per step down. The walk stops there.
///
/// # Characters, not bytes
///
/// The comparison is over `char`s. Doing it over UTF-8 bytes would make one
/// mistyped non-ASCII letter cost two or three edits instead of one, so a budget
/// of two would behave differently for a reader typing `naïve` than for one
/// typing `naive` — the operator would be quietly stricter in some languages
/// than others. The analyzer's ASCII folding removes many such cases before this
/// is reached, but not all of them, and this function does not get to assume it
/// ran.
#[must_use]
pub fn within(typed: &str, candidate: &str, budget: usize) -> bool {
    let left: Vec<char> = typed.chars().collect();
    let right: Vec<char> = candidate.chars().collect();

    // Every edit changes the length by at most one, so a length gap wider than
    // the budget is decided without looking at a single character.
    let (shorter, longer) = if left.len() <= right.len() {
        (left.len(), right.len())
    } else {
        (right.len(), left.len())
    };
    if longer.saturating_sub(shorter) > budget {
        return false;
    }

    // Every value above the budget is the same answer — "too far" — so one
    // sentinel stands for all of them. That is what makes the band safe: a cell
    // the window skipped is not stale data to be stepped around, it is a cell
    // holding `over`, and the next row reads it as the rejection it is.
    let over = budget.saturating_add(1);

    // One row of the matrix, held as the previous row while the next is built.
    // The full matrix is never materialised: nothing here asks for the path, and
    // the answer needs only the last cell.
    let mut previous: Vec<usize> = (0..=right.len()).map(|column| column.min(over)).collect();
    let mut current: Vec<usize> = vec![over; right.len().saturating_add(1)];

    for (row, from) in left.iter().enumerate() {
        let index = row.saturating_add(1);
        current.fill(over);
        // Column zero is a real cell, not padding: it is the cost of deleting
        // every character read so far. It is in band only while that cost is
        // itself within the budget, and `min` says so without a branch.
        current[0] = index.min(over);

        // The band: outside `budget` of the diagonal every cell is already above
        // the budget, so the row is computed across a window rather than across
        // the whole word. When the window is empty — which happens when the
        // right-hand word has run out — the range is simply empty and the row is
        // decided by column zero alone.
        let first = index.saturating_sub(budget).max(1);
        let last = index.saturating_add(budget).min(right.len());

        let mut best = current[0];
        for column in first..=last {
            let previous_column = column.saturating_sub(1);
            let to = right[previous_column];
            let substitution = previous[previous_column].saturating_add(usize::from(*from != to));
            let deletion = previous[column].saturating_add(1);
            let insertion = current[previous_column].saturating_add(1);
            let cell = substitution.min(deletion).min(insertion).min(over);
            current[column] = cell;
            best = best.min(cell);
        }

        // Every cell in this row is already over budget, and a row's values fall
        // by at most one per step down, so no later row returns under it.
        if best > budget {
            return false;
        }

        std::mem::swap(&mut previous, &mut current);
    }

    previous[right.len()] <= budget
}

#[cfg(test)]
mod tests {
    use super::within;

    /// The cases the operator exists for: one letter dropped, one added, one
    /// wrong. Each is one edit, and each is what a reader actually types.
    #[test]
    fn one_edit_is_one_edit() {
        assert!(within("vectr", "vector", 1), "a dropped letter");
        assert!(within("vectorr", "vector", 1), "a doubled letter");
        assert!(within("vectir", "vector", 1), "a wrong letter");
    }

    /// A transposition costs **two** under Levenshtein. Asserted rather than
    /// left implicit, because it is the one place a reader's intuition and this
    /// function disagree, and `docs/tessariql.md` states it for that reason.
    #[test]
    fn a_transposition_costs_two() {
        assert!(!within("vetcor", "vector", 1));
        assert!(within("vetcor", "vector", 2));
    }

    /// The budget is a ceiling that actually holds — the interesting half is the
    /// pair just outside it, which a function that computed a distance and then
    /// compared would also get right, and which a mis-set band would not.
    #[test]
    fn the_budget_is_a_ceiling() {
        assert!(within("cat", "cot", 1));
        assert!(!within("cat", "dog", 2));
        assert!(within("cat", "dog", 3));
    }

    /// The length gate must not reject a pair it should accept, and must reject
    /// the one it should. `abc` to `abcdef` is three insertions.
    #[test]
    fn the_length_gate_agrees_with_the_matrix() {
        assert!(!within("abc", "abcdef", 2));
        assert!(within("abc", "abcdef", 3));
        assert!(within("abc", "abcde", 2));
    }

    /// Identity and emptiness, which is where an off-by-one in the band shows up
    /// as a word not matching itself.
    #[test]
    fn a_word_is_within_zero_edits_of_itself() {
        assert!(within("vector", "vector", 0));
        assert!(!within("vector", "vectors", 0));
        assert!(within("", "", 0));
        assert!(within("", "ab", 2));
        assert!(!within("", "abc", 2));
    }

    /// Edits are counted in characters. Over bytes, `naïve` and `naive` would be
    /// two edits apart rather than one, and the operator would be stricter for
    /// some languages than for others.
    #[test]
    fn edits_are_characters_and_not_bytes() {
        assert!(within("naïve", "naive", 1));
        assert!(within("café", "cafe", 1));
    }

    /// The band is the only part of this function that could return a *wrong*
    /// answer rather than a slow one, so it is checked against the unbounded
    /// definition over a spread of shapes rather than at a few hand-picked
    /// points.
    #[test]
    fn the_band_agrees_with_a_full_matrix() {
        fn full(left: &str, right: &str) -> usize {
            let left: Vec<char> = left.chars().collect();
            let right: Vec<char> = right.chars().collect();
            let mut row: Vec<usize> = (0..=right.len()).collect();
            for (index, from) in left.iter().enumerate() {
                let mut previous = row[0];
                row[0] = index.saturating_add(1);
                for (column, to) in right.iter().enumerate() {
                    let substitution = previous.saturating_add(usize::from(from != to));
                    previous = row[column.saturating_add(1)];
                    row[column.saturating_add(1)] = substitution
                        .min(row[column].saturating_add(1))
                        .min(previous.saturating_add(1));
                }
            }
            row[right.len()]
        }

        let words = [
            "",
            "a",
            "ab",
            "abc",
            "vector",
            "vectr",
            "vetcor",
            "container",
            "containr",
            "analyzer",
            "analyzr",
            "aaaa",
            "abab",
            "banana",
            "bananas",
            "ananab",
        ];
        for left in words {
            for right in words {
                for budget in 0..=3 {
                    assert_eq!(
                        within(left, right, budget),
                        full(left, right) <= budget,
                        "{left:?} vs {right:?} at budget {budget}"
                    );
                }
            }
        }
    }
}
