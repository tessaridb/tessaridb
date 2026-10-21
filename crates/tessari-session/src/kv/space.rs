//! `DEFINE SPACE name [MAX n [EVICT NONE]]` — the space's own kind and its
//! limit, from the statement to the catalog (G036). Enforcement is the store's,
//! in the commit; see `tessari-storage`'s `bounded` module.

use tessari_ql::SpaceBound;
use tessari_storage::{Eviction, SpaceDeclaration, SpaceLimit};

/// The catalog declaration a `DEFINE SPACE` names.
pub(crate) fn declared_space(memory: bool, bound: Option<SpaceBound>) -> SpaceDeclaration {
    SpaceDeclaration {
        memory,
        limit: bound.map(|bound| SpaceLimit {
            max: bound.max,
            eviction: if bound.refuse {
                Eviction::Refuse
            } else {
                Eviction::Modified
            },
        }),
    }
}

/// The `IN MEMORY MAX … [EVICT NONE]` clauses a declaration writes back as, or
/// nothing.
pub(crate) fn space_clause(declared: &SpaceDeclaration) -> String {
    let limit = limit_clause(declared);
    if declared.memory {
        format!(" IN MEMORY{limit}")
    } else {
        limit
    }
}

/// The `MAX … [EVICT NONE]` clause alone.
fn limit_clause(declared: &SpaceDeclaration) -> String {
    match declared.limit {
        None => String::new(),
        Some(SpaceLimit {
            max,
            eviction: Eviction::Modified,
        }) => format!(" MAX {max}"),
        Some(SpaceLimit {
            max,
            eviction: Eviction::Refuse,
        }) => format!(" MAX {max} EVICT NONE"),
    }
}
