//! A Kafka topic name, read as the store's tenancy and topic (ADR-0129 D4).
//!
//! `<namespace>.<database>.<topic>`, or a bare `<topic>` in the database the
//! node was told to read bare names in. Each part must be an identifier —
//! letters, digits and underscores, not starting with a digit — because each
//! is interpolated into a statement as a name, and a name is grammar.

/// The database bare names are read in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Database {
    /// The namespace.
    pub namespace: String,
    /// The database.
    pub database: String,
}

impl Database {
    /// Read `<namespace>.<database>`, or `None` when it is not two identifiers.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (namespace, database) = text.split_once('.')?;
        (is_identifier(namespace) && is_identifier(database)).then(|| Self {
            namespace: namespace.to_owned(),
            database: database.to_owned(),
        })
    }
}

/// A topic a Kafka name stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Topic<'a> {
    pub(crate) namespace: &'a str,
    pub(crate) database: &'a str,
    pub(crate) name: &'a str,
}

impl Topic<'_> {
    /// The tenancy a statement on this topic runs in.
    pub(crate) const fn tenancy(&self) -> (&str, &str) {
        (self.namespace, self.database)
    }
}

/// Whether `text` is an identifier the store's grammar reads as a name.
pub(crate) fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|next| next.is_ascii_alphanumeric() || next == '_')
}

/// The topic a Kafka name stands for, or `None` when it stands for none.
pub(crate) fn topic<'a>(name: &'a str, bare: Option<&'a Database>) -> Option<Topic<'a>> {
    let parts: Vec<&str> = name.split('.').collect();
    let topic = match (parts.as_slice(), bare) {
        ([namespace, database, topic], _) => Topic {
            namespace,
            database,
            name: topic,
        },
        ([topic], Some(bare)) => Topic {
            namespace: &bare.namespace,
            database: &bare.database,
            name: topic,
        },
        _ => return None,
    };
    [topic.namespace, topic.database, topic.name]
        .iter()
        .all(|part| is_identifier(part))
        .then_some(topic)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_three_identifiers_or_one_in_the_bare_database() {
        let bare = Database::parse("shop.main").unwrap();
        assert_eq!(
            topic("app.prod.orders", None).unwrap().tenancy(),
            ("app", "prod")
        );
        assert_eq!(
            topic("orders", Some(&bare)).unwrap().tenancy(),
            ("shop", "main")
        );
        assert!(
            topic("orders", None).is_none(),
            "a bare name with no database"
        );
        for refused in [
            "a.b",
            "a.b.c.d",
            "app.prod.or-ders",
            "app.prod.1x",
            "a; DROP.b.c",
            "",
        ] {
            assert!(topic(refused, Some(&bare)).is_none(), "{refused}");
        }
    }
}
