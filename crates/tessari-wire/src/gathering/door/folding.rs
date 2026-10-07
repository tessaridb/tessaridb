use super::*;

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
            tessari_session::Folded::named("count", None, None).unwrap(),
            tessari_session::Folded::named(
                "sum",
                Some(("n".to_owned(), tessari_session::Parameters::new())),
                None,
            )
            .unwrap(),
        ],
        samples: false,
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
                folds: vec![tessari_session::Folded::named("count", None, None).unwrap()],
                samples: false,
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
                tessari_session::Folded::named("count", None, None).unwrap(),
                tessari_session::Folded::named(
                    "sum",
                    Some(("n".to_owned(), tessari_session::Parameters::new())),
                    None,
                )
                .unwrap(),
            ],
            samples: false,
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

/// ADR-0121: asked for the folds that hold their group, a leader sends their
/// states — a median's runs, a collect's values in key order, a counter's
/// summary, or its samples when asked again for them — and no record.
#[test]
fn the_folds_that_hold_their_group_travel_as_states_and_no_record_travels() {
    let authority = Authority::new();
    let (db, table) = leader();
    let (address, handle) = folding_door(&authority, &db, Some(shard(table, 2)), (1 << 20, 16), 2);
    let text = |text: &str| Some((text.to_owned(), tessari_session::Parameters::new()));
    let holding = |samples| tessari_session::Reduce {
        visible: None,
        condition: None,
        keys: Vec::new(),
        folds: [
            tessari_session::Folded::named("median", text("n"), None),
            tessari_session::Folded::named("collect", text("n"), None),
            tessari_session::Folded::named(
                "increase",
                text("n"),
                text("datetime '2026-10-07T10:00:00Z'"),
            ),
        ]
        .into_iter()
        .flatten()
        .collect(),
        samples,
    };
    let states = |samples| {
        let page = ask(
            &authority,
            address,
            &Gather {
                reduce: Some(holding(samples)),
                ..asking(table, 1)
            },
        )
        .unwrap();
        assert!(page.records.is_empty() && !page.more, "{page:?}");
        match page.reduced {
            Some(tessari_session::Reduced::Partials(mut partials)) if partials.len() == 1 => {
                partials.remove(0).states
            }
            other => panic!("{other:?}"),
        }
    };
    let summary = states(false);
    let samples = states(true);
    handle.join().unwrap();
    assert_eq!(summary.len(), 3, "{summary:?}");
    let run = |value: &str, many: i64| {
        tessari_types::Value::Array(vec![
            tessari_types::Value::Number(tessari_types::Number::Decimal(value.parse().unwrap())),
            tessari_types::Value::from(many),
        ])
    };
    // Shard 1 holds a, b and c: 1, 2 and 3.
    assert_eq!(
        summary.first(),
        Some(&tessari_types::Value::Array(vec![
            run("1", 1),
            run("2", 1),
            run("3", 1)
        ]))
    );
    assert_eq!(
        summary.get(1),
        Some(&tessari_types::Value::Array(
            [1_i64, 2, 3].map(tessari_types::Value::from).to_vec()
        ))
    );
    let tag = |state: Option<&tessari_types::Value>| match state {
        Some(tessari_types::Value::Array(held)) => held.first().cloned(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        tag(summary.get(2)),
        Some(tessari_types::Value::from("summary"))
    );
    assert_eq!(
        tag(samples.get(2)),
        Some(tessari_types::Value::from("samples"))
    );
    // Only the counter answers differently when asked for its samples.
    assert_eq!(samples.get(..2), summary.get(..2));
}
