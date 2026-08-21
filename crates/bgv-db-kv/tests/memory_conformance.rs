//! The in-memory backend runs the full backend conformance suite.
//!
//! This is the same suite every other backend will run. Keeping it in one place
//! is what makes "the trait is swappable" a checked claim rather than an
//! intention.

use bgv_db_kv::{MemoryBackend, conformance};

#[test]
fn memory_backend_satisfies_the_backend_contract() {
    let results = conformance::run_all(MemoryBackend::new);

    assert_eq!(
        results.len(),
        conformance::check_count(),
        "every declared check must run",
    );

    let failures: Vec<String> = results
        .iter()
        .filter_map(|result| {
            result
                .failure
                .as_ref()
                .map(|reason| format!("{}: {reason}", result.name))
        })
        .collect();

    assert!(
        failures.is_empty(),
        "{} of {} conformance checks failed:\n  {}",
        failures.len(),
        results.len(),
        failures.join("\n  "),
    );
}

#[test]
fn suite_covers_every_documented_contract_rule() {
    // The trait documents five numbered guarantees. Each has at least one check
    // whose name names it, so a rule cannot be silently dropped from the suite.
    let results = conformance::run_all(MemoryBackend::new);
    let names: Vec<&str> = results.iter().map(|result| result.name).collect();

    for required in [
        "scan-is-ordered",                    // rule 1 — ordering
        "reverse-scan-mirrors-forward",       // rule 1 — reverse ordering
        "batch-is-atomic-across-keyspaces",   // rule 2 — atomicity
        "failed-precondition-writes-nothing", // rule 3 — precondition coherence
        "keyspaces-are-isolated",             // rule 4 — keyspace isolation
        "absence-is-a-value",                 // rule 5 — absence is not an error
    ] {
        assert!(
            names.contains(&required),
            "conformance suite is missing the check for {required}",
        );
    }
}
