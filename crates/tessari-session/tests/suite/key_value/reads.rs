//! Reading a container in the language: functions over `GET k` (ADR-0135).

use tessari_types::{Number, Value};

use super::{on_each_backend, opened, refused, run, value};

fn integer(n: i64) -> Value {
    Value::Number(Number::Integer(n))
}

fn float(n: f64) -> Value {
    Value::Number(Number::float(n))
}

fn texts(items: &[&str]) -> Value {
    Value::Set(items.iter().map(|item| Value::from(*item)).collect())
}

/// `[member, score]` pairs as a ranking read answers them.
fn pairs(items: &[(&str, f64)]) -> Value {
    Value::Array(
        items
            .iter()
            .map(|(member, score)| Value::Array(vec![Value::from(*member), float(*score)]))
            .collect(),
    )
}

/// A ranking in its stored form, `[score, member]` pairs.
fn ranking(items: &[(&str, f64)]) -> Value {
    Value::Set(
        items
            .iter()
            .map(|(member, score)| Value::Array(vec![float(*score), Value::from(*member)]))
            .collect(),
    )
}

#[test]
fn sets_are_counted_and_combined() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "MEMBERS ADD cache:'a' 'x', 'y', 'z'; MEMBERS ADD cache:'b' 'y', 'z', 'w';",
        );
        assert_eq!(
            value(&mut session, "RETURN set::len(GET cache:'a');"),
            integer(3),
            "{}",
            backend.name
        );
        assert_eq!(
            value(
                &mut session,
                "RETURN set::union(GET cache:'a', GET cache:'b');"
            ),
            texts(&["w", "x", "y", "z"])
        );
        assert_eq!(
            value(
                &mut session,
                "RETURN set::intersect(GET cache:'a', GET cache:'b');"
            ),
            texts(&["y", "z"])
        );
        assert_eq!(
            value(
                &mut session,
                "RETURN set::difference(GET cache:'a', GET cache:'b');"
            ),
            texts(&["x"])
        );
        assert_eq!(
            value(
                &mut session,
                "RETURN set::union(GET cache:'a', GET cache:'none');"
            ),
            Value::None,
            "an absent key propagates"
        );
        assert_eq!(
            value(
                &mut session,
                "RETURN set::union(GET cache:'a', GET cache:'none' ?? set []);"
            ),
            texts(&["x", "y", "z"])
        );
        // What a combination answers is a members set a key can hold.
        run(
            &mut session,
            "SET cache:'both' = set::intersect(GET cache:'a', GET cache:'b');",
        );
        assert_eq!(
            value(&mut session, "MEMBERS ADD cache:'both' 'v';"),
            integer(1),
            "{}: the stored answer is a set of members",
            backend.name
        );
    });
}

#[test]
fn a_set_function_refuses_what_is_not_a_set_of_members() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "LIST PUSH cache:'l' BACK 'x'; MEMBERS ADD cache:'s' 'x'; SCORES ADD cache:'r' 'x' = 1;",
        );
        let why = refused(&mut session, "RETURN set::len(GET cache:'l');");
        assert!(
            why.contains("set::len wants a set"),
            "{}: {why}",
            backend.name
        );
        let why = refused(
            &mut session,
            "RETURN set::union(GET cache:'s', GET cache:'r');",
        );
        assert!(why.contains("not a ranking"), "{why}");
        assert_eq!(
            value(&mut session, "RETURN set::len(GET cache:'r');"),
            integer(1),
            "a ranking is counted by its members"
        );
    });
}

#[test]
fn an_element_and_a_field_are_chosen_by_a_value() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "LIST PUSH cache:'l' BACK 'a', 'b', 'c'; HASH SET cache:'h' 'f1' = 1, 'f2' = 'two';",
        );
        assert_eq!(
            value(
                &mut session,
                "LET $i = 1; RETURN array::at(GET cache:'l', $i);"
            ),
            Value::from("b"),
            "{}",
            backend.name
        );
        assert_eq!(
            value(&mut session, "RETURN array::at(GET cache:'l', -1);"),
            Value::from("c")
        );
        assert_eq!(
            value(&mut session, "RETURN array::at(GET cache:'l', 3);"),
            Value::None
        );
        assert_eq!(
            value(&mut session, "RETURN array::at(GET cache:'l', -4);"),
            Value::None
        );
        assert_eq!(
            value(
                &mut session,
                "LET $f = 'f2'; RETURN object::get(GET cache:'h', $f);"
            ),
            Value::from("two")
        );
        assert_eq!(
            value(&mut session, "RETURN object::get(GET cache:'h', 'f9');"),
            Value::None
        );
        let why = refused(&mut session, "RETURN array::at(GET cache:'l', 1.5);");
        assert!(why.contains("whole number"), "{why}");
    });
}

#[test]
fn a_ranking_answers_scores_ranks_and_ranges() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "SCORES ADD cache:'z' 'ann' = 3, 'bob' = 1, 'cid' = 2, 'dee' = 5;",
        );
        assert_eq!(
            value(&mut session, "RETURN scores::score(GET cache:'z', 'cid');"),
            float(2.0),
            "{}",
            backend.name
        );
        assert_eq!(
            value(&mut session, "RETURN scores::score(GET cache:'z', 'eve');"),
            Value::None
        );
        assert_eq!(
            value(&mut session, "RETURN scores::rank(GET cache:'z', 'ann');"),
            integer(2)
        );
        assert_eq!(
            value(&mut session, "RETURN scores::rank(GET cache:'z', 'eve');"),
            Value::None
        );
        assert_eq!(
            value(&mut session, "RETURN scores::range(GET cache:'z', 0, 1);"),
            pairs(&[("bob", 1.0), ("cid", 2.0)])
        );
        assert_eq!(
            value(&mut session, "RETURN scores::range(GET cache:'z', -2, -1);"),
            pairs(&[("ann", 3.0), ("dee", 5.0)])
        );
        assert_eq!(
            value(&mut session, "RETURN scores::range(GET cache:'z', 3, 9);"),
            pairs(&[("dee", 5.0)])
        );
        assert_eq!(
            value(&mut session, "RETURN scores::range(GET cache:'z', 2, 1);"),
            Value::Array(Vec::new())
        );
        assert_eq!(
            value(&mut session, "RETURN scores::between(GET cache:'z', 2, 3);"),
            pairs(&[("cid", 2.0), ("ann", 3.0)])
        );
        assert_eq!(
            value(
                &mut session,
                "RETURN scores::between(GET cache:'z', NONE, 2);"
            ),
            pairs(&[("bob", 1.0), ("cid", 2.0)]),
            "an absent bound is open"
        );
        assert_eq!(
            value(
                &mut session,
                "RETURN scores::between(GET cache:'z', 3, NONE);"
            ),
            pairs(&[("ann", 3.0), ("dee", 5.0)])
        );
        assert_eq!(
            value(
                &mut session,
                "RETURN scores::between(GET cache:'none', 0, 9);"
            ),
            Value::None,
            "an absent ranking answers NONE"
        );
    });
}

#[test]
fn rankings_are_combined_into_a_ranking_a_key_can_hold() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "SCORES ADD cache:'a' 'ann' = 1, 'bob' = 2; SCORES ADD cache:'b' 'bob' = 10, 'cid' = 4;",
        );
        assert_eq!(
            value(
                &mut session,
                "RETURN scores::union(GET cache:'a', GET cache:'b');"
            ),
            ranking(&[("ann", 1.0), ("cid", 4.0), ("bob", 12.0)]),
            "{}",
            backend.name
        );
        assert_eq!(
            value(
                &mut session,
                "RETURN scores::intersect(GET cache:'a', GET cache:'b');"
            ),
            ranking(&[("bob", 12.0)])
        );
        run(
            &mut session,
            "SET cache:'sum' = scores::union(GET cache:'a', GET cache:'b');",
        );
        assert_eq!(
            value(&mut session, "SCORES INCR cache:'sum' 'ann' BY 1;"),
            float(2.0),
            "{}: the stored answer is a ranking",
            backend.name
        );
        let why = refused(
            &mut session,
            "MEMBERS ADD cache:'m' 'x'; RETURN scores::score(GET cache:'m', 'x');",
        );
        assert!(why.contains("wants a ranking"), "{why}");
    });
}
