//! Topics (G037): an append-only order of messages whose readers keep their
//! positions in the store.
//!
//! Appending is `CREATE` and `INSERT`, as enqueueing is for a queue: the store
//! already has the words. What a topic adds is the read — `READ FROM`, which
//! answers messages after a position and, for a named reader, moves that
//! reader's stored position inside the reader's own transaction — and the
//! report `INFO FOR TOPIC`. The rules that keep a message from being rewritten,
//! its size bounded and its retention stamped live in the commit
//! (`tessari_storage`), where every write path passes.

mod info;
mod public;
mod read;

pub(crate) use read::Reading;

use std::fmt::Write as _;

use tessari_ql::TopicClauses;
use tessari_storage::{PublicAppend, TopicDeclaration};

/// The declaration a `DEFINE TOPIC` statement's clauses describe.
pub(crate) fn declared_topic(clauses: TopicClauses) -> TopicDeclaration {
    TopicDeclaration {
        retain: clauses.retain,
        max_bytes: clauses.max_bytes,
        public: clauses.public.map(|(rate, per)| PublicAppend { rate, per }),
    }
}

/// The clauses of a topic's declaration, as `DEFINE TOPIC` writes them.
pub(crate) fn topic_clauses(declared: &TopicDeclaration) -> String {
    let mut clauses = String::new();
    if let Some(retain) = declared.retain {
        let _ = write!(clauses, " RETAIN {}", retain.to_literal());
    }
    if let Some(max) = declared.max_bytes {
        let _ = write!(clauses, " MAX BYTES {max}");
    }
    if let Some(public) = declared.public {
        let _ = write!(
            clauses,
            " PUBLIC RATE {} PER {}",
            public.rate,
            public.per.to_literal()
        );
    }
    clauses
}
