#![allow(clippy::unwrap_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::thread::JoinHandle;

use tessari_encoding::NODE_ID_LEN;
use tessari_storage::{Catalog, Reach};
use tessari_types::{DatabaseId, NamespaceId, RecordId, ShardId, TableId};
use tessaridb::Db;

use super::{Gather, Page, Ungathered};
use crate::collection::{Serving, Subscriptions};
use crate::error::{Error, Result};
use crate::grant::Deciding;
use crate::link::tests::{Authority, THERE, hello, settled};
use crate::link::{Answered, Ask};
use crate::peer::Purpose;

const LEADER: [u8; NODE_ID_LEN] = [71_u8; NODE_ID_LEN];

/// A catalog answer a test chooses.
#[derive(Debug)]
struct Granting(Option<Reach>);

impl Subscriptions for Granting {
    fn granted(&self, _follower: [u8; NODE_ID_LEN]) -> Result<Option<Reach>> {
        Ok(self.0)
    }
}

fn leader() -> (Arc<Db>, TableId) {
    let db = Db::in_memory().unwrap();
    db.session()
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; \
                 USE DATABASE shop; \
                 DEFINE TABLE ledger (n int) IDENTITY uuid SPLIT AT 'g'; \
                 CREATE ledger:'a' = { n: 1 }; CREATE ledger:'b' = { n: 2 }; \
                 CREATE ledger:'c' = { n: 3 }; CREATE ledger:'h' = { n: 4 };",
        )
        .unwrap();
    let table = {
        let mut transaction = db.store().begin().unwrap();
        Catalog::new(&mut transaction)
            .table_id(NamespaceId::new(1), DatabaseId::new(1), "ledger")
            .unwrap()
            .unwrap()
    };
    (Arc::new(db), table)
}

fn shard(table: TableId, id: u32) -> Reach {
    Reach::Shard(
        NamespaceId::new(1),
        DatabaseId::new(1),
        table,
        ShardId::new(id),
    )
}

/// A door for `LEADER` answering `rounds` connections out of `db`.
fn door(
    authority: &Authority,
    db: &Arc<Db>,
    granted: Option<Reach>,
    budget: usize,
    rounds: usize,
) -> (SocketAddr, JoinHandle<()>) {
    folding_door(
        authority,
        db,
        granted,
        (budget, tessari_constants::GATHER_FOLD_RECORDS),
        rounds,
    )
}

/// The same, folding at most `fold` records into a page of groups.
fn folding_door(
    authority: &Authority,
    db: &Arc<Db>,
    granted: Option<Reach>,
    (budget, fold): (usize, usize),
    rounds: usize,
) -> (SocketAddr, JoinHandle<()>) {
    let peers = crate::link::tests::bind_with(
        "127.0.0.1:0",
        authority.issue(LEADER, Purpose::Peer),
        &authority.der(),
    )
    .unwrap();
    let address = peers.address().unwrap();
    let mine = hello(LEADER);
    let db = Arc::clone(db);
    let handle = std::thread::spawn(move || {
        let granting = Granting(granted);
        for _ in 0..rounds {
            drop(peers.greet(
                || Ok(mine),
                &LEADER,
                &Deciding::holding(settled()),
                &Serving::within(db.store(), &granting, budget).folding_by(fold),
            ));
        }
    });
    (address, handle)
}

fn ask(authority: &Authority, address: SocketAddr, gather: &Gather) -> Result<Page> {
    match crate::link::tests::call_with(
        address,
        authority.issue(THERE, Purpose::Peer),
        &authority.der(),
        LEADER,
        &hello(THERE),
        Ask::Gather(gather),
    )?
    .1
    {
        Answered::Gathered(page) => Ok(page),
        other => panic!("answered {other:?}"),
    }
}

fn asking(table: TableId, shard: u32) -> Gather {
    Gather {
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(1),
        table,
        shard: ShardId::new(shard),
        from: None,
        to: None,
        after: None,
        pushed: None,
        enough: None,
        reduce: None,
        ordered: None,
        counting: None,
    }
}

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
        ordered: Some(super::tests::by_n(true, 2)),
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

fn narrowed(condition: &str, bound: i64, visible: Option<&str>) -> tessari_session::Pushed {
    tessari_session::Pushed {
        visible: visible.map(|field| [field.to_owned()].into()),
        condition: condition.to_owned(),
        parameters: [("p0".to_owned(), tessari_types::Value::from(bound))].into(),
    }
}

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

/// ADR-0097 D2: asked for folds, a leader sends groups and no record, a
/// page of records at a time, each page resuming where its read stopped.
#[test]
fn a_shard_is_folded_page_by_page_and_no_record_travels() {
    let authority = Authority::new();
    let (db, table) = leader();
    let (address, handle) = folding_door(&authority, &db, Some(shard(table, 2)), (1 << 20, 2), 2);
    let count_and_sum = || tessari_session::Reduce {
        visible: None,
        condition: None,
        keys: Vec::new(),
        folds: vec![
            tessari_session::Folded::named("count", None).unwrap(),
            tessari_session::Folded::named(
                "sum",
                Some(("n".to_owned(), tessari_session::Parameters::new())),
            )
            .unwrap(),
        ],
    };
    let first = ask(
        &authority,
        address,
        &Gather {
            reduce: Some(count_and_sum()),
            ..asking(table, 1)
        },
    )
    .unwrap();
    let id = |text: &str| RecordId::Text(text.to_owned());
    let states = |page: &Page| match &page.reduced {
        Some(tessari_session::Reduced::Partials(partials)) => partials
            .iter()
            .map(|partial| (partial.first.clone(), partial.states.first().cloned()))
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert!(first.records.is_empty() && first.more, "{first:?}");
    assert_eq!(first.resume, Some(id("b")));
    assert_eq!(
        states(&first),
        vec![(id("a"), Some(tessari_types::Value::from(2_i64)))]
    );
    let second = ask(
        &authority,
        address,
        &Gather {
            reduce: Some(count_and_sum()),
            after: first.resume.clone(),
            ..asking(table, 1)
        },
    )
    .unwrap();
    handle.join().unwrap();
    assert!(second.records.is_empty() && !second.more, "{second:?}");
    assert_eq!(second.resume, None);
    assert_eq!(
        states(&second),
        vec![(id("c"), Some(tessari_types::Value::from(1_i64)))]
    );
}

/// ADR-0103: a shard's search figures, counted on its leader a page of
/// records at a time and summed by the asker, are what the leader's own
/// index holds for those records.
#[test]
fn a_shards_search_figures_are_counted_page_by_page_and_no_record_travels() {
    let db = Db::in_memory().unwrap();
    db.session()
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; \
                 USE DATABASE shop; \
                 DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer; \
                 DEFINE TABLE docs (body string ANALYZER english) IDENTITY uuid SPLIT AT 'g'; \
                 DEFINE INDEX by_body ON docs FIELDS body SEARCH; \
                 CREATE docs:'a' = { body: 'fox jumps' }; \
                 CREATE docs:'b' = { body: 'a fox and a fox' }; \
                 CREATE docs:'c' = { body: 'dogs' }; CREATE docs:'h' = { body: 'fox' };",
        )
        .unwrap();
    let (table, index) = {
        let mut transaction = db.store().begin().unwrap();
        let table = Catalog::new(&mut transaction)
            .table_id(NamespaceId::new(1), DatabaseId::new(1), "docs")
            .unwrap()
            .unwrap();
        let index = Catalog::new(&mut transaction)
            .indexes_on(table)
            .unwrap()
            .into_iter()
            .find(|index| index.search)
            .unwrap();
        (table, index.id)
    };
    let db = Arc::new(db);
    let authority = Authority::new();
    let (address, handle) = folding_door(&authority, &db, Some(shard(table, 2)), (1 << 20, 2), 2);
    let mut gather = Gather {
        counting: Some(tessari_session::Counting {
            index,
            terms: ["fox".to_owned(), "dog".to_owned()].to_vec(),
        }),
        ..asking(table, 1)
    };
    let first = ask(&authority, address, &gather).unwrap();
    gather.after = first.resume.clone();
    let second = ask(&authority, address, &gather).unwrap();
    handle.join().unwrap();
    assert!(first.records.is_empty() && first.more, "{first:?}");
    assert_eq!(first.resume, Some(RecordId::from("b")));
    assert!(second.records.is_empty() && !second.more, "{second:?}");
    let counted = |page: &Page| page.counted.clone().unwrap();
    // a and b: two documents, 2 + 5 tokens, both say fox; then c: one
    // document of one token, `dog` once stemmed. Not h, which is shard 2.
    assert_eq!(
        counted(&first),
        tessari_storage::SearchCounts {
            documents: 2,
            tokens: 7,
            holding: vec![2, 0],
        }
    );
    assert_eq!(
        counted(&second),
        tessari_storage::SearchCounts {
            documents: 1,
            tokens: 1,
            holding: vec![0, 1],
        }
    );
}

/// A page of groups past the byte budget declines rather than being cut.
#[test]
fn a_page_of_groups_past_the_budget_declines() {
    let authority = Authority::new();
    let (db, table) = leader();
    let (address, handle) = door(&authority, &db, Some(shard(table, 2)), 1, 1);
    let page = ask(
        &authority,
        address,
        &Gather {
            reduce: Some(tessari_session::Reduce {
                visible: None,
                condition: None,
                keys: Vec::new(),
                folds: vec![tessari_session::Folded::named("count", None).unwrap()],
            }),
            ..asking(table, 1)
        },
    )
    .unwrap();
    handle.join().unwrap();
    assert_eq!(page.reduced, Some(tessari_session::Reduced::Declined));
    assert!(page.records.is_empty() && !page.more, "{page:?}");
}

/// G050 C4: what a gather moves, in bytes of `Gathered` page bodies, for
/// one shard of 1 100 records read whole, bounded to 3, and narrowed to 5.
#[test]
fn a_bounded_or_narrowed_gather_moves_fewer_bytes() {
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
    let (address, handle) = door(&authority, &db, Some(Reach::Store), 1 << 20, 6);
    let moved = |first: Gather| -> (usize, usize) {
        let mut gather = first;
        let (mut bytes, mut records) = (0, 0);
        loop {
            let page = ask(&authority, address, &gather).unwrap();
            bytes += page.encode().len();
            records += page.records.len();
            if !page.more {
                return (bytes, records);
            }
            gather.after = page
                .resume
                .clone()
                .or_else(|| page.records.last().map(|(id, _)| id.clone()));
        }
    };
    let whole = moved(asking(table, 1));
    let bounded = moved(Gather {
        enough: Some(3),
        ..asking(table, 1)
    });
    let narrowed = moved(Gather {
        pushed: Some(narrowed("(n >= $p0)", 1095, None)),
        ..asking(table, 1)
    });
    // ADR-0097 D2: `count(*)` and `sum(n)` over the same shard, folded.
    let folded = moved(Gather {
        reduce: Some(tessari_session::Reduce {
            visible: None,
            condition: None,
            keys: Vec::new(),
            folds: vec![
                tessari_session::Folded::named("count", None).unwrap(),
                tessari_session::Folded::named(
                    "sum",
                    Some(("n".to_owned(), tessari_session::Parameters::new())),
                )
                .unwrap(),
            ],
        }),
        ..asking(table, 1)
    });
    handle.join().unwrap();
    eprintln!(
        "GATHER-BYTES whole={whole:?} bounded={bounded:?} narrowed={narrowed:?} \
             folded={folded:?}"
    );
    assert_eq!((whole.1, bounded.1, narrowed.1, folded.1), (1100, 3, 5, 0));
    assert!(
        bounded.0 * 100 < whole.0 && narrowed.0 * 100 < whole.0 && folded.0 * 100 < whole.0,
        "{whole:?} {bounded:?} {narrowed:?} {folded:?}"
    );
}
