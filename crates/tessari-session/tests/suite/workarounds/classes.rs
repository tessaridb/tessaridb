//! A refusal's class follows its cause (ADR-0124 D7).

use tessari_session::Error;
use tessari_types::RefusalClass;

use super::{inside, refused, run, store};

#[test]
fn updating_a_record_that_is_not_there_is_a_conflict_not_an_invalid_request() {
    let store = store();
    let mut session = inside(&store);
    run(&mut session, "DEFINE COLLECTION orders;");
    let error = refused(&mut session, "UPDATE orders:404 MERGE { n: 1 };");
    assert!(matches!(error, Error::NoSuchRecord { .. }), "{error:?}");
    assert_eq!(error.class(), RefusalClass::Conflict);
}

#[test]
fn a_failing_event_carries_the_class_of_what_failed_in_its_body() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE COLLECTION orders; DEFINE COLLECTION stock; \
         DEFINE EVENT take ON orders FOR CREATE THEN UPDATE stock:$id MERGE { taken: true }; \
         DEFINE EVENT ruled ON orders FOR UPDATE THEN THROW 'never on a sunday'; \
         DEFINE COLLECTION bad; \
         DEFINE EVENT lost ON bad THEN CREATE nowhere = { a: 1 };",
    );
    let conflict = refused(&mut session, "CREATE orders:1 = { n: 1 };");
    assert!(
        matches!(conflict, Error::EventFailed { .. }),
        "{conflict:?}"
    );
    assert_eq!(
        conflict.class(),
        RefusalClass::Conflict,
        "the body's NoSuchRecord"
    );

    run(
        &mut session,
        "CREATE stock:2 = {}; CREATE orders:2 = { n: 2 };",
    );
    let thrown = refused(&mut session, "UPDATE orders:2 MERGE { n: 3 };");
    assert!(matches!(thrown, Error::EventFailed { .. }), "{thrown:?}");
    assert_eq!(
        thrown.class(),
        RefusalClass::Invalid,
        "a THROW is invalid wherever it is"
    );

    let missing = refused(&mut session, "CREATE bad:1 = {};");
    assert!(matches!(missing, Error::EventFailed { .. }), "{missing:?}");
    assert_eq!(
        missing.class(),
        RefusalClass::Invalid,
        "the body's missing table"
    );
}
