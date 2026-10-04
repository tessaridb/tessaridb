use super::*;

fn ids(page: &Page) -> Vec<RecordId> {
    page.records.iter().map(|(id, _)| id.clone()).collect()
}

#[test]
fn a_pushed_condition_narrows_the_page_before_it_travels() {
    let authority = Authority::new();
    let (db, table) = leader();
    let (address, handle) = door(&authority, &db, Some(Reach::Store), 1 << 20, 1);
    let page = ask(
        &authority,
        address,
        &Gather {
            pushed: Some(narrowed("(n > $p0)", 1, None)),
            ..asking(table, 1)
        },
    )
    .unwrap();
    handle.join().unwrap();
    assert_eq!(ids(&page), ["b", "c"].map(RecordId::from).to_vec());
    assert!(!page.more);
}

#[test]
fn a_field_the_asker_cannot_see_is_not_searchable_through_the_leader() {
    // ADR-0097 D1: the asker's grant shows it `other` and not `n`, so `n` is
    // absent to the condition exactly as it is to the asker's own read —
    // and the leader keeps nothing rather than revealing which records
    // carry an `n` above 1.
    let authority = Authority::new();
    let (db, table) = leader();
    let (address, handle) = door(&authority, &db, Some(Reach::Store), 1 << 20, 1);
    let page = ask(
        &authority,
        address,
        &Gather {
            pushed: Some(narrowed("(n > $p0)", 1, Some("other"))),
            ..asking(table, 1)
        },
    )
    .unwrap();
    handle.join().unwrap();
    assert_eq!(ids(&page), Vec::<RecordId>::new());
}

#[test]
fn narrowed_pages_still_end_where_the_budget_cuts_them() {
    let authority = Authority::new();
    let (db, table) = leader();
    let (address, handle) = door(&authority, &db, Some(Reach::Store), 1, 2);
    let mut gather = Gather {
        pushed: Some(narrowed("(n >= $p0)", 2, None)),
        ..asking(table, 1)
    };
    let first = ask(&authority, address, &gather).unwrap();
    assert_eq!(
        (ids(&first), first.more, first.resume.clone()),
        (vec![RecordId::from("b")], true, None)
    );
    gather.after = Some(RecordId::from("b"));
    let second = ask(&authority, address, &gather).unwrap();
    handle.join().unwrap();
    assert_eq!(
        (ids(&second), second.more),
        (vec![RecordId::from("c")], false)
    );
}

#[test]
fn a_page_that_keeps_nothing_it_read_says_where_it_got_to() {
    let authority = Authority::new();
    let db = Db::in_memory().unwrap();
    let mut script = String::from(
        "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop; \
             DEFINE TABLE ledger (n int) IDENTITY uuid SPLIT AT 'g';",
    );
    for n in 0..1100 {
        script.push_str(&format!(" CREATE ledger:'a{n:04}' = {{ n: {n} }};"));
    }
    db.session().run(&script).unwrap();
    let table = {
        let mut transaction = db.store().begin().unwrap();
        Catalog::new(&mut transaction)
            .table_id(NamespaceId::new(1), DatabaseId::new(1), "ledger")
            .unwrap()
            .unwrap()
    };
    let db = Arc::new(db);
    let (address, handle) = door(&authority, &db, Some(Reach::Store), 1 << 20, 2);
    let mut gather = Gather {
        pushed: Some(narrowed("(n >= $p0)", 1095, None)),
        ..asking(table, 1)
    };
    let first = ask(&authority, address, &gather).unwrap();
    assert!(first.records.is_empty() && first.more, "{:?}", ids(&first));
    assert_eq!(
        first.resume,
        Some(RecordId::from("a1023")),
        "resumes after the last record read"
    );
    gather.after = first.resume;
    let second = ask(&authority, address, &gather).unwrap();
    handle.join().unwrap();
    assert_eq!(ids(&second).len(), 5);
    assert!(!second.more);
}
