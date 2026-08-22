//! The suite: every corpus, the coverage ratchet, and the document.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};

use bgv_db_conformance::{FORMS, forms_in, read, run, uncovered};

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
            if path.extension()?.to_str()? != "bgvql" {
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
            if let Ok(script) = bgv_db_ql::parse(&case.script) {
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
        .join("../../docs/bgvql.md")
        .canonicalize()
        .expect("docs/bgvql.md is missing");
    let text = fs::read_to_string(&path).unwrap();

    let mut examples = 0_usize;
    let mut failures = Vec::new();
    for (number, block) in fenced_blocks(&text) {
        for statement in split_statements(&block) {
            examples = examples.saturating_add(1);
            if let Err(error) = bgv_db_ql::parse(&statement) {
                failures.push(format!(
                    "docs/bgvql.md:{number} — {}\n      {error}",
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
    // Whether a fence is open, and whether what it holds is bgvQL. A **labelled**
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
