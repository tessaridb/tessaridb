//! `DEFINE VAULT` and the words that go with it, through the language.
//!
//! # What these tests are for, and what `sealing.rs` in the storage crate covers
//!
//! The storage tests assert against **bytes**: that a planted plaintext is
//! nowhere in the backend, that a ciphertext moved between records does not
//! open. They cover the mechanism. These cover the **surface** — that the words
//! parse, that they mean what the design says they mean, and that the four
//! refusals refuse.
//!
//! The two halves need each other. A surface test alone would pass against an
//! implementation that stored everything in the clear and merely printed the
//! right words; a byte test alone would pass against an implementation nobody
//! can reach.
//!
//! # The field is called `token` and not `password`, and that used to be a gap
//!
//! `PASSWORD` is a reserved word — `DEFINE USER ada PASSWORD '…'` — so `DEFINE
//! FIELD password ON team TYPE string SECRET` does not parse, while `CREATE
//! team:'github' SET password = '…'` does, because an object literal's field name
//! accepts a keyword and a declaration's did not. A vault is the one place
//! somebody will certainly try to declare a field called `password`, and the
//! design document's own example does exactly that.
//!
//! Until W131 the quoted form did not work either, so there was **no way to say
//! it at all** — and since a vault is strict, a field nobody can declare is a
//! field a vault cannot hold. `DEFINE FIELD 'password' …` now works, and
//! `a_field_named_password_can_live_in_a_vault` covers it end to end. The bare
//! keyword stays refused and stays **Q-417**, because accepting one there widens
//! every field declaration in the language. The rest of the tests keep `token`,
//! so that they test the vault rather than the lexer.
//!
//! # The refusals, and which of them protect anything
//!
//! Only one does. `DEFINE INDEX` over a secret field is a real control: an index
//! is a searchable copy of the plaintext, and a caller who cannot read the field
//! could still ask, a term at a time, whether any record holds a given value.
//!
//! `SELECT` over a vault protects nothing, because what it would return is
//! ciphertext — the sealed envelope *is* the stored value. It is refused so the
//! language means something: opaque bytes teach a caller that the vault is
//! broken, and a refusal naming `REVEAL` teaches them the feature. The
//! distinction is written here because a later reader deciding whether to relax
//! one of these needs to know which is which.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

const PLANTED: &str = "correct-horse-battery-staple-9f2b";

const TENANCY: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE work; USE DATABASE work;
";

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// A session with tenancy, an unsealed store and a vault holding one secret.
fn holding(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(&format!(
            "{TENANCY}
             UNSEAL VAULT WITH 'an operator passphrase';
             DEFINE VAULT team;
             DEFINE FIELD login    ON team TYPE string;
             DEFINE FIELD token ON team TYPE string SECRET;"
        ))
        .unwrap();
    session
}

fn refusal(session: &mut Session<'_>, statement: &str) -> String {
    session
        .run(statement)
        .expect_err("the statement was accepted")
        .to_string()
}

fn value(session: &mut Session<'_>, statement: &str) -> Value {
    match session.run(statement).unwrap().pop().unwrap() {
        Outcome::Value(value) => value,
        other => panic!("expected a value, got {other:?}"),
    }
}

fn write_one(session: &mut Session<'_>) {
    session
        .run(&format!(
            "CREATE team:'github' = {{ login: 'boog', token: '{PLANTED}' }};"
        ))
        .unwrap();
}

#[test]
fn a_vault_is_declared_written_and_revealed() {
    let store = store();
    let mut session = holding(&store);
    write_one(&mut session);

    let opened = value(&mut session, "REVEAL token FROM team:'github';");
    let Value::Object(fields) = opened else {
        panic!("REVEAL answers with an object")
    };
    assert_eq!(
        fields.get("token"),
        Some(&Value::String(PLANTED.to_owned()))
    );
    // One field asked for, one field answered. A `REVEAL` that quietly returned
    // the whole record would be a `SELECT` that had learned a new word.
    assert_eq!(fields.len(), 1);
}

#[test]
fn a_star_reveals_every_secret_and_no_ordinary_field() {
    let store = store();
    let mut session = holding(&store);
    write_one(&mut session);

    let Value::Object(fields) = value(&mut session, "REVEAL * FROM team:'github';") else {
        panic!("REVEAL answers with an object")
    };
    assert_eq!(
        fields.get("token"),
        Some(&Value::String(PLANTED.to_owned()))
    );
    // `login` is declared and is not secret. `*` means every *secret* field,
    // because a verb that answers with plaintext must not mix in values that
    // were never sealed — a caller reading the answer cannot tell them apart.
    assert!(!fields.contains_key("login"), "{fields:?}");
}

#[test]
fn select_over_a_vault_is_refused_and_names_reveal() {
    let store = store();
    let mut session = holding(&store);
    write_one(&mut session);

    for statement in [
        "SELECT * FROM team;",
        "SELECT * FROM team WHERE login = 'boog';",
        "SELECT * FROM team:'github';",
        // The ordering and the filter the design names separately. Both are
        // refused here by being reads of a vault at all, which is the stronger
        // form of the same refusal: there is no `SELECT` shape over a vault for
        // an `ORDER BY` to hang off.
        "SELECT * FROM team ORDER BY token;",
        "SELECT * FROM team WHERE token = 'guess';",
    ] {
        let said = refusal(&mut session, statement);
        assert!(said.contains("REVEAL"), "{statement}: {said}");
        assert!(
            !said.contains(PLANTED),
            "{statement} quoted a secret: {said}"
        );
    }
}

#[test]
fn an_index_over_a_secret_field_is_refused() {
    let store = store();
    let mut session = holding(&store);

    let said = refusal(&mut session, "DEFINE INDEX by_token ON team FIELDS token;");
    assert!(said.contains("cannot be indexed"), "{said}");

    // A projection of a secret is a secret somebody derived, so the root of the
    // path is what decides rather than the whole path.
    let said = refusal(
        &mut session,
        "DEFINE INDEX by_part ON team FIELDS token.inner;",
    );
    assert!(said.contains("cannot be indexed"), "{said}");

    // The control: an index over a field that is not secret is fine, so the
    // refusal above is about the marker and not about vaults refusing indexes.
    session
        .run("DEFINE INDEX by_login ON team FIELDS login;")
        .unwrap();
}

#[test]
fn secret_needs_a_vault() {
    let store = store();
    let mut session = holding(&store);
    session.run("DEFINE TABLE staff SCHEMALESS;").unwrap();

    let said = refusal(
        &mut session,
        "DEFINE FIELD token ON staff TYPE string SECRET;",
    );
    assert!(said.contains("needs a vault"), "{said}");
}

#[test]
fn secret_is_refused_in_a_columnar_table_and_in_an_alter() {
    let store = store();
    let mut session = holding(&store);
    session.run("DEFINE TABLE staff SCHEMALESS;").unwrap();

    // Both are grammar refusals rather than catalog ones, and both matter for
    // the same reason: a `SECRET` that parsed and was dropped would leave a
    // field the author believes is sealed.
    let said = refusal(&mut session, "DEFINE TABLE payroll (pay string SECRET);");
    assert!(said.contains("SECRET"), "{said}");

    let said = refusal(
        &mut session,
        "ALTER TABLE team ALTER FIELD token TYPE string SECRET;",
    );
    assert!(said.contains("SECRET"), "{said}");
}

#[test]
fn revealing_a_field_that_is_not_secret_is_refused() {
    let store = store();
    let mut session = holding(&store);
    write_one(&mut session);

    let said = refusal(&mut session, "REVEAL login FROM team:'github';");
    assert!(said.contains("not a secret field"), "{said}");
    assert!(
        !said.contains("boog"),
        "the refusal quoted the value: {said}"
    );
}

#[test]
fn a_sealed_store_refuses_the_write_and_the_reveal() {
    let store = store();
    let mut session = holding(&store);
    write_one(&mut session);
    session.run("SEAL VAULT;").unwrap();

    let said = refusal(
        &mut session,
        &format!("CREATE team:'second' = {{ token: '{PLANTED}' }};"),
    );
    assert!(
        !said.contains(PLANTED),
        "the refusal quoted the value: {said}"
    );

    let said = refusal(&mut session, "REVEAL token FROM team:'github';");
    assert!(!said.contains(PLANTED), "{said}");

    // And unsealing again restores it, so sealing is a state rather than a
    // one-way door.
    session
        .run("UNSEAL VAULT WITH 'an operator passphrase';")
        .unwrap();
    let Value::Object(fields) = value(&mut session, "REVEAL token FROM team:'github';") else {
        panic!("REVEAL answers with an object")
    };
    assert_eq!(
        fields.get("token"),
        Some(&Value::String(PLANTED.to_owned()))
    );
}

#[test]
fn a_wrong_passphrase_is_refused_and_does_not_unseal() {
    let store = store();
    let mut session = holding(&store);
    write_one(&mut session);
    session.run("SEAL VAULT;").unwrap();

    let said = refusal(&mut session, "UNSEAL VAULT WITH 'not the passphrase';");
    assert!(
        !said.contains("not the passphrase"),
        "the refusal quoted the passphrase: {said}"
    );
    // Still sealed. A failed unseal that left the keyring half-open would be
    // worse than one that refused loudly.
    refusal(&mut session, "REVEAL token FROM team:'github';");
}

#[test]
fn the_first_unseal_says_it_initialised_and_the_second_says_it_unsealed() {
    let store = store();
    let mut session = Session::new(&store);
    session.run(TENANCY).unwrap();

    // The distinction that makes a mistyped first passphrase survivable: there
    // is no path that replaces a root once written, so an operator who expected
    // `unsealed` and reads `initialised` has to be told at once.
    assert_eq!(
        value(&mut session, "UNSEAL VAULT WITH 'first';"),
        Value::from("initialised")
    );
    session.run("SEAL VAULT;").unwrap();
    assert_eq!(
        value(&mut session, "UNSEAL VAULT WITH 'first';"),
        Value::from("unsealed")
    );
}

#[test]
fn info_for_vault_reports_which_fields_are_sealed_and_nothing_about_them() {
    let store = store();
    let mut session = holding(&store);
    write_one(&mut session);

    let Value::Object(report) = value(&mut session, "INFO FOR VAULT team;") else {
        panic!("INFO answers with an object")
    };
    let Some(Value::Object(fields)) = report.get("fields") else {
        panic!("the report carries its fields")
    };
    let Some(Value::Object(token)) = fields.get("token") else {
        panic!("the secret field is reported")
    };
    assert_eq!(token.get("secret"), Some(&Value::Bool(true)));

    let Some(Value::Object(login)) = fields.get("login") else {
        panic!("the ordinary field is reported")
    };
    assert_eq!(login.get("secret"), Some(&Value::Bool(false)));

    // No length, no fingerprint, no key identifier — each of which would be an
    // oracle that answers more slowly rather than not at all.
    let rendered = format!("{report:?}");
    assert!(!rendered.contains(PLANTED), "{rendered}");
    assert!(!rendered.to_lowercase().contains("key_id"), "{rendered}");
    assert!(!rendered.to_lowercase().contains("length"), "{rendered}");
}

#[test]
fn dropping_a_vault_makes_its_records_unopenable() {
    let store = store();
    let mut session = holding(&store);
    write_one(&mut session);
    session.run("DROP VAULT team;").unwrap();

    // The vault is gone as a name, which is what a drop of anything does. What
    // makes this one a crypto-shred is that the key went with it: re-declaring
    // the name mints a **new** key, so nothing written under the old one is
    // reachable from here or from any restored copy.
    let said = refusal(&mut session, "REVEAL token FROM team:'github';");
    assert!(!said.contains(PLANTED), "{said}");

    session.run("DEFINE VAULT team;").unwrap();
    session
        .run("DEFINE FIELD token ON team TYPE string SECRET;")
        .unwrap();
    // Absent, not opened, and `none` rather than an empty object: the record
    // went with the table, and the re-declared vault holds a **new** key, so
    // there is nothing here that could open the old bytes even if a restore put
    // them back. That is the crypto-shred, stated as the only thing a test in
    // this process can actually observe about it.
    assert_eq!(
        value(&mut session, "REVEAL token FROM team:'github';"),
        Value::None
    );
}

#[test]
fn define_vault_needs_an_unsealed_store() {
    let store = store();
    let mut session = Session::new(&store);
    session.run(TENANCY).unwrap();

    // Before any `UNSEAL` there is no master key, so there is nothing to wrap
    // the vault's own key under. Refused rather than deferred: a vault created
    // with the key left for later would refuse every write while `INFO`
    // reported it ready.
    refusal(&mut session, "DEFINE VAULT team;");
}

/// A vault is strict, and cannot be talked out of it.
///
/// Criterion W2 says strictness is not optional here, and the reason is one
/// sentence: the marker that seals a field is `SECRET` on its declaration, so a
/// field nobody declared is a field nothing seals. A schemaless vault writes it
/// in the clear beside the sealed ones.
///
/// Two halves, because the default alone is one statement deep. The declaration
/// refuses the undeclared field; the `ALTER` refuses to remove that refusal.
#[test]
fn a_vault_is_strict_and_cannot_be_made_schemaless() {
    let store = store();
    let mut session = holding(&store);

    // Declared fields are accepted, so the refusal below is about the field and
    // not about vaults refusing writes.
    write_one(&mut session);

    let said = refusal(
        &mut session,
        "CREATE team:'gitlab' = { login: 'boog', recovery: 'not-declared' };",
    );
    assert!(said.contains("recovery"), "{said}");

    let said = refusal(&mut session, "ALTER TABLE team SET SCHEMALESS;");
    assert!(said.contains("cannot be made schemaless"), "{said}");

    // And the refusal held: the field is still refused afterwards, which is what
    // distinguishes a refused `ALTER` from one that errored after taking effect.
    let said = refusal(
        &mut session,
        "CREATE team:'gitlab' = { login: 'boog', recovery: 'not-declared' };",
    );
    assert!(said.contains("recovery"), "{said}");

    // The control: an ordinary table is still free to be schemaless, so the
    // refusal is about vaults and not about `ALTER` losing the ability.
    session.run("DEFINE TABLE staff SCHEMAFULL;").unwrap();
    session.run("ALTER TABLE staff SET SCHEMALESS;").unwrap();
}

/// The filter and the ordering the design names as oracles have no shape on a
/// `REVEAL` to hang off.
///
/// `select_over_a_vault_is_refused_and_names_reveal` covers them on a `SELECT`,
/// where they are refused. On the statement that *does* return plaintext they
/// are refused earlier and harder: `REVEAL` takes a record target and nothing
/// else, so `WHERE` and `ORDER BY` do not parse.
///
/// Asserted rather than left to the grammar, because the grammar is where
/// somebody would add them: a `WHERE` on `REVEAL` reads like a convenience and
/// is an oracle that answers one bit of a secret at a time.
#[test]
fn reveal_has_no_filter_and_no_ordering_to_hang_an_oracle_on() {
    let store = store();
    let mut session = holding(&store);
    write_one(&mut session);

    for statement in [
        "REVEAL token FROM team WHERE token = 'guess';",
        "REVEAL token FROM team ORDER BY token;",
        "REVEAL * FROM team:'github' WHERE login = 'boog';",
    ] {
        let said = refusal(&mut session, statement);
        assert!(
            !said.contains(PLANTED),
            "{statement} quoted a secret: {said}"
        );
    }

    // The control: the form without the clause is accepted, so the refusals
    // above are about the clause and not about the statement.
    session.run("REVEAL token FROM team:'github';").unwrap();
}

/// Criterion W1's `INFO` clause, in the shape the collection precedent set:
/// the report carries the flag **and** the rendered declaration carries the
/// word.
///
/// Both halves, because either alone leaves the hole. A report without the flag
/// describes a vault and an ordinary table identically — and one of the two
/// refuses `SELECT`, seals its `SECRET` fields and cannot be made schemaless. A
/// flag that never reaches the text leaves a declaration which, re-executed,
/// restores a plain table: the `SECRET` fields would then be refused outright,
/// because there would be no key to seal them with.
#[test]
fn a_vault_is_declared_back_as_a_vault_and_not_as_a_strict_table() {
    let store = store();
    let mut session = holding(&store);

    let Value::Object(report) = value(&mut session, "INFO FOR TABLE team;") else {
        panic!("INFO answers with an object")
    };
    assert_eq!(report.get("vault"), Some(&Value::Bool(true)));
    // The control: the marker distinguishes, rather than being true of tables.
    session.run("DEFINE TABLE staff SCHEMAFULL;").unwrap();
    let Value::Object(ordinary) = value(&mut session, "INFO FOR TABLE staff;") else {
        panic!("INFO answers with an object")
    };
    assert_eq!(ordinary.get("vault"), Some(&Value::Bool(false)));

    let Some(Value::String(script)) = report.get("definition") else {
        panic!("the report carries a definition")
    };
    assert!(
        script.contains("DEFINE VAULT team"),
        "a vault was declared back as something else:\n{script}"
    );

    // And it re-reads as what it claims to be: run in a fresh store it produces
    // a vault, not a table that would refuse the first `SECRET` field.
    let second = store_second();
    let mut fresh = Session::new(&second);
    fresh
        .run(&format!(
            "{TENANCY} UNSEAL VAULT WITH 'another passphrase'; {script}"
        ))
        .unwrap();
    // The restored store answers `INFO FOR VAULT` — a statement an ordinary
    // table has no answer to — and reports the sealed field as sealed. Against a
    // script that had said `DEFINE TABLE`, the `SECRET` field in it would have
    // been refused for want of a key and this would never have been reached.
    let Value::Object(restored) = value(&mut fresh, "INFO FOR VAULT team;") else {
        panic!("INFO answers with an object")
    };
    let Some(Value::Object(fields)) = restored.get("fields") else {
        panic!("the report carries its fields")
    };
    let Some(Value::Object(token)) = fields.get("token") else {
        panic!("the secret field came back")
    };
    assert_eq!(token.get("secret"), Some(&Value::Bool(true)));
}

/// A second store, named through a function because `store` is shadowed by the
/// local binding wherever this is used.
fn store_second() -> Store {
    store()
}

/// Survey row 15 — the planner, and what `EXPLAIN` says about a vault.
///
/// `EXPLAIN` takes a `SELECT`, and a `SELECT` over a vault is refused, so the
/// honest expectation is that there is no plan to print. Asserted rather than
/// assumed, because the refusal has to come from the vault gate and not from
/// `EXPLAIN` failing for some unrelated reason — and because a plan is a
/// description of which keys a read would probe, which is exactly the shape of
/// thing that leaks without returning a value.
///
/// The control is an ordinary table beside it: `EXPLAIN` does produce a plan
/// when there is one to produce, so the refusal below is about vaults.
#[test]
fn a_vault_has_no_plan_to_explain_and_the_refusal_carries_nothing() {
    let store = store();
    let mut session = holding(&store);
    write_one(&mut session);
    session.run("DEFINE TABLE notes SCHEMALESS;").unwrap();
    session
        .run("CREATE notes:1 = { text: 'ordinary' };")
        .unwrap();

    // The control: a plan exists for a table that can be read.
    let planned = format!("{:?}", value(&mut session, "EXPLAIN SELECT * FROM notes;"));
    assert!(
        planned.contains("scan") || planned.contains("notes"),
        "`EXPLAIN` produced no plan for an ordinary table: {planned}"
    );

    for statement in [
        "EXPLAIN SELECT * FROM team;",
        "EXPLAIN SELECT * FROM team:'github';",
        "EXPLAIN SELECT token FROM team;",
        "EXPLAIN SELECT * FROM team WHERE token = 'guess';",
    ] {
        let said = refusal(&mut session, statement);
        // It is refused by the vault gate — the same refusal a bare `SELECT`
        // gets — rather than by `EXPLAIN` tripping over something else.
        assert!(said.contains("REVEAL"), "{statement}: {said}");
        assert!(
            !said.contains(PLANTED),
            "{statement} quoted a secret: {said}"
        );
    }
}

/// A field called `password`, which is what a secret store will be asked for.
///
/// The design document's flagship example is `DEFINE FIELD password ON team TYPE
/// string SECRET`, and until W131 no spelling of it worked: the bare word is a
/// keyword, and the quoted form — which an object literal accepts — was refused
/// in a declaration too. Since a vault is strict, that made the example not
/// merely awkward but **impossible**: a field nobody can declare is a field a
/// vault cannot hold.
///
/// This walks the whole life of such a field rather than asserting the parse,
/// because a name that parses and then cannot be written to, opened or dropped
/// has only moved the problem.
#[test]
fn a_field_named_password_can_live_in_a_vault() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(&format!(
            "{TENANCY}
             UNSEAL VAULT WITH 'an operator passphrase';
             DEFINE VAULT team;
             DEFINE FIELD login ON team TYPE string;
             DEFINE FIELD 'password' ON team TYPE string SECRET;
             CREATE team:'github' = {{ login: 'boog', password: '{PLANTED}' }};"
        ))
        .expect("a vault could not hold a field called password");

    // Written by the bare keyword in an object literal and opened by the quoted
    // name: the two halves of the language now agree about what the field is
    // called, which is the whole of the defect.
    let opened = value(&mut session, "REVEAL 'password' FROM team:'github';");
    let Value::Object(fields) = opened else {
        panic!("REVEAL answers with an object")
    };
    assert_eq!(
        fields.get("password"),
        Some(&Value::String(PLANTED.to_owned())),
        "the field did not come back under the name it was declared with"
    );

    session
        .run("ALTER TABLE team DROP FIELD 'password';")
        .expect("a field that can be declared must be droppable");
}

/// The bare keyword is still refused, and a statement missing its name still
/// says so.
///
/// Both halves matter. The first keeps Q-417 open rather than quietly answered:
/// accepting a bare keyword here would widen every field declaration in the
/// language, which is a decision this change deliberately does not take. The
/// second is the property that makes the change safe — a quoted name introduces
/// no ambiguity precisely because a *missing* name is not a string, so no
/// malformed statement starts parsing as a valid one.
#[test]
fn a_bare_keyword_is_still_not_a_field_name_and_a_missing_name_still_says_so() {
    let store = store();
    let mut session = holding(&store);

    let bare = refusal(
        &mut session,
        "DEFINE FIELD password ON team TYPE string SECRET;",
    );
    assert!(
        bare.contains("a name"),
        "the bare keyword was accepted or refused for another reason: {bare}"
    );

    let missing = refusal(&mut session, "ALTER TABLE team ADD FIELD TYPE string;");
    assert!(
        missing.contains("a name"),
        "a field declaration with no name did not report a missing name: {missing}"
    );
}

/// Rotating a secret, and the two ways an edit could have been spelled.
///
/// # What was wrong, and why nothing caught it
///
/// A record read back for an edit carries `#keys` — the map of data keys
/// wrapped once per recipient — and the *ciphertext* of every sealed field.
/// `SET` and `MERGE` compute from that record, so they carried both into the
/// write: sealing refused `#keys` by name, and a vault holding a second secret
/// refused earlier still, with a schema violation saying a `string` field held
/// bytes.
///
/// Both messages were about the write and neither was about the cause, and the
/// cause is not routable-around: computing from a sealed field means opening
/// it, opening one is `REVEAL`, and `REVEAL` records itself before it answers.
/// So the refusal now says what the language actually offers.
///
/// Every existing test wrote with `CREATE` or replaced records whole, which is
/// why a suite of twenty vault tests was green over a store where a secret
/// could not be rotated by the verb an operator reaches for first.
#[test]
fn an_edit_that_computes_from_a_vault_record_says_what_to_write_instead() {
    let store = store();
    let mut session = holding(&store);
    session
        .run(&format!(
            "CREATE team:'github' = {{ login: 'ada', token: '{PLANTED}' }};"
        ))
        .unwrap();

    for statement in [
        "UPDATE team:'github' SET token = 'rotated';",
        "UPDATE team:'github' MERGE { token: 'rotated' };",
    ] {
        let refused = refusal(&mut session, statement);
        // Pinned to the words only this refusal produces. The two it replaced —
        // one naming `#keys`, one naming a type mismatch — would both satisfy a
        // check that only asserted an error came back.
        assert!(
            refused.contains("written whole") && refused.contains("UPDATE"),
            "{statement} was refused for another reason: {refused}",
        );
        assert!(
            !refused.contains("#keys"),
            "the refusal still names the store's own field: {refused}",
        );
    }

    // And the form it points at works, which is what makes the message worth
    // trusting rather than a dead end.
    session
        .run("UPDATE team:'github' = { login: 'ada', token: 'rotated' };")
        .unwrap();
    let opened = format!(
        "{:?}",
        value(&mut session, "REVEAL token FROM team:'github';")
    );
    assert!(opened.contains("rotated"), "{opened}");
    assert!(
        !opened.contains(PLANTED),
        "the rotation left the old secret readable: {opened}",
    );
}

/// The same refusal does not reach a table that is not a vault.
///
/// The check keys on the record carrying the store's key map, which only a
/// vault's does — so this is the control that says the condition is the vault
/// rather than the verb.
#[test]
fn an_ordinary_table_is_still_edited_field_by_field() {
    let store = store();
    let mut session = holding(&store);
    session
        .run("DEFINE TABLE notes SCHEMALESS; CREATE notes:1 = { body: 'first' };")
        .unwrap();
    session.run("UPDATE notes:1 SET body = 'second';").unwrap();
    let read = format!("{:?}", session.run("SELECT body FROM notes:1;").unwrap());
    assert!(read.contains("second"), "{read}");
}
