//! Nodes, failover, replicas and replication clauses.

mod replicas;

use core::num::NonZeroU32;

use super::Parser;
use tessari_types::{
    Acknowledge, Acknowledgement, Duration, Number, Replication, ReplicationClass, parse_uuid,
};

use crate::ast::{ReachRef, ReplicaChange, StatementKind};
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct, Token};

impl Parser<'_> {
    /// Whether `REVOKE` here takes a certificate rather than a grant: the word
    /// `CERTIFICATE` and then text.
    ///
    /// Both halves, because `certificate` is not reserved — a grant verb in
    /// that position is followed by `ON` or a comma, never by a string — so a
    /// table or verb of that name keeps meaning what it meant.
    pub(super) fn revokes_a_certificate(&self) -> bool {
        let at = |offset: usize| {
            self.tokens
                .get(self.position.saturating_add(offset))
                .map(|spanned| &spanned.token)
        };
        matches!(at(1), Some(Token::Ident(word)) if word.eq_ignore_ascii_case("certificate"))
            && matches!(at(2), Some(Token::Str(_)))
    }

    /// `REVOKE CERTIFICATE '<sha256>'` — a certificate no peer handshake
    /// accepts again, in either direction (ADR-0108 D6).
    ///
    /// The fingerprint is the SHA-256 of the certificate's DER, as 64
    /// hexadecimal digits; the colon-separated form a certificate tool prints
    /// is taken too, and either is stored in lowercase so one certificate has
    /// one spelling. Anything else is refused here, where the span is, rather
    /// than stored as a fingerprint no certificate can have.
    pub(super) fn revoke_certificate(&mut self) -> Result<StatementKind> {
        self.advance();
        self.eat_word("certificate");
        Ok(StatementKind::RevokeCertificate {
            fingerprint: self.fingerprint()?,
        })
    }

    /// A certificate's SHA-256, as 64 hexadecimal digits or the colon-separated
    /// form a certificate tool prints, in lowercase.
    ///
    /// One reader for `REVOKE CERTIFICATE` and `DEFINE REPLICA … FINGERPRINT`,
    /// so a fingerprint copied from one is accepted by the other.
    fn fingerprint(&mut self) -> Result<String> {
        const FINGERPRINT: &str = "a certificate's SHA-256 fingerprint: 64 hexadecimal digits";
        let at = self.position;
        let (written, _) = self.text(FINGERPRINT)?;
        let digits: String = written.chars().filter(|found| *found != ':').collect();
        if digits.len() != 64 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(self.error_at(at, FINGERPRINT));
        }
        Ok(digits.to_ascii_lowercase())
    }

    /// Whether `CREATE` here makes a join token: `JOIN` and the word `TOKEN`.
    ///
    /// `JOIN` is reserved, so no table is called that and a record write can
    /// never begin `CREATE JOIN`; `TOKEN` is read as a word and checked too,
    /// so the refusal for anything else after `CREATE JOIN` stays the write's.
    pub(super) fn creates_a_join_token(&self) -> bool {
        let at = |offset: usize| {
            self.tokens
                .get(self.position.saturating_add(offset))
                .map(|spanned| &spanned.token)
        };
        matches!(at(1), Some(Token::Keyword(Keyword::Join)))
            && matches!(at(2), Some(Token::Ident(word)) if word.eq_ignore_ascii_case("token"))
    }

    /// `CREATE JOIN TOKEN FOR REPLICA r EXPIRES 10m` (ADR-0108 D9).
    ///
    /// `EXPIRES` is required with no default: a token nobody gave a life to
    /// would be a credential that binds a row for as long as nobody remembers
    /// it exists.
    pub(super) fn create_join_token(&mut self) -> Result<StatementKind> {
        self.advance();
        self.eat_keyword(Keyword::Join);
        self.eat_word("token");
        if !self.eat_word("for") || !self.eat_word("replica") {
            return Err(self.error_here("`REPLICA` and the row the token binds"));
        }
        let replica = self.name()?;
        if !self.eat_word("expires") {
            return Err(self.error_here("`EXPIRES` and how long the token binds for"));
        }
        let expires = self.length("expires")?;
        Ok(StatementKind::CreateJoinToken { replica, expires })
    }

    /// A contextual word this statement requires.
    /// `REPLICATION NONE` or `REPLICATION FACTOR 3`, when one stands here.
    ///
    /// `REPLICATION` and `FACTOR` are read as **contextual words** rather than
    /// added to the keyword table, which is not a shortcut: `DEFINE NAMESPACE`
    /// appears 333 times across this engine and four sibling repositories, and
    /// reserving a word retroactively refuses every script that used it as a
    /// name. Nothing here is ambiguous — a bare word after a namespace's name
    /// has no other reading — so the reservation would buy nothing and cost the
    /// corpus.
    ///
    /// Answers `None` when no clause stands here, which is what a namespace
    /// that said nothing is; see [`StatementKind::DefineNamespace`] for why
    /// that is not [`Replication::None`].
    pub(super) fn replication_clause(&mut self) -> Result<Option<Replication>> {
        if !self.eat_word("replication") {
            return Ok(None);
        }
        if self.eat_keyword(Keyword::None) {
            return Ok(Some(Replication::None));
        }
        self.expect_word("factor", "`NONE` or `FACTOR` and a count")?;
        // `whole_number` already refuses zero and says so at the author's own
        // span, which is the answer this clause needs: a factor of zero is not
        // a policy, it says the data is kept nowhere.
        let factor = NonZeroU32::new(self.whole_number("a replication factor of at least one")?)
            .ok_or_else(|| self.error_here("a replication factor of at least one"))?;
        Ok(Some(Replication::Factor(factor)))
    }

    /// `MULTI MASTER` or `SINGLE LEADER`, the clause that says how many writers
    /// a namespace admits (G027 S2.1).
    ///
    /// Four contextual words rather than four keywords, for
    /// [`Self::replication_clause`]'s reason and with more force: `MASTER`,
    /// `LEADER`, `SINGLE` and `MULTI` are ordinary English nouns that a corpus
    /// of 333 `DEFINE NAMESPACE` statements across this engine and four sibling
    /// repositories may well already use as names, and reserving one
    /// retroactively refuses every script that did. Two words rather than one
    /// because the phrase is what an operator already calls the thing, so the
    /// clause they type is the phrase `INFO FOR` will read back to them.
    ///
    /// Answers `None` when no clause stands here. That namespace said nothing,
    /// which reads as single-leader everywhere and is deliberately not
    /// [`ReplicationClass::SingleLeader`] — see
    /// [`StatementKind::DefineNamespace`].
    pub(super) fn replication_class_clause(&mut self) -> Result<Option<ReplicationClass>> {
        if self.eat_word("multi") {
            self.expect_word("master", "`MASTER` after `MULTI`")?;
            return Ok(Some(ReplicationClass::MultiMaster));
        }
        if self.eat_word("single") {
            self.expect_word("leader", "`LEADER` after `SINGLE`")?;
            return Ok(Some(ReplicationClass::SingleLeader));
        }
        Ok(None)
    }

    /// `ACKNOWLEDGE LEADER`, `ACKNOWLEDGE LOCAL MAJORITY` or
    /// `ACKNOWLEDGE MAJORITY`, when one stands here
    /// (ADR-0106 D2) — the level a write, or the `COMMIT` of several, asks for
    /// itself.
    ///
    /// Contextual words for [`Self::replication_clause`]'s reason: each is an
    /// ordinary noun a schema may already use as a name.
    pub(in crate::parser) fn acknowledge_clause(&mut self) -> Result<Option<Acknowledge>> {
        if !self.eat_word("acknowledge") {
            return Ok(None);
        }
        if self.eat_word("leader") {
            return Ok(Some(Acknowledge::Leader));
        }
        if self.eat_word("local") {
            self.expect_word("majority", "`MAJORITY` after `LOCAL`")?;
            return Ok(Some(Acknowledge::LocalMajority));
        }
        self.expect_word("majority", "`LEADER`, `LOCAL MAJORITY` or `MAJORITY`")?;
        Ok(Some(Acknowledge::Majority))
    }

    /// A namespace's acknowledgement: the level, and `OR WEAKER` when a request
    /// may ask for less. Only a namespace says `OR WEAKER` — it is the
    /// operator's to grant, never a request's to claim.
    pub(super) fn acknowledgement_clause(&mut self) -> Result<Option<Acknowledgement>> {
        let Some(level) = self.acknowledge_clause()? else {
            return Ok(None);
        };
        let or_weaker = self.eat_keyword(Keyword::Or);
        if or_weaker {
            self.expect_word("weaker", "`WEAKER` after `OR`")?;
        }
        Ok(Some(Acknowledgement { level, or_weaker }))
    }

    pub(in crate::parser) fn expect_word(
        &mut self,
        word: &str,
        expected: &'static str,
    ) -> Result<()> {
        if self.eat_word(word) {
            return Ok(());
        }
        Err(self.error_here(expected))
    }

    /// `DEFINE NODE ROLES serving, writable ENDPOINTS 'host:9000'`
    ///
    /// Either clause, in that order, and at least one of the two. A statement
    /// naming neither is refused rather than accepted as a no-op: it can only be
    /// a half-written one, and quietly succeeding is how an operator comes to
    /// believe a node was configured.
    ///
    /// What a clause names **replaces** what was there, and a clause left out
    /// leaves its field alone. So `DEFINE NODE ENDPOINTS …` is not a silent way
    /// to drop the roles.
    ///
    /// `ROLES NONE` clears them, and needs a spelling of its own precisely
    /// because absence is taken here. `NONE` is a whole answer rather than a
    /// member of the list, and `DEFINE REPLICA` deliberately does not take it —
    /// a peer is declared rather than amended, so an absent clause already
    /// clears there. The reasoning is in the specification, § *Draining this
    /// node*, and is not restated here: two copies of one argument drift, and
    /// the document is the one a reader of the language actually opens.
    /// `DEFINE FAILOVER AWARENESS 10s COLLECTION 10s ROUND 1s CAMPAIGN 1s LEASE 30s
    /// [BALANCE LEADERSHIPS]`
    ///
    /// **In this order, and all five.** A fixed order rather than clauses in any
    /// arrangement, because these five are read together as a set — four
    /// relations hold between them — and a reader comparing two policies in a
    /// log or a report compares them line by line. Free order would make two
    /// spellings of one policy that a human eye cannot diff.
    ///
    /// Each one is required, which is the difference from [`Self::define_node`]:
    /// that statement amends a row and an absent clause leaves its field alone,
    /// while this one replaces a checked set. A statement naming three periods
    /// could only mix new values with old ones under a single version, or
    /// perform a read-modify-write nobody can see in what they typed.
    ///
    /// The relations themselves are NOT checked here. They are checked where the
    /// policy is built, by `Failover::stated`, which is the only way to make one
    /// that is not the default — so the refusal names the direction the value is
    /// wrong in, and there is exactly one place that knows those directions. A
    /// copy of them in the parser would be a second answer that drifts.
    pub(super) fn define_failover(&mut self) -> Result<StatementKind> {
        let awareness = self.period("awareness")?;
        let collection = self.period("collection")?;
        let round = self.period("round")?;
        let campaign = self.period("campaign")?;
        let lease = self.period("lease")?;
        let balance_leaderships = self.eat_word("balance");
        if balance_leaderships {
            self.expect_word("leaderships", "`LEADERSHIPS` after `BALANCE`")?;
        }
        Ok(StatementKind::DefineFailover {
            awareness,
            collection,
            round,
            campaign,
            lease,
            balance_leaderships,
        })
    }

    /// One named period of a failover policy, refused with its own word.
    ///
    /// The clause word is in the error rather than a generic *a duration*,
    /// because five clauses in a fixed order means the operator's mistake is
    /// almost always *which one did I leave out* — and an error that cannot say
    /// leaves them counting durations.
    ///
    /// A period of no length is refused here rather than at `Failover::stated`
    /// for the reason the queue timeout gives about its own zero: it is a
    /// mistake in the statement, and the statement is where the span is.
    pub(super) fn period(&mut self, clause: &'static str) -> Result<Duration> {
        // Four of the five clause words are contextual identifiers, on the
        // reasoning `NODE` and `REPLICA` are read that way. `COLLECTION` is the
        // exception because the language already reserved it for
        // `DEFINE COLLECTION`, so it arrives as a keyword and has to be eaten as
        // one.
        //
        // The clause keeps the name anyway. Calling it something else here to
        // dodge one token kind would give the same field two spellings — one in
        // the statement, one in the row and the report — which is the drift this
        // codebase has already paid for twice. The collision is only syntactic:
        // nothing but a period clause can stand at this position.
        let taken = if clause == "collection" {
            self.eat_keyword(Keyword::Collection)
        } else {
            self.eat_word(clause)
        };
        if !taken {
            return Err(self.error_here(match clause {
                "awareness" => "`AWARENESS` and how often this node refreshes what it knows",
                "collection" => "`COLLECTION` and how long a follower waits between collecting",
                "round" => "`ROUND` and how long one election round may take",
                "campaign" => "`CAMPAIGN` and how often a node checks whether to stand",
                _ => "`LEASE` and how long a granted leadership is held",
            }));
        }
        self.length(clause)
    }

    /// The duration after a clause word, refused when it has no length.
    pub(super) fn length(&mut self, clause: &'static str) -> Result<Duration> {
        let Some(Token::Duration(written)) = self.peek() else {
            return Err(self.error_here("a duration, like `10s` or `1m`"));
        };
        let period = *written;
        let at = self.span_here();
        self.advance();
        if period.seconds() < 0 || (period.seconds() == 0 && period.nanos() == 0) {
            return Err(Error::EmptyPeriod {
                clause,
                written: period.to_literal(),
                span: at,
            });
        }
        Ok(period)
    }

    /// A count standing where one is required.
    ///
    /// Refused rather than clamped when it does not fit or is not positive: a
    /// clamped count is a statement that ran as something other than what it
    /// says, which is the class of bug this grammar spends refusals to avoid.
    pub(super) fn whole_number(&mut self, expected: &'static str) -> Result<u32> {
        let Some(Token::Number(tessari_types::Number::Integer(held))) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let held = u32::try_from(*held).map_err(|_| self.error_here(expected))?;
        if held == 0 {
            return Err(self.error_here(expected));
        }
        self.advance();
        Ok(held)
    }

    /// `RETAIN 100000 RECORDS` or `RETAIN NONE` after `DEFINE NODE`, when it is
    /// there.
    ///
    /// The outer `Option` is *was the clause written*, and the inner one is
    /// *what it said*, which is the shape every amending clause on this
    /// statement has: absent means leave the setting alone, and `NONE` means put
    /// it back to unbounded.
    ///
    /// # Why `RECORDS` is required and why there is no other unit
    ///
    /// A bare number would leave the reader to guess between records, bytes and
    /// a duration, and the three have different failure modes — only one of them
    /// is a count this store can enforce exactly, because a log position IS a
    /// record count. Bytes and ages are both derived quantities here and would
    /// have to be approximated; a clause that says `RECORDS` cannot be silently
    /// re-read as either.
    ///
    /// # Why zero is refused rather than clamped
    ///
    /// `RETAIN 0 RECORDS` reads as *keep nothing*, and keeping nothing is the
    /// one setting that must not be expressible: a level follower is served by
    /// reading the record BEFORE the position it asks for, so a log with no
    /// records left cannot answer a follower that is perfectly healthy. The
    /// store clamps anyway — the last record always survives — but a statement
    /// that runs as something other than what it says is the class of bug this
    /// grammar spends refusals to avoid.
    pub(super) fn retained_records(&mut self) -> Result<Option<Option<u64>>> {
        if !self.eat_word("retain") {
            return Ok(None);
        }
        if self.eat_keyword(Keyword::None) {
            return Ok(Some(None));
        }
        let expected = "`RETAIN n RECORDS`, or `RETAIN NONE` to keep the whole log";
        let Some(Token::Number(Number::Integer(held))) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let held = u64::try_from(*held).unwrap_or(0);
        if held == 0 {
            return Err(self.error_here(expected));
        }
        self.advance();
        if !self.eat_word("records") {
            return Err(self.error_here(expected));
        }
        Ok(Some(Some(held)))
    }

    /// `MAX 5242880` after a bucket's name, when it is there.
    ///
    /// A count of **bytes**, written out. `5MB` is not a spelling this grammar
    /// has: digits touching a letter are a duration whatever the letter is
    /// (see the lexer), so `5MB` would be a duration with an unrecognised unit
    /// and refused. Giving the clause a shorter spelling means changing that
    /// rule for every literal in the language, which is a large change bought
    /// for a small convenience.
    ///
    /// A zero refuses rather than clamping, for the reason `DEPTH 0` does: a
    /// bucket that accepts no file is not a bucket with a ceiling, it is a
    /// table nothing can be written to, and a caller who wrote `MAX 0` meant
    /// something else.
    pub(super) fn byte_ceiling(&mut self) -> Result<Option<u64>> {
        if !self.eat_word("max") {
            return Ok(None);
        }
        let expected = "`MAX n` — the largest file the bucket takes, in bytes";
        let Some(Token::Number(Number::Integer(held))) = self.peek() else {
            return Err(self.error_here(expected));
        };
        // A negative and a zero refuse as the same thing, which they are: both
        // say the bucket admits no file at all.
        let held = u64::try_from(*held).unwrap_or(0);
        if held == 0 {
            return Err(self.error_here(expected));
        }
        self.advance();
        Ok(Some(held))
    }
}
