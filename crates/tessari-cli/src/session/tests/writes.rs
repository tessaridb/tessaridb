use super::*;

#[test]
fn a_write_the_store_named_answers_with_the_identity_it_produced() {
    // `ok` was the answer here until this wave, and it threw away the only
    // route back to the record: the caller did not choose the identity and
    // has no statement that would find it again.
    assert_eq!(said("CREATE users = { name: 'ada' };"), ["1"]);
}

#[test]
fn a_batch_insert_answers_with_one_identity_per_row() {
    assert_eq!(
        said("INSERT INTO users (name) VALUES ('ada'), ('grace'), ('alan');"),
        ["1", "2", "3"]
    );
}

#[test]
fn the_identity_a_uuid_table_answers_with_is_one_the_grammar_reads() {
    // The case the integer default hides, and the one this wave is for.
    // Before it, a uuid table answered with thirty-two undivided hex digits
    // — which the grammar does not read as an identity at all, so pasting
    // the answer back produced "not a duration this store can hold", a
    // refusal naming nothing a reader could act on.
    //
    // That the spelling then *finds* the record is asserted where the store
    // outlives the statement: `tessari-ql`'s `identity_spelling` parses it
    // back for all four kinds, and `tessari-session`'s `store_named_records`
    // reads the record at it. Split that way because this harness gives each
    // script its own database, so a second script here could only address a
    // record the first one did not write.
    let produced = said("CREATE sessions = { token: 'abc' };");
    let [identity] = produced.as_slice() else {
        panic!("expected one identity, got {produced:?}");
    };
    assert!(
        identity.starts_with("uuid '") && identity.ends_with('\''),
        "a uuid table should answer in the spelling the grammar reads: {identity}"
    );
    assert_eq!(
        identity.len(),
        "uuid '".len() + 36 + 1,
        "the canonical 8-4-4-4-12 form, not the undivided digits: {identity}"
    );
}

#[test]
fn an_addressed_write_still_answers_the_way_it_did() {
    // The relaxation is about the write that has no identity to report. A
    // caller who supplied one is being told nothing new by hearing it back.
    assert_eq!(
        said("CREATE users:9 = { name: 'ada' };"),
        Vec::<String>::new()
    );
}
