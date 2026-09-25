//! The suite: every corpus, the coverage ratchet, and the document.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};

use tessari_conformance::{
    FORMS, examples, forms_in, function_spellings, read, run, run_on, specification_path,
    uncalled_functions, uncovered,
};

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

/// The key-value corpus again, on the disk backend: a space must behave the same
/// wherever it is stored (G035, owner's condition).
#[test]
fn the_key_value_corpus_holds_on_the_disk_backend() {
    let text = fs::read_to_string(corpus_dir().join("key-value.tessariql")).unwrap();
    let corpus = read("key-value.tessariql", &text).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let backend = tessari_lsm::LsmBackend::open(
        directory.path(),
        tessari_lsm::StoreConfig::new(tessari_lsm::Durability::ProcessCrashSafe),
    )
    .unwrap();
    let results = run_on(&corpus, std::sync::Arc::new(backend));
    let failures: Vec<String> = results
        .iter()
        .filter_map(|result| {
            result.failure.as_ref().map(|failure| {
                format!(
                    "{}:{} {} — {failure}",
                    result.corpus, result.line, result.name
                )
            })
        })
        .collect();
    assert!(
        results.len() > 10,
        "the corpus holds {} cases",
        results.len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
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
fn every_function_has_a_case() {
    // The sibling of the ratchet above, and it is here because its absence had
    // already cost something: four functions reached the shipped language with
    // no conformance case, and two of them — `string::lower` and
    // `string::concat` — had no test of any kind in the workspace. The
    // statement half of this rule has been enforced since the corpus existed;
    // the function half was a sentence nobody checked.
    let scripts: Vec<String> = corpora()
        .into_iter()
        .flat_map(|(name, text)| {
            read(&name, &text)
                .unwrap()
                .cases
                .into_iter()
                .map(|case| case.script)
        })
        .collect();

    let missing = uncalled_functions(&scripts);
    assert!(
        missing.is_empty(),
        "{} of {} functions have no conformance case: {missing:?}",
        missing.len(),
        function_spellings().len()
    );
}

#[test]
fn every_example_in_the_specification_parses() {
    // The document and the parser drift apart silently in both directions, and
    // this is the only test that reads the document itself rather than a copy
    // of it made by hand.
    let text = fs::read_to_string(specification_path()).unwrap();

    let mut counted = 0_usize;
    let mut failures = Vec::new();
    for (number, statement) in examples(&text) {
        counted = counted.saturating_add(1);
        if let Err(error) = tessari_ql::parse(&statement) {
            failures.push(format!(
                "docs/tessariql.md:{number} — {}\n      {error}",
                statement.trim()
            ));
        }
    }

    assert!(
        counted > 20,
        "only {counted} examples found; the extractor is probably reading nothing"
    );
    assert!(
        failures.is_empty(),
        "{} of {counted} examples in the specification do not parse:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}
