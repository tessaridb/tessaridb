use super::{Function, Purity};

/// The whole membership of [`Purity::PerStatement`], asserted as a set.
///
/// The guard the type system cannot give. `purity` forces a new function to
/// be *classified*, but a classification is a claim about how the function
/// must be evaluated, and the two can be written apart: a function put here
/// wrongly is folded to one value per statement and nothing says so.
///
/// So the set is written down. Adding a member fails this test, and the
/// failure is the instruction: go and read what folding does to it before
/// widening this list.
#[test]
fn the_statement_constant_functions_are_exactly_the_one_the_fold_was_written_for() {
    let constant: Vec<&str> = Function::ALL
        .iter()
        .filter(|function| function.purity() == Purity::PerStatement)
        .map(|function| function.spelling())
        .collect();
    // `session::context` joined in G051 SG3: fixed for a statement, and
    // folding it once above the records is exactly right.
    assert_eq!(constant, ["time::now", "session::context"]);
}

/// The whole membership of [`Purity::PerCall`], asserted the same way.
///
/// This is the list `plan::fold` refuses to evaluate above the records, so
/// widening it makes a read slower and narrowing it makes one **wrong** —
/// asymmetric, and the direction that costs correctness is the one a
/// classification typo takes silently.
#[test]
fn the_functions_the_fold_may_not_touch_are_exactly_the_one_that_generates() {
    let afresh: Vec<&str> = Function::ALL
        .iter()
        .filter(|function| function.purity() == Purity::PerCall)
        .map(|function| function.spelling())
        .collect();
    // `search::ranks` joined in G038: it reads no argument and no record, so
    // it looks constant, and folded it would hand every row one row's ranks.
    // The three a `FROM SEARCH` record answers joined in G051 (ADR-0105) for
    // the same reason: `search::score()` takes no argument there.
    assert_eq!(
        afresh,
        [
            "rand::uuid",
            "search::score",
            "search::ranks",
            "search::table_name",
            "search::snippet"
        ]
    );
}

#[test]
fn every_function_is_classified_and_only_the_clock_and_the_generator_are_not_pure() {
    for function in Function::ALL {
        let expected = match function {
            Function::TimeNow | Function::SessionContext => Purity::PerStatement,
            Function::RandUuid
            | Function::SearchRanks
            | Function::SearchScore
            | Function::SearchTable
            | Function::SearchSnippet => Purity::PerCall,
            _ => Purity::Pure,
        };
        assert_eq!(function.purity(), expected, "{function} is misclassified");
    }
}

/// Every cast names a kind a field can be declared as, spelled identically.
///
/// The decision this holds in place: a cast and a `DEFINE FIELD … TYPE` say
/// the same word for the same kind. Two vocabularies for one type system is
/// the sort of thing that reads fine in each file and forces every author to
/// remember which side of the language they are on — `type::integer(x)` into
/// a field declared `int`, and no error anywhere to point at it.
///
/// `type::of` is excluded because it is not a cast: it answers *about* a
/// value rather than producing one of a kind.
#[test]
fn every_cast_spells_its_kind_the_way_a_field_declaration_does() {
    let casts: Vec<&str> = Function::ALL
        .iter()
        .filter_map(|function| {
            function
                .spelling()
                .strip_prefix("type::")
                .filter(|name| *name != "of")
        })
        .collect();
    assert_eq!(
        casts,
        ["bool", "int", "float", "string", "datetime", "uuid"],
        "the cast set moved"
    );
    for name in casts {
        assert!(
            tessari_types::FieldKind::parse(name).is_some_and(|kind| kind.name() == name),
            "`type::{name}` is not the spelling a field declaration uses"
        );
    }
}

#[test]
fn every_function_is_findable_by_its_own_spelling_and_no_two_share_one() {
    let mut spellings: Vec<&str> = Function::ALL
        .iter()
        .map(|function| {
            assert_eq!(Function::parse(function.spelling()), Some(*function));
            function.spelling()
        })
        .collect();
    let count = spellings.len();
    spellings.sort_unstable();
    spellings.dedup();
    assert_eq!(spellings.len(), count, "two functions share a spelling");
}

#[test]
fn a_name_that_is_not_a_function_is_not_one() {
    // Case-sensitive, unlike a keyword: a function name is a name, and every
    // other name in this language is case-sensitive too.
    assert_eq!(Function::parse("string::length"), None);
    assert_eq!(Function::parse("STRING::LEN"), None);
    assert_eq!(Function::parse("len"), None);
}
