//! Sets of members and members ranked by score on a space key (G079 C6).

use std::collections::BTreeSet;
use std::thread;

use tessari_session::{RunNow, Session};
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

/// `[member, score]` pairs as a pop answers them.
fn popped(pairs: &[(&str, f64)]) -> Value {
    Value::Array(
        pairs
            .iter()
            .map(|(member, score)| Value::Array(vec![Value::from(*member), float(*score)]))
            .collect(),
    )
}

#[test]
fn members_are_added_once_and_removed() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        assert_eq!(
            value(&mut session, "MEMBERS ADD cache:'s' 'a', 'b', 'a';"),
            integer(2),
            "{}",
            backend.name
        );
        assert_eq!(
            value(&mut session, "MEMBERS ADD cache:'s' 'b', 'c';"),
            integer(1)
        );
        assert_eq!(
            value(&mut session, "GET cache:'s';"),
            texts(&["a", "b", "c"])
        );
        assert_eq!(
            value(&mut session, "MEMBERS REMOVE cache:'s' 'a', 'zz';"),
            integer(1)
        );
        assert_eq!(
            value(&mut session, "MEMBERS REMOVE cache:'s' 'b', 'c';"),
            integer(2)
        );
        assert_eq!(
            value(&mut session, "GET cache:'s';"),
            Value::None,
            "{}: the last member gone, the key is gone",
            backend.name
        );
        let why = refused(&mut session, "MEMBERS ADD cache:'s' [1, 2];");
        assert!(why.contains("a member is a single value"), "{why}");
    });
}

#[test]
fn a_member_is_popped_at_random_and_moved_between_sets() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "MEMBERS ADD cache:'s' 'a', 'b', 'c';");
        let one = value(&mut session, "MEMBERS POP cache:'s';");
        let two = value(&mut session, "MEMBERS POP cache:'s' LIMIT 5;");
        let Value::Array(two) = two else {
            panic!("{}: a limited pop answers an array", backend.name);
        };
        let all: BTreeSet<Value> = std::iter::once(one).chain(two).collect();
        assert_eq!(Value::Set(all), texts(&["a", "b", "c"]), "{}", backend.name);
        assert_eq!(value(&mut session, "GET cache:'s';"), Value::None);
        assert_eq!(value(&mut session, "MEMBERS POP cache:'s';"), Value::None);
        run(&mut session, "MEMBERS ADD cache:'from' 'x', 'y';");
        assert_eq!(
            value(&mut session, "MEMBERS MOVE cache:'from' 'x' TO cache:'to';"),
            Value::Bool(true)
        );
        assert_eq!(
            value(
                &mut session,
                "MEMBERS MOVE cache:'from' 'nope' TO cache:'to';"
            ),
            Value::Bool(false)
        );
        assert_eq!(value(&mut session, "GET cache:'from';"), texts(&["y"]));
        assert_eq!(
            value(&mut session, "GET cache:'to';"),
            texts(&["x"]),
            "{}",
            backend.name
        );
    });
}

#[test]
fn scores_rank_members_and_answer_what_they_changed() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        assert_eq!(
            value(&mut session, "SCORES ADD cache:'z' 'bob' = 7, 'ada' = 10;"),
            integer(2),
            "{}",
            backend.name
        );
        assert_eq!(
            value(
                &mut session,
                "SCORES ADD cache:'z' 'bob' = 8, 'cy' = 1 CHANGED;"
            ),
            integer(2)
        );
        assert_eq!(
            value(&mut session, "SCORES ADD cache:'z' 'bob' = 3 IF GREATER;"),
            integer(0)
        );
        assert_eq!(
            value(
                &mut session,
                "SCORES ADD cache:'z' 'bob' = 9 IF GREATER CHANGED;"
            ),
            integer(1)
        );
        assert_eq!(
            value(&mut session, "SCORES ADD cache:'z' 'ada' = 1 IF ABSENT;"),
            integer(0)
        );
        assert_eq!(
            value(&mut session, "SCORES ADD cache:'z' 'dee' = 5 IF PRESENT;"),
            integer(0)
        );
        assert_eq!(
            value(&mut session, "SCORES INCR cache:'z' 'cy' BY 2.5;"),
            float(3.5)
        );
        // The set's own order is the ranking: lowest score first.
        assert_eq!(
            value(&mut session, "SCORES POP cache:'z' LOWEST;"),
            popped(&[("cy", 3.5)])
        );
        assert_eq!(
            value(&mut session, "SCORES POP cache:'z' HIGHEST LIMIT 1;"),
            popped(&[("ada", 10.0)])
        );
        assert_eq!(
            value(&mut session, "SCORES REMOVE cache:'z' 'bob', 'zz';"),
            integer(1)
        );
        assert_eq!(
            value(&mut session, "GET cache:'z';"),
            Value::None,
            "{}: the last member gone, the key is gone",
            backend.name
        );
        let why = refused(&mut session, "SCORES ADD cache:'z' 'a' = 'high';");
        assert!(why.contains("a score is a finite number"), "{why}");
        let why = refused(&mut session, "SCORES ADD cache:'z' 1 = 2;");
        assert!(why.contains("a member is named by text"), "{why}");
    });
}

#[test]
fn a_set_and_a_ranking_are_told_apart() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "MEMBERS ADD cache:'s' 'a';");
        run(&mut session, "SCORES ADD cache:'z' 'a' = 1;");
        let why = refused(&mut session, "SCORES ADD cache:'s' 'b' = 2;");
        assert!(why.contains("holds a set, not a sorted set"), "{why}");
        let why = refused(&mut session, "MEMBERS ADD cache:'z' 'b';");
        assert!(
            why.contains("holds a sorted set, not a set"),
            "{}: {why}",
            backend.name
        );
    });
}

/// C6: N writers on one key, none of their writes lost and none refused.
#[test]
fn concurrent_set_writes_all_land_and_none_is_refused() {
    const WRITERS: usize = 8;
    const EACH: usize = 25;
    on_each_backend(|backend| {
        opened(&backend.store);
        let refusals: usize = thread::scope(|scope| {
            let handles: Vec<_> = (0..WRITERS)
                .map(|writer| {
                    scope.spawn(move || {
                        let mut session = Session::new(&backend.store);
                        session
                            .run_now("USE NAMESPACE prod; USE DATABASE app;")
                            .unwrap();
                        (0..EACH)
                            .map(|turn| {
                                let add = format!("MEMBERS ADD cache:'s' '{writer}-{turn}';");
                                let score =
                                    format!("SCORES INCR cache:'z' 'w{}' BY 1;", writer % 3);
                                usize::from(session.run_now(&add).is_err())
                                    + usize::from(session.run_now(&score).is_err())
                            })
                            .sum::<usize>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .sum()
        });
        let mut session = Session::new(&backend.store);
        session
            .run_now("USE NAMESPACE prod; USE DATABASE app;")
            .unwrap();
        let total = WRITERS * EACH;
        let Value::Set(members) = value(&mut session, "GET cache:'s';") else {
            panic!("{}: a set", backend.name);
        };
        let Value::Set(ranked) = value(&mut session, "GET cache:'z';") else {
            panic!("{}: a ranking", backend.name);
        };
        let scored: f64 = ranked
            .iter()
            .filter_map(|pair| match pair {
                Value::Array(pair) => pair.first().cloned(),
                _ => None,
            })
            .filter_map(|score| match score {
                Value::Number(number) => number.as_float(),
                _ => None,
            })
            .sum();
        assert_eq!(
            (members.len(), scored, refusals),
            (total, f64::from(u32::try_from(total).unwrap()), 0),
            "{}: every add and every increment landed and none was refused",
            backend.name
        );
    });
}
