//! A route after any parenthesised expression (ADR-0124 D3).

use tessari_types::{Number, Value};

use super::{inside, rows, run, store, value};

fn five() -> Value {
    Value::Number(Number::Integer(5))
}

#[test]
fn a_field_of_an_embedded_read_is_one_expression() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE COLLECTION settings; CREATE settings:'reclaim' = { grace: 5, tags: [5, 6] };",
    );
    assert_eq!(
        value(
            &mut session,
            "RETURN (SELECT grace FROM ONLY settings:'reclaim').grace;"
        ),
        five()
    );
    assert_eq!(
        value(
            &mut session,
            "RETURN (SELECT * FROM ONLY settings:'reclaim').tags[0];"
        ),
        five()
    );
    assert_eq!(value(&mut session, "RETURN ({ a: { b: 5 } }).a.b;"), five());
    assert_eq!(value(&mut session, "RETURN ([1, 5])[1];"), five());
}

#[test]
fn an_event_body_reads_a_setting_without_a_let() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE COLLECTION settings; DEFINE COLLECTION objects; DEFINE COLLECTION reclaims; \
         CREATE settings:'reclaim' = { grace: 5 }; \
         DEFINE EVENT queue_it ON objects THEN \
             CREATE reclaims = { grace: (SELECT grace FROM ONLY settings:'reclaim').grace };",
    );
    run(&mut session, "CREATE objects:1 = { key: 'a' };");
    let found = rows(&mut session, "SELECT grace FROM reclaims;");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].get("grace"), Some(&five()));
}
