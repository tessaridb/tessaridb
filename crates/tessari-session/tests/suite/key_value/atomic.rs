//! `INCR` and the conditional `SET` (G035 S3.1-S3.3).

use std::thread;

use tessari_session::Session;
use tessari_types::{Number, Value};

use super::{PAST_SHORT, SHORT, on_each_backend, opened, refused, run, value};

fn integer(n: i64) -> Value {
    Value::Number(Number::Integer(n))
}

#[test]
fn incr_counts_from_zero_and_answers_the_new_number() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        assert_eq!(
            value(&mut session, "INCR cache:'n';"),
            integer(1),
            "{}",
            backend.name
        );
        assert_eq!(value(&mut session, "INCR cache:'n' BY 5;"), integer(6));
        assert_eq!(value(&mut session, "INCR cache:'n' BY -2;"), integer(4));
        assert_eq!(value(&mut session, "GET cache:'n';"), integer(4));
    });
}

#[test]
fn incr_keeps_the_expiry_the_key_had() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "SET cache:'n' = 1 EXPIRE 1h;");
        run(&mut session, "INCR cache:'n';");
        assert!(
            matches!(
                value(&mut session, "RETURN TTL cache:'n';"),
                Value::Duration(_)
            ),
            "{}",
            backend.name
        );
    });
}

#[test]
fn incr_refuses_what_is_not_a_number_and_what_would_overflow() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "SET cache:'s' = 'x';");
        let why = refused(&mut session, "INCR cache:'s';");
        assert!(why.contains("string"), "{}: {why}", backend.name);
        run(&mut session, "SET cache:'big' = 9223372036854775807;");
        let why = refused(&mut session, "INCR cache:'big';");
        assert!(why.contains('+'), "{}: {why}", backend.name);
        assert_eq!(value(&mut session, "GET cache:'s';"), Value::from("x"));
    });
}

#[test]
fn set_if_absent_writes_once_and_is_a_lock_with_an_expiry() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        let take = format!("SET cache:'lock' = 'me' IF ABSENT EXPIRE {SHORT};");
        assert_eq!(
            value(&mut session, &take),
            Value::Bool(true),
            "{}",
            backend.name
        );
        assert_eq!(
            value(&mut session, "SET cache:'lock' = 'you' IF ABSENT;"),
            Value::Bool(false)
        );
        assert_eq!(value(&mut session, "GET cache:'lock';"), Value::from("me"));
        thread::sleep(PAST_SHORT);
        assert_eq!(
            value(
                &mut session,
                "SET cache:'lock' = 'you' EXPIRE 1h IF ABSENT;"
            ),
            Value::Bool(true),
            "{}: the lapsed lock is free again",
            backend.name
        );
    });
}

#[test]
fn set_if_present_never_creates_a_key() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        assert_eq!(
            value(&mut session, "SET cache:'k' = 1 IF PRESENT;"),
            Value::Bool(false)
        );
        assert_eq!(
            value(&mut session, "GET cache:'k';"),
            Value::None,
            "{}",
            backend.name
        );
        run(&mut session, "SET cache:'k' = 1;");
        assert_eq!(
            value(&mut session, "SET cache:'k' = 2 IF PRESENT;"),
            Value::Bool(true)
        );
        assert_eq!(value(&mut session, "GET cache:'k';"), integer(2));
    });
}

#[test]
fn set_if_equal_is_a_compare_and_set() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "SET cache:'v' = 'one';");
        assert_eq!(
            value(&mut session, "SET cache:'v' = 'two' IF = 'zero';"),
            Value::Bool(false)
        );
        assert_eq!(
            value(&mut session, "SET cache:'v' = 'two' IF = 'one';"),
            Value::Bool(true)
        );
        assert_eq!(
            value(&mut session, "GET cache:'v';"),
            Value::from("two"),
            "{}",
            backend.name
        );
        assert_eq!(
            value(&mut session, "SET cache:'missing' = 1 IF = 1;"),
            Value::Bool(false)
        );
    });
}

/// S3.3: no increment is lost, and none is refused, under contention.
#[test]
fn concurrent_increments_all_land_and_none_is_refused() {
    const WRITERS: i64 = 4;
    const EACH: i64 = 50;
    on_each_backend(|backend| {
        opened(&backend.store);
        let refusals: i64 = thread::scope(|scope| {
            let handles: Vec<_> = (0..WRITERS)
                .map(|_| {
                    scope.spawn(|| {
                        let mut session = Session::new(&backend.store);
                        session
                            .run("USE NAMESPACE prod; USE DATABASE app;")
                            .unwrap();
                        (0..EACH)
                            .filter(|_| session.run("INCR cache:'hits';").is_err())
                            .count()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| i64::try_from(handle.join().unwrap()).unwrap())
                .sum()
        });
        let mut session = Session::new(&backend.store);
        session
            .run("USE NAMESPACE prod; USE DATABASE app;")
            .unwrap();
        assert_eq!(
            (value(&mut session, "GET cache:'hits';"), refusals),
            (integer(WRITERS * EACH), 0),
            "{}: every increment landed and none was refused",
            backend.name
        );
    });
}
