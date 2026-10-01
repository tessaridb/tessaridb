//! A word as the language stemmers read it: letters, and the regions their
//! rules are confined to.

/// A word's letters, each one `char`, so a suffix is counted in letters
/// whatever script it is written in.
#[derive(Debug, Clone)]
pub(super) struct Word {
    letters: Vec<char>,
}

impl Word {
    pub(super) fn of(text: &str) -> Self {
        Self {
            letters: text.chars().collect(),
        }
    }

    pub(super) const fn len(&self) -> usize {
        self.letters.len()
    }

    pub(super) fn letters(&self) -> &[char] {
        &self.letters
    }

    /// The letter at `at`, when there is one.
    pub(super) fn at(&self, at: usize) -> Option<char> {
        self.letters.get(at).copied()
    }

    pub(super) fn ends_with(&self, suffix: &str) -> bool {
        let wanted = suffix.chars().count();
        self.len() >= wanted
            && self
                .letters
                .iter()
                .rev()
                .zip(suffix.chars().rev())
                .all(|(held, asked)| *held == asked)
    }

    /// Where `suffix` begins when the word ends with it.
    pub(super) fn start_of(&self, suffix: &str) -> usize {
        self.len().saturating_sub(suffix.chars().count())
    }

    /// The longest of `suffixes` the word ends with — the choice a rule makes
    /// before its condition is asked, so a condition that fails does not fall
    /// back to a shorter suffix.
    pub(super) fn longest<'a>(&self, suffixes: &[&'a str]) -> Option<&'a str> {
        suffixes
            .iter()
            .copied()
            .filter(|suffix| self.ends_with(suffix))
            .max_by_key(|suffix| suffix.chars().count())
    }

    /// The longest of `suffixes` the word ends with that begins at or after
    /// `from` — for an algorithm whose search never looks before a region, so
    /// a suffix reaching past it is not a candidate at all.
    pub(super) fn longest_from<'a>(&self, suffixes: &[&'a str], from: usize) -> Option<&'a str> {
        suffixes
            .iter()
            .copied()
            .filter(|suffix| self.ends_with(suffix) && self.start_of(suffix) >= from)
            .max_by_key(|suffix| suffix.chars().count())
    }

    /// Remove the last `count` letters.
    pub(super) fn cut(&mut self, count: usize) {
        let keep = self.len().saturating_sub(count);
        self.letters.truncate(keep);
    }

    /// Replace the last `count` letters with `with`.
    pub(super) fn replace(&mut self, count: usize, with: &str) {
        self.cut(count);
        self.letters.extend(with.chars());
    }

    /// Whether the letters before `at` end with `text`.
    pub(super) fn before_is(&self, at: usize, text: &str) -> bool {
        let Some(head) = self.letters.get(..at) else {
            return false;
        };
        let wanted = text.chars().count();
        head.len() >= wanted
            && head
                .iter()
                .rev()
                .zip(text.chars().rev())
                .all(|(held, asked)| *held == asked)
    }

    /// The standard regions: R1 after the first non-vowel following a vowel,
    /// R2 the same again from R1; the word's end where there is none.
    pub(super) fn regions(&self, vowel: impl Fn(char) -> bool) -> (usize, usize) {
        let after = |from: usize| -> usize {
            let mut at = from;
            while at < self.len() && !self.at(at).is_some_and(&vowel) {
                at = at.saturating_add(1);
            }
            while at < self.len() && self.at(at).is_some_and(&vowel) {
                at = at.saturating_add(1);
            }
            at.saturating_add(1).min(self.len())
        };
        let r1 = after(0);
        (r1, after(r1))
    }

    pub(super) fn text(&self) -> String {
        self.letters.iter().collect()
    }
}
