//! The corpus format: what a case is, and how one is read from a file.
//!
//! A corpus file is a **session**. Its cases run in order against one store, so
//! a case may write what a later case reads — which is how a language is
//! actually used, and how a suite of independent one-statement cases quietly
//! fails to test anything about state.
//!
//! ```text
//! # a comment
//! --- case: a table is defined and written
//! DEFINE TABLE users;
//! CREATE users:1 = { name: 'ada' };
//! --- expect: ok
//!
//! --- case: the record is there afterwards
//! SELECT * FROM users;
//! --- expect: rows 1
//! ```
//!
//! Expectations are written in bgvQL where they name a value, because a suite
//! that needs a second language to say what it expects has two definitions to
//! keep in step instead of one.

/// What a case expects to happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expectation {
    /// The script runs and nothing fails.
    Ok,
    /// The script fails, and the failure is of this kind.
    ///
    /// The kind is the error's variant name, which is the coarsest thing a
    /// corpus can assert without pinning message wording that is free to
    /// improve.
    Error(String),
    /// The last statement answered with this many records.
    Rows(usize),
    /// The last statement answered with this many keys.
    Keys(usize),
    /// The last statement answered with this value, written as bgvQL.
    Value(String),
}

/// One case: a script and what it should do.
#[derive(Debug, Clone)]
pub struct Case {
    /// What the case is called, for the failure message.
    pub name: String,
    /// The bgvQL to run.
    pub script: String,
    /// What is expected of it.
    pub expectation: Expectation,
    /// The line the case began on, so a failure can be found in the file.
    pub line: usize,
}

/// A corpus file, read.
#[derive(Debug, Clone)]
pub struct Corpus {
    /// What the file is called.
    pub name: String,
    /// Its cases, in the order they run.
    pub cases: Vec<Case>,
}

/// Why a corpus file could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedCorpus {
    /// What is wrong.
    pub reason: String,
    /// Where it is wrong.
    pub line: usize,
}

impl core::fmt::Display for MalformedCorpus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "line {}: {}", self.line, self.reason)
    }
}

const CASE: &str = "--- case:";
const EXPECT: &str = "--- expect:";

/// Read a corpus from the text of a file.
///
/// # Errors
///
/// Returns the first malformation, with its line. A corpus that cannot be read
/// is a failure of the suite rather than of the language, and saying which is
/// the whole point of refusing it here.
pub fn read(name: &str, text: &str) -> Result<Corpus, MalformedCorpus> {
    let mut cases = Vec::new();
    let mut pending: Option<(String, String, usize)> = None;

    for (index, raw) in text.lines().enumerate() {
        let line = index.saturating_add(1);
        let trimmed = raw.trim();

        if let Some(title) = trimmed.strip_prefix(CASE) {
            if pending.is_some() {
                return Err(MalformedCorpus {
                    reason: "a case began before the one before it was expected".to_owned(),
                    line,
                });
            }
            pending = Some((title.trim().to_owned(), String::new(), line));
            continue;
        }

        if let Some(written) = trimmed.strip_prefix(EXPECT) {
            let Some((name, script, at)) = pending.take() else {
                return Err(MalformedCorpus {
                    reason: "an expectation with no case before it".to_owned(),
                    line,
                });
            };
            let expectation = expectation(written.trim(), line)?;
            cases.push(Case {
                name,
                script,
                expectation,
                line: at,
            });
            continue;
        }

        // Outside a case, only comments and blank lines are allowed — a stray
        // statement there would run nowhere and be read as if it had.
        let Some((_, script, _)) = pending.as_mut() else {
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            return Err(MalformedCorpus {
                reason: format!("a line outside any case: {trimmed}"),
                line,
            });
        };
        if trimmed.starts_with('#') {
            continue;
        }
        script.push_str(raw);
        script.push('\n');
    }

    if let Some((name, _, line)) = pending {
        return Err(MalformedCorpus {
            reason: format!("case {name:?} has no expectation"),
            line,
        });
    }
    Ok(Corpus {
        name: name.to_owned(),
        cases,
    })
}

fn expectation(written: &str, line: usize) -> Result<Expectation, MalformedCorpus> {
    let (word, rest) = match written.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest.trim()),
        None => (written, ""),
    };
    let malformed = |reason: String| MalformedCorpus { reason, line };
    match word {
        "ok" => Ok(Expectation::Ok),
        "error" if !rest.is_empty() => Ok(Expectation::Error(rest.to_owned())),
        "rows" => rest
            .parse()
            .map(Expectation::Rows)
            .map_err(|_| malformed(format!("{rest:?} is not a count"))),
        "keys" => rest
            .parse()
            .map(Expectation::Keys)
            .map_err(|_| malformed(format!("{rest:?} is not a count"))),
        "value" if !rest.is_empty() => Ok(Expectation::Value(rest.to_owned())),
        _ => Err(malformed(format!("{written:?} is not an expectation"))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn a_file_reads_into_its_cases_in_order() {
        let corpus = read(
            "sample",
            "# a note\n\
             --- case: first\n\
             DEFINE TABLE users;\n\
             --- expect: ok\n\
             \n\
             --- case: second\n\
             SELECT * FROM users;\n\
             --- expect: rows 0\n",
        )
        .unwrap();
        assert_eq!(corpus.cases.len(), 2);
        assert_eq!(corpus.cases[0].name, "first");
        assert_eq!(corpus.cases[0].script.trim(), "DEFINE TABLE users;");
        assert_eq!(corpus.cases[1].expectation, Expectation::Rows(0));
    }

    #[test]
    fn every_expectation_kind_reads() {
        for (written, expected) in [
            ("ok", Expectation::Ok),
            (
                "error NoIndexOnField",
                Expectation::Error("NoIndexOnField".to_owned()),
            ),
            ("rows 3", Expectation::Rows(3)),
            ("keys 2", Expectation::Keys(2)),
            ("value 'ada'", Expectation::Value("'ada'".to_owned())),
        ] {
            assert_eq!(expectation(written, 1), Ok(expected), "{written}");
        }
    }

    #[test]
    fn a_corpus_that_cannot_be_read_says_where() {
        // A statement outside a case would run nowhere while reading as though
        // it had, so it is refused rather than skipped.
        let error = read("bad", "SELECT * FROM users;\n").unwrap_err();
        assert_eq!(error.line, 1);

        let error = read("bad", "--- case: unfinished\nSELECT * FROM users;\n").unwrap_err();
        assert!(error.reason.contains("no expectation"), "{error}");

        let error = read("bad", "--- expect: ok\n").unwrap_err();
        assert!(error.reason.contains("no case"), "{error}");
    }
}
