//! Who may talk to this store.
//!
//! A user is a catalog entry like any other — an ordinary record in the system
//! tenancy (ADR-0009) — so declaring one takes part in the transaction that
//! issued it and reaches every replica through the same apply path.
//!
//! # What is stored is a hash, and the store does not know how it was made
//!
//! The catalog holds an opaque string. It is produced and checked one layer up,
//! by the session, using a password-hashing function this store does not
//! implement — because a password hash written here would be the one place in
//! this project where doing it ourselves is not a trade-off but a mistake.
//!
//! The consequence worth naming: **the plaintext never reaches this layer**, so
//! it never reaches the log, and therefore never reaches a replica or a backup.
//!
//! # A user belongs to a tenancy, or to the store
//!
//! A store-level user is the root; everyone else is scoped to a namespace and a
//! database and cannot see another tenant's data at all. Authorization therefore
//! uses the shape the tenancy already has rather than inventing a second one.

use std::collections::BTreeMap;

use tessari_encoding::decode_payload;
use tessari_types::{DatabaseId, NamespaceId, RecordId, Value};

use super::definition::{field_id, field_name, number, object};
use super::{Catalog, Level, id_key, qualify, system};
use crate::error::{Error, Result};

const FIELD_ID: &str = "id";
const FIELD_NAME: &str = "name";
const FIELD_NAMESPACE: &str = "namespace";
const FIELD_DATABASE: &str = "database";
const FIELD_ROLE: &str = "role";
const FIELD_SECRET: &str = "secret";

const ENTITY: &str = "user";

/// What may be done to a table.
///
/// The same two words a grant is written with and a role is measured against, so
/// that "may ada read this" is one question with one vocabulary rather than a
/// role's answer and a grant's answer needing to be reconciled.
///
/// Administering is deliberately absent: declaring a user is a store-level act
/// and there is no table to grant it on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verb {
    /// Reading records.
    Read,
    /// Writing records, and declaring structure on the table.
    Write,
}

impl Verb {
    /// Every verb, so a listing cannot drift from the set.
    pub const ALL: &'static [Self] = &[Self::Read, Self::Write];

    /// How the verb is written.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }

    /// Read one back from how it is written.
    ///
    /// Case-sensitive, like every other name in this language.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|held| held.name() == text)
    }
}

/// What a user is allowed to do.
///
/// Three, not a grant matrix. A matrix over verbs and objects is a real feature
/// with a `GRANT` statement, a revocation story and a place to put a per-object
/// list, and half of one is worse than none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    /// Read what the tenancy holds, and nothing else.
    Viewer,
    /// Read and write records, and define structure.
    Editor,
    /// Everything, including declaring users.
    Owner,
}

impl Role {
    /// Every role, so a listing cannot drift from the set.
    pub const ALL: &'static [Self] = &[Self::Viewer, Self::Editor, Self::Owner];

    /// How the role is written.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Editor => "editor",
            Self::Owner => "owner",
        }
    }

    /// The role a word names, if it names one.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|role| role.name().eq_ignore_ascii_case(word))
    }
}

/// A declared user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserDefinition {
    /// The user's id.
    pub id: u32,
    /// The name signed in with, unique across the store.
    pub name: String,
    /// The namespace the user belongs to, or `None` for a store-level user.
    pub namespace: Option<NamespaceId>,
    /// The database within it, or `None`.
    pub database: Option<DatabaseId>,
    /// What the user may do.
    pub role: Role,
    /// The password hash, opaque to this layer.
    pub secret: String,
}

impl UserDefinition {
    /// The value written to the catalog.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(self.id)),
            (FIELD_NAME.to_owned(), Value::from(self.name.as_str())),
            (FIELD_ROLE.to_owned(), Value::from(self.role.name())),
            (FIELD_SECRET.to_owned(), Value::from(self.secret.as_str())),
        ]);
        if let Some(namespace) = self.namespace {
            fields.insert(FIELD_NAMESPACE.to_owned(), number(namespace.get()));
        }
        if let Some(database) = self.database {
            fields.insert(FIELD_DATABASE.to_owned(), number(database.get()));
        }
        Value::Object(fields)
    }

    /// Read a definition back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field is missing, holds the
    /// wrong type, or names a role this binary does not know.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, ENTITY)?;
        let malformed = |field: &'static str, found: &'static str| Error::CatalogMalformed {
            entity: ENTITY,
            field,
            found,
        };
        let Some(Value::String(role)) = fields.get(FIELD_ROLE) else {
            return Err(malformed(
                FIELD_ROLE,
                fields.get(FIELD_ROLE).map_or("none", Value::type_name),
            ));
        };
        // An unknown role is corruption rather than a bad request: it was
        // written by something that knew a role this binary does not, and
        // guessing would grant or refuse the wrong thing.
        let Some(role) = Role::parse(role) else {
            return Err(malformed(FIELD_ROLE, "an unknown role"));
        };
        let Some(Value::String(secret)) = fields.get(FIELD_SECRET) else {
            return Err(malformed(
                FIELD_SECRET,
                fields.get(FIELD_SECRET).map_or("none", Value::type_name),
            ));
        };
        Ok(Self {
            id: field_id(fields, FIELD_ID, ENTITY)?,
            name: field_name(fields, ENTITY)?,
            namespace: fields
                .contains_key(FIELD_NAMESPACE)
                .then(|| field_id(fields, FIELD_NAMESPACE, ENTITY).map(NamespaceId::new))
                .transpose()?,
            database: fields
                .contains_key(FIELD_DATABASE)
                .then(|| field_id(fields, FIELD_DATABASE, ENTITY).map(DatabaseId::new))
                .transpose()?,
            role,
            secret: secret.clone(),
        })
    }
}

impl Catalog<'_, '_> {
    /// Declare a user.
    ///
    /// `secret` is opaque here: this layer stores what it is given and never
    /// sees a plaintext, which is why one never reaches the log.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NameTaken`] when the name is already declared.
    pub fn create_user(
        &mut self,
        name: &str,
        namespace: Option<NamespaceId>,
        database: Option<DatabaseId>,
        role: Role,
        secret: &str,
    ) -> Result<UserDefinition> {
        let qualified = qualify(Level::User, &[], name);
        self.reserve_name(&qualified)?;
        let id = self.allocate(Level::User)?;
        let definition = UserDefinition {
            id,
            name: name.to_owned(),
            namespace,
            database,
            role,
            secret: secret.to_owned(),
        };
        self.write(system::USERS, id, &definition.to_value());
        self.claim_name(&qualified, id);
        Ok(definition)
    }

    /// Every declared user.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub fn users(&self) -> Result<Vec<UserDefinition>> {
        let mut found = Vec::new();
        for (_, payload) in self.transaction.scan_table(
            system::SYSTEM_NAMESPACE,
            system::SYSTEM_DATABASE,
            system::USERS,
        )? {
            found.push(UserDefinition::from_value(&decode_payload(&payload)?)?);
        }
        Ok(found)
    }

    /// Whether this store has any user at all.
    ///
    /// A store with none is **open**: requiring a signin against one would lock
    /// everybody out of it, and there would be no way in to fix that. Declaring
    /// the first user is what closes it.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub fn is_open(&self) -> Result<bool> {
        Ok(self
            .transaction
            .scan_table(
                system::SYSTEM_NAMESPACE,
                system::SYSTEM_DATABASE,
                system::USERS,
            )?
            .is_empty())
    }

    /// Remove a user's declaration and release its name.
    ///
    /// # Errors
    ///
    /// Returns an error when the substrate fails.
    pub fn drop_user(&mut self, user: &UserDefinition) -> Result<()> {
        let qualified = qualify(Level::User, &[], &user.name);
        self.transaction.delete(system::address(
            system::USERS,
            RecordId::Int(id_key(user.id)),
        ));
        self.transaction
            .delete(system::address(system::NAMES, RecordId::from(qualified)));
        Ok(())
    }
}
