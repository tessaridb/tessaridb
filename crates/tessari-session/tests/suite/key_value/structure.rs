//! Lists and hashes on a space key (G079 C6): each write one statement that
//! reads the key and writes it, so a neighbour's write is never lost.

use std::thread;

use tessari_session::{RunNow, Session};
use tessari_types::{Number, Value};

use super::{on_each_backend, opened, refused, run, value};

fn integer(n: i64) -> Value {
    Value::Number(Number::Integer(n))
}

fn texts(items: &[&str]) -> Value {
    Value::Array(items.iter().map(|item| Value::from(*item)).collect())
}

#[test]
fn a_list_is_pushed_and_popped_at_either_end() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        assert_eq!(
            value(&mut session, "LIST PUSH cache:'l' BACK 'a', 'b';"),
            integer(2),
            "{}",
            backend.name
        );
        // Each value goes to the front in turn, so the last named is first.
        assert_eq!(
            value(&mut session, "LIST PUSH cache:'l' FRONT 'y', 'z';"),
            integer(4)
        );
        assert_eq!(
            value(&mut session, "GET cache:'l';"),
            texts(&["z", "y", "a", "b"])
        );
        assert_eq!(
            value(&mut session, "LIST POP cache:'l' FRONT;"),
            Value::from("z")
        );
        assert_eq!(
            value(&mut session, "LIST POP cache:'l' BACK LIMIT 2;"),
            texts(&["b", "a"])
        );
        assert_eq!(
            value(&mut session, "RETURN array::len(GET cache:'l');"),
            integer(1)
        );
        assert_eq!(
            value(&mut session, "LIST POP cache:'l' BACK LIMIT 5;"),
            texts(&["y"])
        );
        // The last element gone, the key is gone: no key holds an empty list.
        assert_eq!(value(&mut session, "GET cache:'l';"), Value::None);
        assert_eq!(
            value(&mut session, "LIST POP cache:'l' FRONT;"),
            Value::None,
            "{}",
            backend.name
        );
        assert_eq!(
            value(&mut session, "LIST POP cache:'l' FRONT LIMIT 3;"),
            Value::Array(Vec::new())
        );
    });
}

#[test]
fn a_push_if_present_never_makes_a_list() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        assert_eq!(
            value(&mut session, "LIST PUSH cache:'l' BACK 'a' IF PRESENT;"),
            integer(0)
        );
        assert_eq!(
            value(&mut session, "GET cache:'l';"),
            Value::None,
            "{}",
            backend.name
        );
        run(&mut session, "LIST PUSH cache:'l' BACK 'a';");
        assert_eq!(
            value(&mut session, "LIST PUSH cache:'l' FRONT 'b' IF PRESENT;"),
            integer(2)
        );
    });
}

#[test]
fn a_list_is_set_trimmed_searched_and_inserted_into() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "LIST PUSH cache:'l' BACK 'a', 'b', 'a', 'c', 'a';",
        );
        assert_eq!(
            value(&mut session, "LIST REMOVE cache:'l' 'a' LIMIT 1 FROM BACK;"),
            integer(1)
        );
        assert_eq!(
            value(&mut session, "GET cache:'l';"),
            texts(&["a", "b", "a", "c"])
        );
        assert_eq!(
            value(&mut session, "LIST REMOVE cache:'l' 'a';"),
            integer(2),
            "{}: no limit removes every one",
            backend.name
        );
        assert_eq!(
            value(&mut session, "LIST INSERT cache:'l' 'x' BEFORE 'c';"),
            integer(3)
        );
        assert_eq!(
            value(&mut session, "LIST INSERT cache:'l' 'y' AFTER 'c';"),
            integer(4)
        );
        assert_eq!(
            value(&mut session, "LIST INSERT cache:'l' 'y' AFTER 'nope';"),
            integer(-1)
        );
        assert_eq!(
            value(&mut session, "LIST INSERT cache:'none' 'y' AFTER 'c';"),
            integer(0)
        );
        run(&mut session, "LIST SET cache:'l' AT -1 = 'z';");
        assert_eq!(
            value(&mut session, "GET cache:'l';"),
            texts(&["b", "x", "c", "z"])
        );
        let why = refused(&mut session, "LIST SET cache:'l' AT 4 = 'q';");
        assert!(why.contains("index 4"), "{}: {why}", backend.name);
        let why = refused(&mut session, "LIST SET cache:'none' AT 0 = 'q';");
        assert!(why.contains("no list"), "{}: {why}", backend.name);
        run(&mut session, "LIST TRIM cache:'l' FROM 1 TO -2;");
        assert_eq!(value(&mut session, "GET cache:'l';"), texts(&["x", "c"]));
        run(&mut session, "LIST TRIM cache:'l' FROM 5 TO 9;");
        assert_eq!(
            value(&mut session, "GET cache:'l';"),
            Value::None,
            "{}: trimmed to nothing is no key",
            backend.name
        );
    });
}

#[test]
fn an_element_moves_between_lists_in_one_statement() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "LIST PUSH cache:'from' BACK 'a', 'b';");
        assert_eq!(
            value(
                &mut session,
                "LIST MOVE cache:'from' BACK TO cache:'to' FRONT;"
            ),
            Value::from("b")
        );
        assert_eq!(
            value(
                &mut session,
                "LIST MOVE cache:'from' FRONT TO cache:'to' BACK;"
            ),
            Value::from("a")
        );
        assert_eq!(value(&mut session, "GET cache:'from';"), Value::None);
        assert_eq!(
            value(&mut session, "GET cache:'to';"),
            texts(&["b", "a"]),
            "{}",
            backend.name
        );
        assert_eq!(
            value(
                &mut session,
                "LIST MOVE cache:'from' FRONT TO cache:'to' BACK;"
            ),
            Value::None
        );
    });
}

#[test]
fn a_hash_set_counts_the_fields_it_added() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        assert_eq!(
            value(&mut session, "HASH SET cache:'h' 'a' = 1, 'b' = 'x';"),
            integer(2),
            "{}",
            backend.name
        );
        assert_eq!(
            value(&mut session, "HASH SET cache:'h' 'a' = 2, 'c' = 3;"),
            integer(1)
        );
        assert_eq!(
            value(&mut session, "HASH SET cache:'h' 'a' = 9 IF ABSENT;"),
            integer(0)
        );
        assert_eq!(
            value(&mut session, "HASH SET cache:'h' 'd' = 4 IF ABSENT;"),
            integer(1)
        );
        assert_eq!(value(&mut session, "RETURN (GET cache:'h').a;"), integer(2));
        assert_eq!(
            value(&mut session, "RETURN object::len(GET cache:'h');"),
            integer(4)
        );
        assert_eq!(
            value(&mut session, "HASH REMOVE cache:'h' 'a', 'zz';"),
            integer(1)
        );
        assert_eq!(
            value(&mut session, "HASH REMOVE cache:'h' 'b', 'c', 'd';"),
            integer(3)
        );
        assert_eq!(
            value(&mut session, "GET cache:'h';"),
            Value::None,
            "{}: the last field gone, the key is gone",
            backend.name
        );
    });
}

#[test]
fn a_hash_field_is_incremented_in_place() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        assert_eq!(
            value(&mut session, "HASH INCR cache:'h' 'n' BY 5;"),
            integer(5)
        );
        assert_eq!(
            value(&mut session, "HASH INCR cache:'h' 'n' BY -2;"),
            integer(3)
        );
        assert_eq!(
            value(&mut session, "HASH INCR cache:'h' 'f' BY 1.5;"),
            Value::Number(Number::Float(1.5))
        );
        run(&mut session, "HASH SET cache:'h' 's' = 'word';");
        let why = refused(&mut session, "HASH INCR cache:'h' 's' BY 1;");
        assert!(why.contains("string"), "{}: {why}", backend.name);
        assert_eq!(value(&mut session, "RETURN (GET cache:'h').n;"), integer(3));
    });
}

#[test]
fn a_structure_write_on_another_kind_is_refused_by_name() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "SET cache:'s' = 'x';");
        let why = refused(&mut session, "LIST PUSH cache:'s' BACK 1;");
        assert!(
            why.contains("holds a string, not a list"),
            "{}: {why}",
            backend.name
        );
        run(&mut session, "LIST PUSH cache:'l' BACK 1;");
        let why = refused(&mut session, "HASH SET cache:'l' 'a' = 1;");
        assert!(why.contains("holds a list, not a hash"), "{why}");
        assert_eq!(value(&mut session, "GET cache:'s';"), Value::from("x"));
    });
}

#[test]
fn a_structure_write_keeps_the_expiry_the_key_had() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "LIST PUSH cache:'l' BACK 1;");
        run(&mut session, "EXPIRE cache:'l' 1h;");
        run(&mut session, "LIST PUSH cache:'l' BACK 2;");
        run(&mut session, "HASH SET cache:'h' 'a' = 1;");
        run(&mut session, "EXPIRE cache:'h' 1h;");
        run(&mut session, "HASH INCR cache:'h' 'a' BY 1;");
        for key in ["l", "h"] {
            assert!(
                matches!(
                    value(&mut session, &format!("RETURN TTL cache:'{key}';")),
                    Value::Duration(_)
                ),
                "{}: {key}",
                backend.name
            );
        }
    });
}

/// C6: N writers on one key, none of their writes lost and none refused.
#[test]
fn concurrent_structure_writes_all_land_and_none_is_refused() {
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
                                let push = format!("LIST PUSH cache:'l' BACK '{writer}-{turn}';");
                                let field = format!("HASH INCR cache:'h' 'w{}' BY 1;", writer % 3);
                                usize::from(session.run_now(&push).is_err())
                                    + usize::from(session.run_now(&field).is_err())
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
        let total = i64::try_from(WRITERS * EACH).unwrap();
        let counted = value(
            &mut session,
            "RETURN [array::len(GET cache:'l'), array::sum(object::values(GET cache:'h'))];",
        );
        assert_eq!(
            (counted, refusals),
            (Value::Array(vec![integer(total), integer(total)]), 0),
            "{}: every push and every increment landed and none was refused",
            backend.name
        );
        let distinct = value(
            &mut session,
            "RETURN array::len(array::distinct(GET cache:'l'));",
        );
        assert_eq!(distinct, integer(total), "{}: none twice", backend.name);
    });
}
