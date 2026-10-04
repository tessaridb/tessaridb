use super::*;

const PROD: NamespaceId = NamespaceId::new(3);
const OTHER: NamespaceId = NamespaceId::new(4);
const LIBRARY: DatabaseId = DatabaseId::new(7);
const ARCHIVE: DatabaseId = DatabaseId::new(8);

#[test]
fn the_rule_a_ladder_could_not_express_is_a_value_here() {
    // The whole reason this type exists: write without manage, at a
    // namespace. Unrepresentable in any total order over roles.
    let held = Held::of([
        Authority::new(Kind::Read, Reach::Namespace(PROD)),
        Authority::new(Kind::Write, Reach::Namespace(PROD)),
    ]);
    assert!(held.permits(Kind::Write, Reach::Database(PROD, LIBRARY)));
    assert!(
        !held.permits(Kind::Manage, Reach::Namespace(PROD)),
        "writing a namespace's records must not confer creating databases in it"
    );
    assert!(!held.permits(Kind::Manage, Reach::Database(PROD, LIBRARY)));
}

#[test]
fn the_only_implication_is_downward_containment() {
    let store = Held::of([Authority::new(Kind::Read, Reach::Store)]);
    assert!(store.permits(Kind::Read, Reach::Namespace(PROD)));
    assert!(store.permits(Kind::Read, Reach::Database(PROD, LIBRARY)));

    let database = Held::of([Authority::new(Kind::Read, Reach::Database(PROD, LIBRARY))]);
    assert!(!database.permits(Kind::Read, Reach::Namespace(PROD)));
    assert!(!database.permits(Kind::Read, Reach::Store));
    assert!(!database.permits(Kind::Read, Reach::Database(PROD, ARCHIVE)));
}

#[test]
fn no_kind_implies_another() {
    for holding in Kind::ALL {
        let held = Held::of([Authority::new(*holding, Reach::Store)]);
        for wanted in Kind::ALL {
            assert_eq!(
                held.permits(*wanted, Reach::Store),
                holding == wanted,
                "{} must answer {} only for itself",
                holding.name(),
                wanted.name()
            );
        }
    }
}

#[test]
fn a_namespace_does_not_reach_a_sibling() {
    let held = Held::of([Authority::new(Kind::Manage, Reach::Namespace(PROD))]);
    assert!(held.permits(Kind::Manage, Reach::Database(PROD, LIBRARY)));
    assert!(!held.permits(Kind::Manage, Reach::Namespace(OTHER)));
    assert!(!held.permits(Kind::Manage, Reach::Database(OTHER, LIBRARY)));
}

#[test]
fn selecting_a_container_looks_both_up_and_down_while_a_demand_looks_only_down() {
    // The asymmetry that matters, and getting it backwards locks a
    // database-scoped user out of the namespace their database is in.
    let held = Held::of([Authority::new(Kind::Read, Reach::Database(PROD, LIBRARY))]);

    // A demand is answered at the container reached, and a database reach
    // contains nothing above it.
    assert!(!held.permits(Kind::Read, Reach::Namespace(PROD)));
    assert!(!held.anything_at(Reach::Namespace(PROD)));

    // Selecting looks both ways: `prod` is a step on the way to `prod.shop`.
    assert!(held.touches(Reach::Namespace(PROD)));
    assert!(held.touches(Reach::Database(PROD, LIBRARY)));
    assert!(held.touches(Reach::Store));

    // And it still refuses a container this holder has no business in, in
    // either direction — which is the oracle it exists to close.
    assert!(!held.touches(Reach::Namespace(OTHER)));
    assert!(!held.touches(Reach::Database(PROD, ARCHIVE)));
    assert!(!held.touches(Reach::Database(OTHER, LIBRARY)));
    assert!(!Held::nothing().touches(Reach::Store));
}

#[test]
fn holding_nothing_refuses_everything() {
    let held = Held::nothing();
    for kind in Kind::ALL {
        for reach in [
            Reach::Store,
            Reach::Namespace(PROD),
            Reach::Database(PROD, LIBRARY),
        ] {
            assert!(!held.permits(*kind, reach));
        }
    }
    assert!(!held.anything_at(Reach::Store));
}

#[test]
fn the_top_is_every_kind_at_the_store_and_not_a_special_case() {
    let held = Held::every_kind_at(Reach::Store);
    for kind in Kind::ALL {
        assert!(held.permits(*kind, Reach::Database(PROD, LIBRARY)));
    }
}

#[test]
fn selecting_a_container_asks_for_any_authority_and_not_for_read() {
    // A govern-only administrator selects the namespace they administer.
    let held = Held::of([Authority::new(Kind::Govern, Reach::Namespace(PROD))]);
    assert!(held.anything_at(Reach::Namespace(PROD)));
    assert!(held.anything_at(Reach::Database(PROD, LIBRARY)));
    assert!(!held.permits(Kind::Read, Reach::Namespace(PROD)));
    assert!(!held.anything_at(Reach::Namespace(OTHER)));
}

#[test]
fn a_shard_is_a_reach_the_catalog_stores_and_no_authority_comes_in() {
    let shard = Reach::Shard(
        NamespaceId::new(3),
        DatabaseId::new(4),
        TableId::new(5),
        ShardId::new(2),
    );
    assert_eq!(
        Reach::from_value(&shard.to_value(), "leadership", "range")
            .expect("a shard reach reads back"),
        shard
    );
    for kind in [
        Kind::Read,
        Kind::Write,
        Kind::Manage,
        Kind::Govern,
        Kind::Operate,
        Kind::Replicate,
    ] {
        assert!(!kind.may_be_held_at(shard), "{kind:?} at a shard");
    }
    // Written faithfully all the same, so a stored row naming one reads back
    // as what it says rather than as a wider grant.
    let held = Authority::new(Kind::Read, shard);
    assert_eq!(read(&written(held)), Some(held));
}

#[test]
fn shard_zero_is_not_a_shard_in_either_spelling() {
    let mut stored = Reach::Shard(
        NamespaceId::new(3),
        DatabaseId::new(4),
        TableId::new(5),
        ShardId::new(1),
    )
    .to_value();
    if let Value::Object(fields) = &mut stored {
        fields.insert(FIELD_SHARD.to_owned(), Value::from(0_i64));
    }
    assert!(Reach::from_value(&stored, "leadership", "range").is_err());
    assert_eq!(read("read@3.4.5.0"), None);
}

#[test]
fn every_authority_survives_the_round_trip() {
    let mut held = Held::nothing();
    for kind in Kind::ALL {
        held.add(Authority::new(*kind, Reach::Store));
        held.add(Authority::new(*kind, Reach::Namespace(PROD)));
        held.add(Authority::new(*kind, Reach::Database(PROD, LIBRARY)));
    }
    let written = held.to_value();
    assert_eq!(
        Held::from_value(&written).expect("the set this test just wrote"),
        held
    );
}

#[test]
fn the_written_form_is_the_one_documented() {
    assert_eq!(
        written(Authority::new(Kind::Read, Reach::Store)),
        "read@store"
    );
    assert_eq!(
        written(Authority::new(Kind::Write, Reach::Namespace(PROD))),
        "write@3"
    );
    assert_eq!(
        written(Authority::new(Kind::Manage, Reach::Database(PROD, LIBRARY))),
        "manage@3.7"
    );
}

#[test]
fn an_authority_this_binary_does_not_know_is_corruption_and_not_a_shrug() {
    let value = Value::Array(vec![Value::from("transcend@store")]);
    assert!(
        Held::from_value(&value).is_err(),
        "an unknown kind must refuse rather than be dropped from the set"
    );
    assert!(Held::from_value(&Value::from("read@store")).is_err());
    assert!(Held::from_value(&Value::Array(vec![Value::from("read")])).is_err());
    assert!(Held::from_value(&Value::Array(vec![Value::from("read@nowhere")])).is_err());
}

#[test]
fn a_record_with_no_authorities_falls_back_to_its_role() {
    let fields = BTreeMap::new();
    let reach = Reach::Database(PROD, LIBRARY);

    let viewer = held_of(&fields, Some(Role::Viewer), reach).expect("a viewer");
    assert!(viewer.permits(Kind::Read, reach));
    assert!(!viewer.permits(Kind::Write, reach));

    // The bundle the new rule forbids, preserved on purpose: an editor was
    // declared under a promise that they may define structure.
    let editor = held_of(&fields, Some(Role::Editor), reach).expect("an editor");
    assert!(editor.permits(Kind::Read, reach));
    assert!(editor.permits(Kind::Write, reach));
    assert!(editor.permits(Kind::Manage, reach));
    assert!(!editor.permits(Kind::Govern, reach));
    assert!(!editor.permits(Kind::Operate, reach));

    let owner = held_of(&fields, Some(Role::Owner), reach).expect("an owner");
    for kind in Kind::ALL {
        assert_eq!(
            owner.permits(*kind, reach),
            kind.may_be_held_at(reach),
            "an owner holds every kind this reach can hold and only those: {}",
            kind.name()
        );
    }
    // Named as well as derived. The loop above compares the bundle against
    // the rule, so both being wrong the same way would pass it; this says
    // which kind the rule is about at a reach below the store.
    assert!(
        !owner.permits(Kind::Replicate, reach),
        "an owner of one database does not hold the store's log"
    );
}

/// The row an older binary already wrote, and the reason the rule is asked
/// in `permits` rather than only at the two statements that refuse it.
///
/// Constructed directly because there is no longer any way to say it: every
/// road into the set now filters or refuses. That is the point — this is the
/// state of every store on disk that declared a namespace owner before the
/// rule existed, and a guard that only closes the door leaves all of those
/// standing open.
#[test]
fn a_stored_authority_at_a_reach_its_kind_cannot_reach_answers_nothing() {
    let reach = Reach::Namespace(PROD);
    let legacy = Held::of([
        Authority::new(Kind::Replicate, reach),
        Authority::new(Kind::Read, reach),
    ]);

    assert!(
        !legacy.permits(Kind::Replicate, reach),
        "a namespace-reach replication row from an older binary must authorise nothing"
    );
    assert!(
        !legacy.permits(Kind::Replicate, Reach::Store),
        "and it must not have been read upward into the store either"
    );
    // The neighbouring row is untouched, so this refuses one authority and
    // not the record that carries it.
    assert!(legacy.permits(Kind::Read, reach));
}

#[test]
fn an_explicit_set_beats_the_role_it_was_declared_with() {
    // The migration's whole point: once a record carries a set, the role is
    // no longer what decides — otherwise narrowing an editor would be
    // impossible without deleting them.
    let narrowed = Held::of([
        Authority::new(Kind::Read, Reach::Namespace(PROD)),
        Authority::new(Kind::Write, Reach::Namespace(PROD)),
    ]);
    let fields = BTreeMap::from([(FIELD_AUTHORITIES.to_owned(), narrowed.to_value())]);
    let held = held_of(&fields, Some(Role::Editor), Reach::Namespace(PROD)).expect("the set");
    assert_eq!(held, narrowed);
    assert!(
        !held.permits(Kind::Manage, Reach::Namespace(PROD)),
        "the stored set decides, not the role the record still carries"
    );
}

#[test]
fn a_role_summary_is_the_widest_that_fits_and_never_a_near_miss() {
    let reach = Reach::Database(PROD, LIBRARY);

    // Each role summarises as itself, which is what makes the two spellings
    // of a declaration one declaration.
    for role in Role::ALL {
        assert_eq!(
            Held::from_role(*role, reach).role_within(reach),
            Some(*role),
            "{role:?} must summarise as itself"
        );
    }

    // Widest that fits, not nearest. An owner's set contains a viewer's, so
    // a search that stopped at the first match would report `viewer` for a
    // user holding everything — and this is the direction that matters,
    // because the summary is what an older binary reads.
    assert_eq!(
        Held::every_kind_at(reach).role_within(reach),
        Some(Role::Owner)
    );

    // A superset of `editor` that is not `owner` still summarises as the
    // editor it contains, and never as the owner it does not.
    let more = Held::of([
        Authority::new(Kind::Read, reach),
        Authority::new(Kind::Write, reach),
        Authority::new(Kind::Manage, reach),
        Authority::new(Kind::Operate, reach),
    ]);
    assert_eq!(more.role_within(reach), Some(Role::Editor));

    // And the case the whole model exists for has no summary at all. Every
    // role begins with `read`, so a set without it fits none of them —
    // reporting the nearest would hand an older binary a read this user does
    // not hold.
    let ungovernable = Held::of([Authority::new(Kind::Manage, reach)]);
    assert_eq!(ungovernable.role_within(reach), None);
    assert_eq!(Held::nothing().role_within(reach), None);

    // A set held at a *different* reach summarises as nothing here, so a
    // namespace authority is never reported as a role over one database in
    // it. Containment runs downward through holding, not through summary.
    assert_eq!(
        Held::from_role(Role::Owner, Reach::Namespace(PROD)).role_within(reach),
        None
    );
}

#[test]
fn removing_takes_only_what_was_named() {
    let mut held = Held::of([
        Authority::new(Kind::Write, Reach::Store),
        Authority::new(Kind::Write, Reach::Namespace(PROD)),
    ]);
    assert!(held.remove(&Authority::new(Kind::Write, Reach::Namespace(PROD))));
    assert!(
        held.permits(Kind::Write, Reach::Namespace(PROD)),
        "the store-wide authority still contains this reach and was not rewritten"
    );
    assert!(!held.remove(&Authority::new(Kind::Read, Reach::Store)));
}

#[test]
fn a_reach_comes_from_the_tenancy_and_a_database_needs_its_namespace() {
    assert_eq!(Reach::of(None, None), Some(Reach::Store));
    assert_eq!(Reach::of(Some(PROD), None), Some(Reach::Namespace(PROD)));
    assert_eq!(
        Reach::of(Some(PROD), Some(LIBRARY)),
        Some(Reach::Database(PROD, LIBRARY))
    );
    assert_eq!(
        Reach::of(None, Some(LIBRARY)),
        None,
        "a database in no namespace is not a place"
    );
}

#[test]
fn a_kind_reads_back_from_how_it_is_written() {
    for kind in Kind::ALL {
        assert_eq!(Kind::parse(kind.name()), Some(*kind));
    }
    assert_eq!(Kind::parse("Read"), None, "names are case-sensitive");
    assert_eq!(Kind::parse("administer"), None);
}
