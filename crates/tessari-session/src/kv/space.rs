//! `DEFINE SPACE name [MAX n [EVICT NONE]]` — the space's own kind and its
//! limit, from the statement to the catalog (G036). Enforcement is the store's,
//! in the commit; see `tessari-storage`'s `bounded` module.

use tessari_ql::SpaceBound;
use tessari_storage::{Eviction, SpaceDeclaration, SpaceLimit};

/// The catalog declaration a `DEFINE SPACE` names.
pub(crate) fn declared_space(bound: Option<SpaceBound>) -> SpaceDeclaration {
    SpaceDeclaration {
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

/// The `MAX … [EVICT NONE]` clause a declaration writes back as, or nothing.
pub(crate) fn space_clause(declared: &SpaceDeclaration) -> String {
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
