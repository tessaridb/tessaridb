//! Where a statement ends in text typed or piped a line at a time.

/// Whether the accumulated text closes a statement.
///
/// A `;` inside a string does not, which is the whole reason this is a walk
/// rather than a `contains`.
///
/// The session itself no longer asks this — it feeds a [`Scanner`] line by line
/// instead — but the session's tests assert the single-pass answer, which is the
/// behaviour the incremental scan has to reproduce.
#[cfg(test)]
pub(crate) fn closed(text: &str) -> bool {
    scan(text).closed
}

/// What one pass over partial input found.
pub(crate) struct Scan {
    /// A statement ended in this text.
    pub(crate) closed: bool,
    /// There is something here besides whitespace and comments.
    ///
    /// The difference matters only at end of input, where text that never
    /// closed is reported as a statement that ran out. A file whose last line
    /// is a note has nothing unfinished in it.
    pub(crate) substantial: bool,
    /// A `BEGIN` in this text has no `COMMIT` or `CANCEL` after it.
    pub(crate) open_transaction: bool,
}

/// The three words that move a transaction boundary, recognised only where a
/// statement begins — so a field called `begin` is a field.
pub(crate) fn boundary(word: &str, open: &mut bool) {
    if word.eq_ignore_ascii_case("BEGIN") {
        *open = true;
    } else if word.eq_ignore_ascii_case("COMMIT") || word.eq_ignore_ascii_case("CANCEL") {
        *open = false;
    }
}

/// Reads far enough to find a statement's end, honouring quotes and comments.
///
/// Comments run from `--` to the end of their line, which the lexer already
/// knows and this did not. Both halves of that omission were wrong, and the
/// second one silently:
///
/// - a script ending in a comment left text that never closed, and was reported
///   as input that ran out mid-statement — sending the reader to look for an
///   unbalanced quote that is not there;
/// - a `;` **inside** a comment ended the statement early. `SELECT * FROM users
///   -- oops; a note` then `WHERE name = 'ada';` split into a read with no
///   filter and a fragment beginning `WHERE`. The first ran and printed every
///   record. Nothing reported a fault about the answer, because as far as
///   everything below here was concerned there was no fault: the wrong question
///   was asked correctly.
///
/// Kept as a single-pass entry point for the tests below: they state the
/// behaviour in terms of a whole text, and the incremental scan the session
/// runs has to agree with it however the text is cut up.
#[cfg(test)]
pub(crate) fn scan(text: &str) -> Scan {
    let mut scanner = Scanner::default();
    scanner.feed(text);
    scanner.state()
}

/// A scan that can be handed the next piece of input instead of the whole
/// buffer again.
///
/// # Why this is not one function over the accumulated text
///
/// It was, and outside a transaction that is free: the walk stops at the first
/// `;`, so the text it examines is one statement however long the session runs.
/// Inside a transaction the walk deliberately does **not** stop at `;` — a `;`
/// there ends a statement and not the group that has to be submitted together —
/// so the accumulated block is what gets examined, and examining it again after
/// every appended line is quadratic in the number of statements.
///
/// Measured before this existed, loading `CREATE`s through the console: 76 µs
/// per statement at 500 of them inside one transaction, 589 µs at 8 000, the
/// cost doubling each time the count did. The same statements through the node's
/// HTTP surface, which does not go through here, cost 15-23 µs and got *cheaper*
/// with batching — so this was the console's own cost and not the store's.
///
/// The state below is exactly the set of local variables the walk used to keep
/// between characters. Keeping them across calls is the whole change.
#[derive(Default)]
pub(crate) struct Scanner {
    quote: Option<char>,
    escaped: bool,
    commented: bool,
    substantial: bool,
    open_transaction: bool,
    closed: bool,
    /// Something besides whitespace and comments has been read since the last
    /// `;` that closed a statement.
    ///
    /// What makes `closed` true is a `;`, but what makes the text safe to submit
    /// is that nothing was begun after it: `A; SELECT x` closed `A` and opened a
    /// read whose `FROM` is on the next line, and submitting there sent half a
    /// statement (Q-872).
    tail: bool,
    /// A `-` that has been read while the character after it has not.
    ///
    /// Only a doubled dash opens a comment, so a `-` at the very end of a piece
    /// of input cannot be resolved until the next piece arrives. Holding it is
    /// what keeps a comment split across two lines reading as a comment.
    held_dash: bool,
    /// The first word of a statement is the only place a keyword is a keyword.
    at_statement_start: bool,
    word: String,
    /// Whether anything has been fed yet, which is what `at_statement_start`
    /// means before the first character.
    started: bool,
}

impl Scanner {
    /// What the scan has found so far.
    ///
    /// A word still being read and a dash still being held are both answered
    /// **provisionally**: the single-pass walk resolved them at end of input,
    /// and here the input may not have ended, so they are applied to the answer
    /// without being consumed. Feeding the rest and asking again gives the same
    /// result the single pass would have.
    pub(crate) fn state(&self) -> Scan {
        let mut open_transaction = self.open_transaction;
        if self.at_statement_start && !self.word.is_empty() {
            boundary(&self.word, &mut open_transaction);
        }
        Scan {
            closed: self.closed && !self.tail && !self.held_dash,
            substantial: self.substantial || self.held_dash,
            open_transaction,
        }
    }

    /// Read the next piece of input, continuing where the last one stopped.
    ///
    /// Reads the piece **to its end** rather than stopping at the first `;`.
    /// Stopping there was the older behaviour and it made the scan answer about
    /// a prefix while the caller submitted the whole line: a `SELECT` sharing a
    /// line with a following `BEGIN` was reported as a closed statement, the
    /// line went over as one script ending with a transaction open, and the
    /// store discarded the read nobody was told about (Q-370).
    ///
    /// So `closed` means *a statement ended in what has been fed and nothing was
    /// begun after it*, and `open_transaction` describes the end of everything
    /// fed. A caller submits when a statement has closed **and** no transaction
    /// is still open, which is the pair of facts it actually needs. A `closed`
    /// that stayed true once any `;` had been read was the same prefix answer
    /// one step later: `A; SELECT x` went over before its `FROM` (Q-872).
    pub(crate) fn feed(&mut self, text: &str) {
        if !self.started {
            self.started = true;
            self.at_statement_start = true;
        }
        let mut quote = self.quote;
        let mut escaped = self.escaped;
        let mut commented = self.commented;
        let mut substantial = self.substantial;
        let mut open_transaction = self.open_transaction;
        let mut closed = self.closed;
        let mut tail = self.tail;
        let mut at_statement_start = self.at_statement_start;
        let mut word = core::mem::take(&mut self.word);
        let mut characters = text.chars().peekable();
        // A dash held from the previous piece is resolved against this one's
        // first character before anything else looks at it.
        if self.held_dash {
            self.held_dash = false;
            if characters.peek() == Some(&'-') {
                let _ = characters.next();
                commented = true;
            } else {
                substantial = true;
                tail = true;
            }
        }
        while let Some(character) = characters.next() {
            // A word ends at anything that cannot be inside one.
            if !character.is_alphanumeric() && character != '_' && !word.is_empty() {
                if at_statement_start {
                    boundary(&word, &mut open_transaction);
                    at_statement_start = false;
                }
                word.clear();
            }

            if commented {
                commented = character != '\n';
                continue;
            }
            if escaped {
                escaped = false;
                continue;
            }
            match (quote, character) {
                (Some(_), '\\') => escaped = true,
                (Some(open), held) if held == open => quote = None,
                (Some(_), _) => {}
                (None, '\'' | '"') => {
                    substantial = true;
                    tail = true;
                    at_statement_start = false;
                    quote = Some(character);
                }
                // Only a doubled dash opens a comment. A single one is arithmetic,
                // and `-1` is a number — the lexer draws the same line.
                (None, '-') if characters.peek() == Some(&'-') => {
                    let _ = characters.next();
                    commented = true;
                }
                // A `-` at the very end of this piece cannot be judged yet: whether
                // it opens a comment depends on a character that has not arrived.
                (None, '-') if characters.peek().is_none() => self.held_dash = true,
                (None, ';') => {
                    substantial = true;
                    at_statement_start = true;
                    // A `;` inside a transaction ends a statement and not the group
                    // that has to be submitted together. Closing here is what made a
                    // `BEGIN;` in a file arrive on its own, be discarded for ending
                    // with a transaction open, and leave every statement after it
                    // to commit by itself.
                    if !open_transaction {
                        closed = true;
                        tail = false;
                    }
                }
                (None, held) if held.is_alphanumeric() || held == '_' => {
                    substantial = true;
                    tail = true;
                    word.push(held);
                }
                (None, held) => {
                    if !held.is_whitespace() {
                        substantial = true;
                        tail = true;
                    }
                }
            }
        }
        self.quote = quote;
        self.escaped = escaped;
        self.commented = commented;
        self.substantial = substantial;
        self.open_transaction = open_transaction;
        self.closed = closed;
        self.tail = tail;
        self.at_statement_start = at_statement_start;
        // The word is NOT terminated here — it may continue into the next piece.
        // Its effect on a transaction boundary is applied provisionally by
        // [`Scanner::state`] instead, which is what the single-pass walk did at
        // end of input.
        self.word = word;
    }
}
