use super::*;

#[test]
fn a_conflict_is_not_retryable_and_names_both_sequences() {
    let error = Error::Conflict {
        id: RecordId::from("r"),
        snapshot: Sequence::new(5),
        committed: Sequence::new(9),
        with: ConflictWith::Commit,
    };
    assert_eq!(error.category(), ErrorCategory::Conflict);
    assert!(!error.is_retryable());
    let text = error.to_string();
    assert!(text.contains('5'), "{text}");
    assert!(text.contains('9'), "{text}");
}

#[test]
fn contention_is_retryable_because_the_transaction_itself_is_still_valid() {
    assert!(Error::CommitContention { attempts: 8 }.is_retryable());
}
