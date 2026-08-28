//! The suite: every corpus, the coverage ratchet, and the document.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use tessari_conformance::{FORMS, forms_in, read, run, uncovered};

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("corpus")
}

/// Every corpus file, by name and text.
fn corpora() -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = fs::read_dir(corpus_dir())
        .expect("the corpus directory is missing")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension()?.to_str()? != "tessariql" {
                return None;
            }
            let name = path.file_stem()?.to_str()?.to_owned();
            Some((name, fs::read_to_string(&path).ok()?))
        })
        .collect();
    found.sort_by(|left, right| left.0.cmp(&right.0));
    assert!(!found.is_empty(), "no corpus files");
    found
}

#[test]
fn every_case_in_every_corpus_does_what_it_says() {
    let mut failures = Vec::new();
    let mut total = 0_usize;

    for (name, text) in corpora() {
        let corpus = match read(&name, &text) {
            Ok(corpus) => corpus,
            Err(malformed) => {
                failures.push(format!(
                    "{name}: the corpus itself is malformed — {malformed}"
                ));
                continue;
            }
        };
        for result in run(&corpus) {
            total = total.saturating_add(1);
            if let Some(failure) = result.failure {
                failures.push(format!(
                    "{}:{} {} — {failure}",
                    result.corpus, result.line, result.name
                ));
            }
        }
    }

    assert!(total > 0, "the corpora hold no cases");
    assert!(
        failures.is_empty(),
        "{} of {total} cases failed:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

#[test]
fn every_statement_form_has_a_case() {
    // The invariant the knowledge base states: a statement is in the language
    // only if the corpus contains a case for it. Adding a form to the grammar
    // fails this until one exists.
    let mut covered: Vec<&'static str> = Vec::new();
    for (name, text) in corpora() {
        let corpus = read(&name, &text).unwrap();
        for case in &corpus.cases {
            // A case that is meant to fail may still be unparseable, and an
            // unparseable case covers nothing.
            if let Ok(script) = tessari_ql::parse(&case.script) {
                covered.extend(forms_in(&script));
            }
        }
    }
    covered.sort_unstable();
    covered.dedup();

    let missing = uncovered(&covered);
    assert!(
        missing.is_empty(),
        "{} of {} statement forms have no conformance case: {missing:?}",
        missing.len(),
        FORMS.len()
    );
}

#[test]
fn every_example_in_the_specification_parses() {
    // The document and the parser drift apart silently in both directions, and
    // this is the only test that reads the document itself rather than a copy
    // of it made by hand.
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/tessariql.md")
        .canonicalize()
        .expect("docs/tessariql.md is missing");
    let text = fs::read_to_string(&path).unwrap();

    let mut examples = 0_usize;
    let mut failures = Vec::new();
    for (number, block) in fenced_blocks(&text) {
        for statement in split_statements(&block) {
            examples = examples.saturating_add(1);
            if let Err(error) = tessari_ql::parse(&statement) {
                failures.push(format!(
                    "docs/tessariql.md:{number} — {}\n      {error}",
                    statement.trim()
                ));
            }
        }
    }

    assert!(
        examples > 20,
        "only {examples} examples found; the extractor is probably reading nothing"
    );
    assert!(
        failures.is_empty(),
        "{} of {examples} examples in the specification do not parse:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

/// The fenced blocks of a markdown file, with the line each began on.
///
/// Blocks marked with a language are skipped: the examples are the unmarked
/// ones, and a `text` block is a diagram rather than a script.
fn fenced_blocks(text: &str) -> Vec<(usize, String)> {
    let mut blocks = Vec::new();
    // Whether a fence is open, and whether what it holds is TessariQL. A **labelled**
    // fence (```json, ```text) is tracked as open even though its body is
    // skipped: without that its closing line reads as an opener, every fence
    // after it pairs with the wrong partner, and the document's prose starts
    // arriving here as though it were a statement. That inverted silently until
    // a labelled fence was added beside a bare one.
    let mut open: Option<(usize, String, bool)> = None;
    for (index, line) in text.lines().enumerate() {
        let number = index.saturating_add(1);
        if let Some(rest) = line.trim_end().strip_prefix("```") {
            match open.take() {
                Some((at, body, checked)) => {
                    if checked {
                        blocks.push((at, body));
                    }
                }
                None => {
                    open = Some((
                        number.saturating_add(1),
                        String::new(),
                        rest.trim().is_empty(),
                    ));
                }
            }
            continue;
        }
        if let Some((_, body, _)) = open.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    blocks
}

/// The statements of a block, kept whole.
///
/// Splitting on `;` alone would cut a string containing one in half, so the
/// scan tracks quoting the way the lexer does.
fn split_statements(block: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for character in block.chars() {
        current.push(character);
        match quote {
            Some(open) => {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == open {
                    quote = None;
                }
            }
            None => match character {
                '\'' | '"' => quote = Some(character),
                ';' => {
                    statements.push(core::mem::take(&mut current));
                }
                _ => {}
            },
        }
    }
    if !current.trim().is_empty() {
        statements.push(current);
    }
    statements
        .into_iter()
        .filter(|statement| !statement.trim().is_empty())
        .collect()
}

#[test]
fn the_statement_splitter_does_not_cut_a_string_in_half() {
    let split = split_statements("SET k:1 = 'a;b'; SELECT * FROM users;");
    assert_eq!(split.len(), 2, "{split:?}");
    assert!(split[0].contains("'a;b'"), "{split:?}");
}

/// The document names every function and every fold the language has.
///
/// # Why this is a test and not a habit
///
/// Two waves in a row added vocabulary and neither had anything forcing a
/// documentation entry: `rand::uuid` and the two `crypto::` digests each got a
/// section of their own and were both missing from the group table that is
/// supposed to be the complete list. A reader who trusts that table would have
/// concluded the functions did not exist.
///
/// The failure is quiet in the direction that matters. A function absent from
/// the document still works, so nothing breaks, no test fails, and the only
/// symptom is that nobody uses it — which looks exactly like a feature nobody
/// wanted.
///
/// It reads the **group table** rather than searching the whole document,
/// because prose mentions a function in passing and that is not documentation:
/// the point of the table is that it is exhaustive, so this asserts precisely
/// that.
#[test]
fn the_vocabulary_tables_name_every_function_and_every_fold() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/tessariql.md")
        .canonicalize()
        .expect("docs/tessariql.md is missing");
    let text = fs::read_to_string(&path).unwrap();

    let documented: BTreeSet<String> = table_rows(&text)
        .filter_map(|(group, listed)| {
            // The group cell is written `` `string` ``, so the backticks come
            // off before the name is read.
            let group = backticked(group).next().and_then(bare)?;
            Some(
                backticked(listed)
                    .filter_map(bare)
                    .map(|name| format!("{group}::{name}"))
                    .collect::<Vec<String>>(),
            )
        })
        .flatten()
        .collect();

    let missing: Vec<&str> = tessari_ql::Function::ALL
        .iter()
        .map(|function| function.spelling())
        .filter(|spelling| !documented.contains(*spelling))
        .collect();
    assert!(
        missing.is_empty(),
        "the group table in docs/tessariql.md does not name: {missing:?}"
    );

    // The folds are spelled without a group, so they are matched against every
    // bare name any table row lists.
    let named: BTreeSet<String> = table_rows(&text)
        .flat_map(|(first, second)| {
            backticked(first)
                .chain(backticked(second))
                .filter_map(bare)
                .collect::<Vec<String>>()
        })
        .collect();
    let absent: Vec<&str> = tessari_ql::Aggregate::ALL
        .iter()
        .map(|fold| fold.spelling())
        .filter(|spelling| !named.contains(*spelling))
        .collect();
    assert!(
        absent.is_empty(),
        "no table in docs/tessariql.md names the folds: {absent:?}"
    );
}

/// The first two cells of every two-column markdown table row.
fn table_rows(text: &str) -> impl Iterator<Item = (&str, &str)> {
    text.lines().filter_map(|line| {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        // A leading and a trailing empty cell, so a two-column row is four.
        if cells.len() != 4 {
            return None;
        }
        Some((*cells.get(1)?, *cells.get(2)?))
    })
}

/// Everything written between backticks in one cell.
fn backticked(cell: &str) -> impl Iterator<Item = &str> {
    cell.split('`').skip(1).step_by(2)
}

/// A backticked token reduced to the name it declares, if it declares one.
///
/// `sha256(text)` is the name `sha256`; `count(*)` is `count`; `<expr>` and
/// `LIMIT` are not names at all.
fn bare(token: &str) -> Option<String> {
    let name = token.split('(').next()?.trim();
    let named = !name.is_empty()
        && name
            .chars()
            .all(|letter| letter.is_ascii_lowercase() || letter.is_ascii_digit() || letter == '_');
    named.then(|| name.to_owned())
}
