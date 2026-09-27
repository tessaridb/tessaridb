//! Vault statements: reveal, recipients, unseal and the vault declaration.

use super::Parser;

use crate::ast::StatementKind;
use crate::error::Result;
use crate::token::{Keyword, Punct, Span, Token};

impl Parser<'_> {
    /// `REVEAL password FROM team:github` · `REVEAL * FROM team:github`
    ///
    /// A field list or `*`, then one record. There is no `WHERE` and no `ORDER
    /// BY`, and their absence is the feature: a filter over a secret is an
    /// oracle answering one bit per statement, and a verb with nowhere to put
    /// one cannot be talked into accepting one later by a clause somebody adds
    /// for a different reason.
    pub(super) fn reveal_statement(&mut self, start: Span) -> Result<StatementKind> {
        let mut fields = Vec::new();
        if !self.eat_punct(Punct::Star) {
            loop {
                // The same reader the declaration uses. A field that can only be
                // declared by quoting its name has to be openable by quoting it
                // too, or `REVEAL` is the one statement that cannot name what
                // `DEFINE FIELD` just created.
                fields.push(self.declared_field_name()?);
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
        }
        self.expect_keyword(Keyword::From, "`FROM` and the record to open")?;
        let target = self.record_target()?;
        Ok(StatementKind::Reveal {
            target,
            fields,
            span: start.to(self.span_behind()),
        })
    }

    /// `ADD RECIPIENT 'ops-escrow' TO team:github KEY $wrapped`
    ///
    /// The material is an **expression** where the passphrase below is a bare
    /// literal, and the difference is deliberate rather than inconsistent. A
    /// passphrase is a secret this store must never let through the evaluator; a
    /// recipient's material is the caller's own ciphertext, and the caller that
    /// computed it client-side needs to hand it over as a parameter rather than
    /// format it into a statement as hex.
    pub(super) fn add_recipient_statement(&mut self, start: Span) -> Result<StatementKind> {
        self.expect_word("recipient", "`RECIPIENT`")?;
        let recipient = self.expression()?;
        self.expect_keyword(Keyword::To, "`TO` and the record")?;
        let target = self.record_target()?;
        self.expect_word("key", "`KEY` and the material to store")?;
        let material = self.expression()?;
        Ok(StatementKind::AddRecipient {
            target,
            recipient,
            material,
            span: start.to(self.span_behind()),
        })
    }

    /// `REMOVE RECIPIENT 'ops-escrow' FROM team:github`
    pub(super) fn remove_recipient_statement(&mut self, start: Span) -> Result<StatementKind> {
        self.expect_word("recipient", "`RECIPIENT`")?;
        let recipient = self.expression()?;
        self.expect_keyword(Keyword::From, "`FROM` and the record")?;
        let target = self.record_target()?;
        Ok(StatementKind::RemoveRecipient {
            target,
            recipient,
            span: start.to(self.span_behind()),
        })
    }

    /// `UNSEAL VAULT WITH '…'`
    ///
    /// The passphrase is a **string literal** and nothing else — not an
    /// expression, not a parameter, not a name. An expression here would put a
    /// secret through the evaluator, where it could be concatenated into a
    /// message, compared with `=`, or returned by the very statement that read
    /// it; a literal goes from the lexer to the key derivation and nowhere else.
    pub(super) fn unseal_statement(&mut self, start: Span) -> Result<StatementKind> {
        self.expect_vault_word("`VAULT`")?;
        if !self.eat_word("with") {
            return Err(self.error_here("`WITH` and the passphrase"));
        }
        let Some(Token::Str(passphrase)) = self.peek() else {
            // The expectation names the shape and never what stands there. Every
            // other parse error in this file quotes the token it found, and this
            // is the one position where the token is the secret.
            return Err(self.error_here("a quoted passphrase"));
        };
        let passphrase = passphrase.clone();
        self.advance();
        Ok(StatementKind::UnsealVault {
            passphrase,
            span: start.to(self.span_behind()),
        })
    }

    /// The word `VAULT` after `SEAL` or `UNSEAL`, which is contextual too.
    pub(super) fn expect_vault_word(&mut self, expected: &'static str) -> Result<()> {
        if self.eat_word("vault") {
            return Ok(());
        }
        Err(self.error_here(expected))
    }

    /// `DEFINE VAULT team`
    ///
    /// A name and nothing else, for the reason `DEFINE GEO` gives: what makes a
    /// vault a vault is a key, and a key is not a clause a caller writes. The
    /// statement mints one, which is why this is the one declaration that
    /// requires the store to be unsealed — enforced where the keyring is, not
    /// here.
    pub(super) fn define_vault(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        Ok(StatementKind::DefineVault {
            name: self.name()?,
            if_not_exists,
        })
    }
}
