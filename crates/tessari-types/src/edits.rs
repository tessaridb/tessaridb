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
/// Edits are insertion, deletion, substitution and the **transposition of two
/// adjacent characters** — optimal string alignment, the restricted form of
/// Damerau's distance. `hte` is one edit from `the`, because swapping two
/// letters is the commonest slip of a typing hand and it is one mistake, not
/// two. Restricted means a transposed pair is not edited again: `ca` is three
/// edits from `abc`, where the unrestricted distance would say two. That is
/// the form a dynamic programme over one previous row pair computes, and the
/// only cases it differs on are ones nobody types. Until G051 a transposition
/// cost two (Levenshtein); `docs/tessariql.md` states the change.
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
/// exceeds the budget, no later row can come back under it, since no cell is
/// smaller than the smallest cell of the row above it — a transposition included,
/// which costs one more than a cell two rows up that the row above already
/// bounds. The walk stops there.
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

    // Two rows of the matrix behind the one being built: the row above, and the
    // one above that, which is where a transposition reaches back to. The full
    // matrix is never materialised: nothing here asks for the path, and the
    // answer needs only the last cell.
    let mut before: Vec<usize> = vec![over; right.len().saturating_add(1)];
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
        // decided by column zero alone. A transposition keeps both lengths, so
        // it does not widen the band.
        let first = index.saturating_sub(budget).max(1);
        let last = index.saturating_add(budget).min(right.len());

        let mut best = current[0];
        for column in first..=last {
            let previous_column = column.saturating_sub(1);
            let to = right[previous_column];
            let substitution = previous[previous_column].saturating_add(usize::from(*from != to));
            let deletion = previous[column].saturating_add(1);
            let insertion = current[previous_column].saturating_add(1);
            let mut cell = substitution.min(deletion).min(insertion);
            // `…ab` against `…ba`: the pair costs one edit from the cell two rows
            // and two columns back. A cell outside last row's band reads as
            // `over`, which is the rejection it stands for.
            if row > 0
                && column > 1
                && left[row.saturating_sub(1)] == to
                && *from == right[column.saturating_sub(2)]
            {
                cell = cell.min(before[column.saturating_sub(2)].saturating_add(1));
            }
            let cell = cell.min(over);
            current[column] = cell;
            best = best.min(cell);
        }

        // Every cell in this row is already over budget, and no later row can
        // come back under it: a step down costs at least what the row above's
        // smallest cell did, and a transposition costs one more than a cell two
        // rows up, which the row above already bounds from below.
        if best > budget {
            return false;
        }

        std::mem::swap(&mut before, &mut previous);
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

    /// A transposition of two adjacent letters costs **one** — the commonest
    /// slip of a typing hand, charged as the one mistake it is (optimal string
    /// alignment). Two transpositions are two edits, and a letter cannot be
    /// transposed and then edited again: `ca` → `abc` is three, not two.
    #[test]
    fn a_transposition_costs_one() {
        assert!(within("vetcor", "vector", 1));
        assert!(!within("vetcor", "vector", 0));
        assert!(
            within("trasnactoin", "transaction", 2),
            "two transpositions"
        );
        assert!(!within("trasnactoin", "transaction", 1));
        assert!(
            !within("ca", "abc", 2),
            "optimal string alignment, not unrestricted Damerau"
        );
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
            let width = right.len().saturating_add(1);
            let at = |row: usize, column: usize| row.saturating_mul(width).saturating_add(column);
            let mut cells = vec![0_usize; left.len().saturating_add(1).saturating_mul(width)];
            for row in 0..=left.len() {
                for column in 0..=right.len() {
                    let cell = if row == 0 || column == 0 {
                        row.saturating_add(column)
                    } else {
                        let (up, back) = (row.saturating_sub(1), column.saturating_sub(1));
                        let cost = usize::from(left[up] != right[back]);
                        let mut best = cells[at(up, back)]
                            .saturating_add(cost)
                            .min(cells[at(up, column)].saturating_add(1))
                            .min(cells[at(row, back)].saturating_add(1));
                        if row > 1
                            && column > 1
                            && left[up] == right[column.saturating_sub(2)]
                            && left[row.saturating_sub(2)] == right[back]
                        {
                            let swapped =
                                cells[at(row.saturating_sub(2), column.saturating_sub(2))];
                            best = best.min(swapped.saturating_add(1));
                        }
                        best
                    };
                    cells[at(row, column)] = cell;
                }
            }
            cells[at(left.len(), right.len())]
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
            "trasnactoin",
            "transaction",
            "ca",
            "abc",
            "acb",
            "bca",
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
