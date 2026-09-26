//! Who may append and read: an anonymous caller at a `PUBLIC` topic, and
//! grants (S5.1, S5.2).

use tessari_session::Session;
use tessari_storage::Store;

use super::super::key_value::{on_each_backend, refused, run};
use super::{opened, read};

const PASSWORD: &str = "correct horse battery";

/// `prod`/`app` holding the topic `events`, a public topic `inbox`, a
/// collection `secrets` — and an owner, so the store is closed.
fn closed(store: &Store) -> Session<'_> {
    let mut owner = opened(store);
    run(
        &mut owner,
        &format!(
            "DEFINE TOPIC inbox MAX BYTES 256 PUBLIC RATE 3 PER 1h;\n\
             DEFINE COLLECTION secrets; CREATE secrets:1 = {{ pay: 100 }};\n\
             DEFINE USER root ROLE owner PASSWORD '{PASSWORD}';"
        ),
    );
    owner.sign_in("root", PASSWORD).unwrap();
    owner
}

fn anonymous(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run("USE NAMESPACE prod; USE DATABASE app;")
        .unwrap();
    session
}

#[test]
fn an_anonymous_caller_may_append_to_a_public_topic_and_do_nothing_else() {
    on_each_backend(|backend| {
        let mut owner = closed(&backend.store);
        let mut stranger = anonymous(&backend.store);
        let signed_in = "this store requires a signed-in user";
        for script in [
            "READ FROM inbox;",
            "SELECT * FROM inbox;",
            "INFO FOR TOPIC inbox;",
            "CREATE events = { n: 1 };",
            "CREATE secrets = { n: 1 };",
            "SELECT * FROM secrets;",
            // A read inside the value would come back in the append's reply.
            "CREATE inbox = { copy: (SELECT * FROM secrets) };",
            "CREATE inbox = { copy: (SELECT * FROM inbox) };",
            "INSERT INTO inbox (copy) VALUES ((SELECT * FROM secrets));",
            // A taken id's refusal would say which messages exist.
            "CREATE inbox:'named' = { n: 1 };",
            "DEFINE TOPIC mine PUBLIC RATE 1 PER 1s MAX BYTES 10;",
            "BEGIN;",
        ] {
            let why = refused(&mut stranger, script);
            assert!(why.contains(signed_in), "{} {script}: {why}", backend.name);
        }
        run(&mut stranger, "CREATE inbox = { n: 1 };");
        let why = refused(
            &mut stranger,
            &format!("CREATE inbox = {{ text: '{}' }};", "x".repeat(300)),
        );
        assert!(
            why.contains("topic inbox takes messages of at most 256 bytes"),
            "{}: {why}",
            backend.name
        );
        run(&mut stranger, "INSERT INTO inbox (n) VALUES (2);");
        // The size refusal took from the allowance too: it passed the door and
        // failed at the commit, so this is the fourth message of three.
        let why = refused(&mut stranger, "CREATE inbox = { n: 3 };");
        assert!(
            why.contains("topic inbox takes at most 3 anonymous messages per 1h on this node"),
            "{}: {why}",
            backend.name
        );
        let (messages, _) = read(&mut owner, "READ FROM inbox;");
        let positions: Vec<u64> = messages.iter().map(|(at, _)| *at).collect();
        assert_eq!(positions, vec![1, 2], "{}", backend.name);
        // A signed-in caller is not rated.
        for _ in 0..5 {
            run(&mut owner, "CREATE inbox = { n: 9 };");
        }
    });
}

#[test]
fn an_insert_is_charged_per_message_and_one_larger_than_the_rate_is_refused() {
    on_each_backend(|backend| {
        let _owner = closed(&backend.store);
        let mut stranger = anonymous(&backend.store);
        let why = refused(
            &mut stranger,
            "INSERT INTO inbox (n) VALUES (1), (2), (3), (4);",
        );
        assert!(
            why.contains("anonymous messages per 1h"),
            "{}: {why}",
            backend.name
        );
        run(&mut stranger, "INSERT INTO inbox (n) VALUES (1), (2), (3);");
        let why = refused(&mut stranger, "CREATE inbox = { n: 4 };");
        assert!(
            why.contains("anonymous messages per 1h"),
            "{}: {why}",
            backend.name
        );
    });
}

#[test]
fn read_on_a_topic_reads_and_keeps_a_position_and_write_appends() {
    on_each_backend(|backend| {
        let mut owner = closed(&backend.store);
        run(
            &mut owner,
            &format!(
                "USE NAMESPACE prod; USE DATABASE app;\n\
                 DEFINE USER ria ON prod.app ROLE editor PASSWORD '{PASSWORD}';\n\
                 DEFINE USER wes ON prod.app ROLE editor PASSWORD '{PASSWORD}';\n\
                 GRANT read ON events TO ria; GRANT write ON events TO wes;\n\
                 CREATE events = {{ n: 1 }};"
            ),
        );
        let mut ria = Session::new(&backend.store);
        ria.sign_in("ria", PASSWORD).unwrap();
        run(&mut ria, "USE NAMESPACE prod; USE DATABASE app;");
        let (messages, _) = read(&mut ria, "READ FROM events FOR CONSUMER 'audit';");
        assert_eq!(messages.len(), 1, "{}", backend.name);
        let (again, _) = read(&mut ria, "READ FROM events FOR CONSUMER 'audit';");
        assert!(
            again.is_empty(),
            "{}: the position was not kept",
            backend.name
        );
        let why = refused(&mut ria, "CREATE events = { n: 2 };");
        assert!(
            why.contains("\"ria\" has not been granted write on \"events\""),
            "{}: {why}",
            backend.name
        );

        let mut wes = Session::new(&backend.store);
        wes.sign_in("wes", PASSWORD).unwrap();
        run(&mut wes, "USE NAMESPACE prod; USE DATABASE app;");
        run(&mut wes, "CREATE events = { n: 2 };");
        let why = refused(&mut wes, "READ FROM events;");
        assert!(
            why.contains("\"wes\" has not been granted read on \"events\""),
            "{}: {why}",
            backend.name
        );
        let (messages, _) = read(&mut ria, "READ FROM events FOR CONSUMER 'audit';");
        assert_eq!(
            messages.iter().map(|(at, _)| *at).collect::<Vec<_>>(),
            vec![2]
        );
    });
}
