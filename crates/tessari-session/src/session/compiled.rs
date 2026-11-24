//! The session's statement compile cache (G099 C3, Q-989's build).
//!
//! What is cached is the **compiled form** of a script — the tree parse and
//! binding produced — and nothing else. The tree is pure syntax: names
//! unresolved, catalog untouched, store unasked. Resolution and planning
//! run against the live catalog at execution, through the same gate a fresh
//! compile takes (the one `EXPLAIN` asks), so a cached compile cannot freeze
//! a plan, an index choice or a grant — a catalog change between two
//! identical calls is visible in the second call's plan, which the suite
//! arms. The catalog-version-keyed half of the classic plan cache already
//! exists in this engine as G072's `CatalogRows`; what Q-989 found missing
//! was this statement-level half.
//!
//! # What makes a script cacheable
//!
//! Everything parse+bind produces without asking the store. Binding that
//! fell back to `DEFINE PARAM` database defaults asked the store what the
//! defaults are — so a script that bound that way is **not** cached: the
//! bound tree bakes the default's value in, and a redefined default must
//! not be answered by the old one.
//!
//! # The key
//!
//! The session's tenancy, the source text, and a digest of the bound
//! values. Two calls that differ in any of the three compile separately;
//! the digest is the rendered values, so `$who = 'ada'` and `$who = 'grace'`
//! are different entries and each answers its own.

use std::collections::HashMap;

use tessari_ql::{Parameters, Script};

/// One compiled form and when it was last handed out, for the eviction.
#[derive(Debug)]
struct Entry {
    script: Script,
    tick: u64,
}

/// The bounded cache: at most [`tessari_constants::COMPILE_CACHE_MAX`]
/// entries, least-recently-used evicted past it. Single-threaded by the
/// session's own `&mut` discipline — no lock, no guard rules.
#[derive(Debug, Default)]
pub(crate) struct Compiled {
    entries: HashMap<Key, Entry>,
    tick: u64,
}

/// What identifies one compiled form.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    namespace: Option<String>,
    database: Option<String>,
    source: String,
    parameters: String,
}

impl Compiled {
    /// The compiled form of this exact call, when held.
    pub(crate) fn get(
        &mut self,
        namespace: &Option<String>,
        database: &Option<String>,
        source: &str,
        parameters: &Parameters,
    ) -> Option<Script> {
        let key = Key {
            namespace: namespace.clone(),
            database: database.clone(),
            source: source.to_owned(),
            parameters: digest(parameters),
        };
        let entry = self.entries.get_mut(&key)?;
        self.tick = self.tick.wrapping_add(1);
        entry.tick = self.tick;
        Some(entry.script.clone())
    }

    /// Hold a compiled form, evicting the least-recently-used past capacity.
    pub(crate) fn put(
        &mut self,
        namespace: &Option<String>,
        database: &Option<String>,
        source: &str,
        parameters: &Parameters,
        script: Script,
    ) {
        if self.entries.len() >= tessari_constants::COMPILE_CACHE_MAX {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.tick)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            }
        }
        self.tick = self.tick.wrapping_add(1);
        self.entries.insert(
            Key {
                namespace: namespace.clone(),
                database: database.clone(),
                source: source.to_owned(),
                parameters: digest(parameters),
            },
            Entry {
                script,
                tick: self.tick,
            },
        );
    }

    /// How many forms are held — the introspection the suite's bound test.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// The bound values as one deterministic string: the names in their sorted
/// order with each value's rendered form. A map iterates sorted, so the
/// digest is stable for one binding whatever the caller's insertion order.
fn digest(parameters: &Parameters) -> String {
    let mut out = String::new();
    for (name, value) in parameters.iter() {
        out.push_str(name);
        out.push('=');
        out.push_str(&format!("{value:?}"));
        out.push('\n');
    }
    out
}
