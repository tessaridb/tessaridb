//! The space a RESP connection reads and writes (ADR-0129 D9).
//!
//! `<namespace>.<database>.<space>`, each part an identifier — letters, digits
//! and underscores, not starting with a digit — because each is put into a
//! statement as a name, and a name is grammar.

/// The space a connection's keys live in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Space {
    /// The namespace.
    pub namespace: String,
    /// The database.
    pub database: String,
    /// The space.
    pub name: String,
}

impl Space {
    /// Read `<namespace>.<database>.<space>`, or `None` when it is not three
    /// identifiers.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('.');
        let (Some(namespace), Some(database), Some(name), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        Self::of(namespace, database, name)
    }

    /// The space named by three parts, or `None` when one is no identifier.
    #[must_use]
    pub fn of(namespace: &str, database: &str, name: &str) -> Option<Self> {
        [namespace, database, name]
            .iter()
            .all(|part| is_identifier(part))
            .then(|| Self {
                namespace: namespace.to_owned(),
                database: database.to_owned(),
                name: name.to_owned(),
            })
    }
}

/// Whether `text` is an identifier the store's grammar reads as a name.
fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|next| next.is_ascii_alphanumeric() || next == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_space_is_three_identifiers() {
        let space = Space::parse("app.main.cache").unwrap();
        assert_eq!(
            (
                space.namespace.as_str(),
                space.database.as_str(),
                space.name.as_str()
            ),
            ("app", "main", "cache")
        );
        for refused in [
            "app.main",
            "a.b.c.d",
            "app.main.ca-che",
            "app.main.1x",
            "a; DROP.b.c",
            "",
        ] {
            assert!(Space::parse(refused).is_none(), "{refused}");
        }
    }
}
