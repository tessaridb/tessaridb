use super::*;

#[test]
fn a_peer_holding_part_of_the_table_is_given_another_shard_page_by_page() {
    let authority = Authority::new();
    let (db, table) = leader();
    // A budget of one byte: every page carries exactly one record.
    let (address, handle) = door(&authority, &db, Some(shard(table, 2)), 1, 3);
    let mut gather = asking(table, 1);
    let mut received: Vec<RecordId> = Vec::new();
    let mut pages = 0;
    loop {
        let page = ask(&authority, address, &gather).unwrap();
        pages += 1;
        received.extend(page.records.iter().map(|(id, _)| id.clone()));
        if !page.more {
            break;
        }
        gather.after = received.last().cloned();
    }
    // Asserted before the door is joined: a page that ignored its budget
    // ends the conversation early, and joining first would wait forever on
    // connections that never come instead of failing here.
    assert_eq!(
        received,
        ["a", "b", "c"].map(RecordId::from).to_vec(),
        "shard 1 whole, and not shard 2's 'h'"
    );
    assert_eq!(
        pages, 3,
        "one record a page, the last saying no more follow"
    );
    handle.join().unwrap();
}

#[test]
fn an_ordered_ask_is_answered_with_the_shards_first_records_page_by_page() {
    // ADR-0102: shard 1 holds a (1), b (2) and c (3); its first two by `n`
    // descending are c and b, sent in identity order. A budget of one byte
    // puts each on its own page, so the second page is ranked again and
    // begins past the first.
    let authority = Authority::new();
    let (db, table) = leader();
    let (address, handle) = door(&authority, &db, Some(shard(table, 2)), 1, 2);
    let mut gather = Gather {
        ordered: Some(super::super::tests::by_n(true, 2)),
        ..asking(table, 1)
    };
    let first = ask(&authority, address, &gather).unwrap();
    gather.after = first.records.last().map(|(id, _)| id.clone());
    let second = ask(&authority, address, &gather).unwrap();
    handle.join().unwrap();
    let ids =
        |page: &Page| -> Vec<RecordId> { page.records.iter().map(|(id, _)| id.clone()).collect() };
    assert_eq!(ids(&first), [RecordId::from("b")]);
    assert!(first.more);
    assert_eq!(ids(&second), [RecordId::from("c")]);
    assert!(!second.more);
}

#[test]
fn a_window_is_answered_inside_the_shard_and_never_past_it() {
    let authority = Authority::new();
    let (db, table) = leader();
    let (address, handle) = door(&authority, &db, Some(shard(table, 2)), 1 << 20, 1);
    let gather = Gather {
        from: Some(RecordId::from("b")),
        to: Some((RecordId::from("z"), true)),
        ..asking(table, 1)
    };
    let page = ask(&authority, address, &gather).unwrap();
    handle.join().unwrap();
    let ids: Vec<RecordId> = page.records.into_iter().map(|(id, _)| id).collect();
    assert_eq!(ids, ["b", "c"].map(RecordId::from).to_vec());
    assert!(!page.more);
}

#[test]
fn a_peer_holding_nothing_of_the_table_is_refused_by_name() {
    let authority = Authority::new();
    let (db, table) = leader();
    let (address, handle) = door(
        &authority,
        &db,
        Some(Reach::Namespace(NamespaceId::new(9))),
        1 << 20,
        1,
    );
    let refused = ask(&authority, address, &asking(table, 1));
    handle.join().unwrap();
    assert!(
        matches!(refused, Err(Error::NotGathered(Ungathered::NotEntitled))),
        "{refused:?}"
    );
}

#[test]
fn a_holder_lacking_the_shard_or_the_table_says_which() {
    let authority = Authority::new();
    let (db, table) = leader();
    db.store().record_served(shard(table, 2)).unwrap();
    let (address, handle) = door(&authority, &db, Some(Reach::Store), 1 << 20, 2);
    let not_held = ask(&authority, address, &asking(table, 1));
    let no_table = ask(&authority, address, &asking(TableId::new(999), 1));
    handle.join().unwrap();
    assert!(
        matches!(not_held, Err(Error::NotGathered(Ungathered::NotHeld))),
        "{not_held:?}"
    );
    assert!(
        matches!(no_table, Err(Error::NotGathered(Ungathered::NoSuchTable))),
        "{no_table:?}"
    );
}

#[test]
fn a_shard_the_answerers_map_has_moved_past_is_refused_as_moved() {
    // ADR-0095 D4: an asker holding an older map names a shard the answerer
    // has retired. *No such table* would send it looking for a table; the
    // repair is to read the map again, so the refusal says the map moved.
    let authority = Authority::new();
    let (db, table) = leader();
    db.session()
        .run("USE NAMESPACE prod; USE DATABASE shop; ALTER TABLE ledger SPLIT AT 'c';")
        .unwrap();
    let (address, handle) = door(&authority, &db, Some(Reach::Store), 1 << 20, 1);
    let retired = ask(&authority, address, &asking(table, 1));
    handle.join().unwrap();
    assert!(
        matches!(retired, Err(Error::NotGathered(Ungathered::MapMoved))),
        "{retired:?}"
    );
}
