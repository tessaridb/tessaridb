//! Users, grants and the reach they are given.

use super::Parser;
use tessari_types::Number;

use crate::ast::{
    Credential, Name, NamespaceChange, Password, ReachRef, StatementKind, TableChange, UserChange,
    UserGrant,
};
use crate::error::Result;
use crate::token::{Keyword, Punct, Token};

impl Parser<'_> {
    /// `DEFINE USER ada ON prod.orders ROLE editor PASSWORD '…'`
    ///
    /// `ON` names a tenancy the way `orders.users` names a table; without it the
    /// user belongs to the store and is its root.
    pub(super) fn define_user(&mut self) -> Result<StatementKind> {
        self.advance();
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        let scope = if self.eat_keyword(Keyword::On) {
            Some(self.reach_ref()?)
        } else {
            None
        };
        // `ROLE` first because it is what every existing statement says, and
        // `AUTHORITIES` reached only when the statement does not say `ROLE` —
        // so no spelling that parses today parses differently now.
        let role = if self.eat_keyword(Keyword::Role) {
            UserGrant::Role(self.name()?)
        } else if self.eat_word("authorities") {
            UserGrant::Authorities(self.kind_list()?)
        } else {
            return Err(self.error_here("`ROLE` or `AUTHORITIES` and what the user may do"));
        };
        // `PASSHASH` is contextual — an ordinary name everywhere else — and is
        // the spelling a state script writes a user with (ADR-0091): the store
        // never held the password, only what it hashed to.
        let credential = if self.eat_keyword(Keyword::Password) {
            let (password, _) = self.text("the password, as text")?;
            Credential::Password(Password::new(password))
        } else if self.eat_word("passhash") {
            let (hash, _) = self.text("the stored hash, as text")?;
            Credential::Hash(hash)
        } else {
            return Err(self.error_here("`PASSWORD` or `PASSHASH` and the credential"));
        };
        Ok(StatementKind::DefineUser {
            name,
            scope,
            role,
            credential,
            if_not_exists,
        })
    }

    /// `ALTER USER ada SET PASSWORD '…'` · `ALTER TABLE users SET SCHEMAFULL`
    ///
    /// The target is spelled out, and the comment this replaces predicted why:
    /// `ALTER ada SET …` reads as though there were one namespace of alterable
    /// things, and a table becoming alterable is exactly the case that would
    /// have made that reading wrong everywhere it was already written.
    pub(super) fn alter_statement(&mut self) -> Result<StatementKind> {
        self.advance();
        if self.eat_word("group") {
            return self.alter_group();
        }
        // ADR-0098, Q-892. One clause of a member row per statement, amended in
        // place: declaring a bound row again would tombstone its node.
        if self.eat_word("replica") {
            let name = self.name()?;
            let change = self.replica_change()?;
            return Ok(StatementKind::AlterReplica { name, change });
        }
        if self.eat_keyword(Keyword::Table) {
            let table = self.table_ref()?;
            // The columnar spellings first, because `SET` is the one that reads
            // as a whole-table change and the three field verbs read as changes
            // to something inside it.
            if self.eat_word("add") {
                self.expect_keyword(Keyword::Field, "`FIELD` and the field to add")?;
                let name = self.declared_field_name()?;
                return self.field_declaration(name, table, false);
            }
            if self.eat_keyword(Keyword::Alter) {
                self.expect_keyword(Keyword::Field, "`FIELD` and the field to change")?;
                let name = self.declared_field_name()?;
                return self.field_declaration(name, table, true);
            }
            if self.eat_keyword(Keyword::Drop) {
                self.expect_keyword(Keyword::Field, "`FIELD` and the field to remove")?;
                return Ok(StatementKind::DropField {
                    name: self.declared_field_name()?,
                    table,
                });
            }
            // ADR-0095. `split` and `at` contextual, as in `DEFINE TABLE`; a
            // point is a literal for the reason `split_points` gives.
            if self.eat_word("split") {
                self.expect_word("at", "`AT` and the identity a new shard begins at")?;
                return Ok(StatementKind::AlterTable {
                    table,
                    change: TableChange::Split(self.split_points()?),
                });
            }
            if self.eat_keyword(Keyword::Merge) {
                self.expect_word("shard", "`SHARD` and the two shards to merge")?;
                let first = self.shard_number()?;
                self.expect_punct(Punct::Comma, "`,` and the shard beside it")?;
                let second = self.shard_number()?;
                return Ok(StatementKind::AlterTable {
                    table,
                    change: TableChange::MergeShards(first, second),
                });
            }
            self.expect_keyword(
                Keyword::Set,
                "`SET`, `ADD FIELD`, `ALTER FIELD`, `DROP FIELD`, `SPLIT AT` or `MERGE SHARD`",
            )?;
            let change = if self.eat_keyword(Keyword::Schemafull) {
                TableChange::Schemafull
            } else if self.eat_keyword(Keyword::Schemaless) {
                TableChange::Schemaless
            } else {
                return Err(self.error_here("`SCHEMAFULL` or `SCHEMALESS`"));
            };
            return Ok(StatementKind::AlterTable { table, change });
        }
        if self.eat_keyword(Keyword::Namespace) {
            let name = self.name()?;
            let change = if let Some(replication) = self.replication_clause()? {
                NamespaceChange::Replication(replication)
            } else if let Some(acknowledge) = self.acknowledgement_clause()? {
                NamespaceChange::Acknowledge(acknowledge)
            } else {
                return Err(self.error_here("`REPLICATION` or `ACKNOWLEDGE` and the policy to set"));
            };
            return Ok(StatementKind::AlterNamespace { name, change });
        }
        if !self.eat_keyword(Keyword::User) {
            return Err(self
                .error_here("`NAMESPACE`, `USER`, `TABLE` or `REPLICA` and the thing to change"));
        }
        let name = self.name()?;
        self.expect_keyword(Keyword::Set, "`SET` and the one thing to change")?;
        let change = match self.peek_keyword() {
            Some(Keyword::Password) => {
                self.advance();
                let (password, _) = self.text("the new password, as text")?;
                UserChange::Password(Password::new(password))
            }
            Some(Keyword::Role) => {
                self.advance();
                UserChange::Role(self.name()?)
            }
            _ => return Err(self.error_here("`PASSWORD` or `ROLE`")),
        };
        Ok(StatementKind::AlterUser { name, change })
    }

    /// `GRANT read, write ON orders TO ada` and its opposite.
    ///
    /// One function for both because they differ in two tokens and nothing else,
    /// and two nearly identical parsers is two places for the grammar to drift.
    /// `TO` and `FROM` rather than one word for both, because a reader should be
    /// able to tell which direction a statement goes without reading its verb
    /// twice.
    pub(super) fn grant_statement(&mut self, giving: bool) -> Result<StatementKind> {
        self.advance();
        let mut verbs = vec![self.word_or_name()?];
        while self.eat_punct(Punct::Comma) {
            verbs.push(self.word_or_name()?);
        }
        self.expect_keyword(Keyword::On, "`ON` and the table or reach")?;
        // A reach is keyword-led in all three spellings and a table name can
        // never be a keyword, so this decision is made by the grammar rather
        // than by looking anything up. That is the whole reason `STORE` was
        // reserved: deciding it any other way would silently widen a table
        // grant somebody already wrote.
        if let Some(reach) = self.reach_keyword()? {
            let kinds = verbs;
            if giving {
                self.expect_keyword(Keyword::To, "`TO` and the user")?;
                return Ok(StatementKind::GrantAuthority {
                    kinds,
                    reach,
                    user: self.name()?,
                });
            }
            self.expect_keyword(Keyword::From, "`FROM` and the user")?;
            return Ok(StatementKind::RevokeAuthority {
                kinds,
                reach,
                user: self.name()?,
            });
        }
        let table = self.table_ref()?;
        if giving {
            // `FIELDS` narrows what may be *read*. It sits where the same word
            // sits in `DEFINE INDEX … FIELDS`, because it names the same thing.
            let mut fields = Vec::new();
            if self.eat_keyword(Keyword::Fields) {
                fields.push(self.name()?);
                while self.eat_punct(Punct::Comma) {
                    fields.push(self.name()?);
                }
            }
            self.expect_keyword(Keyword::To, "`TO` and the user")?;
            return Ok(StatementKind::Grant {
                verbs,
                table,
                fields,
                user: self.name()?,
            });
        }
        self.expect_keyword(Keyword::From, "`FROM` and the user")?;
        Ok(StatementKind::Revoke {
            verbs,
            table,
            user: self.name()?,
        })
    }

    /// A reach, when the next token opens one, and nothing consumed when not.
    ///
    /// Returning `None` rather than erroring is what lets one `ON` serve both
    /// the table grant and the authority grant: the caller falls through to a
    /// table reference having consumed nothing.
    pub(super) fn reach_keyword(&mut self) -> Result<Option<ReachRef>> {
        match self.peek_keyword() {
            Some(Keyword::Store) => {
                self.advance();
                Ok(Some(ReachRef::Store))
            }
            Some(Keyword::Namespace) => {
                self.advance();
                Ok(Some(ReachRef::Namespace(self.name()?)))
            }
            Some(Keyword::Database) => {
                self.advance();
                Ok(Some(ReachRef::Database(self.table_ref()?)))
            }
            _ => Ok(None),
        }
    }

    /// `prod.shop.orders 2`, once `SHARD` has been read (G031).
    ///
    /// Fully qualified and never resolved against the session's `USE`: a
    /// subscription is a statement about the cluster, and a shard named relative
    /// to whatever the writing session happened to select would mean different
    /// shards in two scripts that read the same.
    pub(super) fn shard_reach(&mut self) -> Result<ReachRef> {
        let namespace = self.name()?;
        self.expect_punct(Punct::Dot, "`.` and the database")?;
        let database = self.name()?;
        self.expect_punct(Punct::Dot, "`.` and the split table")?;
        let table = self.name()?;
        let shard = self.shard_number()?;
        Ok(ReachRef::Shard {
            namespace,
            database,
            table,
            shard,
        })
    }

    /// A shard's number, as `INFO FOR TABLE` reports it.
    pub(super) fn shard_number(&mut self) -> Result<u32> {
        let expected = "the shard's number, as `INFO FOR TABLE` reports it";
        let Some(Token::Number(Number::Integer(shard))) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let shard = u32::try_from(*shard)
            .ok()
            .filter(|shard| *shard > 0)
            .ok_or_else(|| self.error_here(expected))?;
        self.advance();
        Ok(shard)
    }

    /// A reach in any of its spellings, including the bare `prod.orders`.
    ///
    /// The bare form is a **database** and has been since `DEFINE USER … ON
    /// prod.orders` existed. It is kept rather than deprecated because every
    /// statement already written says it.
    pub(super) fn reach_ref(&mut self) -> Result<ReachRef> {
        match self.reach_keyword()? {
            Some(reach) => Ok(reach),
            None => Ok(ReachRef::Database(self.table_ref()?)),
        }
    }

    /// `manage, read` — one or more authority kinds, as written.
    ///
    /// Words rather than names because every kind is a bare word, and the
    /// kind is checked where the store knows the set rather than here, so a
    /// misspelling is one error at one place.
    pub(super) fn kind_list(&mut self) -> Result<Vec<Name>> {
        let mut kinds = vec![self.word_or_name()?];
        while self.eat_punct(Punct::Comma) {
            kinds.push(self.word_or_name()?);
        }
        Ok(kinds)
    }
}
