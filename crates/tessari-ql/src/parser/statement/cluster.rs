//! Nodes, failover, replicas and replication clauses.

mod replicas;

use core::num::NonZeroU32;

use super::Parser;
use tessari_types::{
    Acknowledge, Acknowledgement, Duration, Number, Replication, ReplicationClass, parse_uuid,
};

use crate::ast::{
    PoolKinds, Ratio, ReachRef, ReplicaChange, ReplicationChange, StatementKind, TopologyChange,
    TopologyRealm,
};
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

    /// Whether `CREATE` here makes a link token: the word `LINK` and the word
    /// `TOKEN`.
    ///
    /// Both are checked rather than reserved, for the reason `creates_a_join
    /// _token` checks `TOKEN`: a record write begins `CREATE <table>`, and a
    /// table called `link` followed by an id or a keyword is never these two
    /// words in this order — `CREATE link SET …` and `CREATE link:token` keep
    /// meaning the writes they have always meant.
    pub(super) fn creates_a_link_token(&self) -> bool {
        let at = |offset: usize| {
            self.tokens
                .get(self.position.saturating_add(offset))
                .map(|spanned| &spanned.token)
        };
        matches!(at(1), Some(Token::Ident(word)) if word.eq_ignore_ascii_case("link"))
            && matches!(at(2), Some(Token::Ident(word)) if word.eq_ignore_ascii_case("token"))
    }

    /// `CREATE LINK TOKEN FOR REPLICATION backup EXPIRES 10m` — the join-token
    /// pattern (ADR-0108 D9) carried across the cluster seam: a one-time token
    /// the remote cluster offers when it binds the named link.
    ///
    /// `EXPIRES` is required with no default, for the reason
    /// [`Self::create_join_token`] gives: a token nobody gave a life to would be
    /// a credential that binds a row for as long as nobody remembers it exists.
    pub(super) fn create_link_token(&mut self) -> Result<StatementKind> {
        self.advance();
        self.eat_word("link");
        self.eat_word("token");
        if !self.eat_word("for") || !self.eat_word("replication") {
            return Err(self.error_here("`REPLICATION` and the row the token binds"));
        }
        let replication = self.name()?;
        if !self.eat_word("expires") {
            return Err(self.error_here("`EXPIRES` and how long the token binds for"));
        }
        let expires = self.length("expires")?;
        Ok(StatementKind::CreateLinkToken {
            replication,
            expires,
        })
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

    /// `DEFINE BALANCER LOAD ABOVE <ratio> WITHIN <period> SPACING <period>`
    ///
    /// In this order, and all three. The relations between them (`ABOVE`
    /// above 1, `SPACING` at least one lease period, `WITHIN` at least the
    /// round's cadence) are checked where the policy is built, for the reason
    /// [`Self::define_failover`] keeps for the failover policy's relations:
    /// exactly one place knows the directions, and a copy in the parser would
    /// be a second answer that drifts. The one check that stays here is the
    /// ratio's size — a value not above 1 is a mistake in what was typed, and
    /// the statement is where the number is.
    pub(super) fn define_balancer(&mut self) -> Result<StatementKind> {
        self.expect_word("load", "`LOAD` after `BALANCER`")?;
        self.expect_word("above", "`ABOVE` and the load margin")?;
        let above = self.ratio()?;
        // `expect_word` rather than `period` for the clause names: the
        // refusal for a missing clause names the clause, the way the failover
        // statement's do, and a shared helper's fallback message would name
        // another clause's words instead.
        self.expect_word("within", "`WITHIN` and the window the rates span")?;
        let within = self.length("within")?;
        self.expect_word(
            "spacing",
            "`SPACING` and how soon a moved range may move again",
        )?;
        let spacing = self.length("spacing")?;
        Ok(StatementKind::DefineBalancer {
            above,
            within,
            spacing,
        })
    }

    /// The ratio after `ABOVE`, refused where the number is when it is not
    /// above 1 — a margin of one would move a placement on any noise, and a
    /// statement that asked for it could only be a half-written one.
    fn ratio(&mut self) -> Result<Ratio> {
        // The integer spelling lands as hundredths by integer multiplication —
        // no float is ever constructed for it, so the conversion question the
        // fixed-point spelling exists to close never arises.
        if let Some(Token::Number(Number::Integer(written))) = self.peek() {
            let scaled = written
                .checked_mul(100)
                .and_then(|scaled| u64::try_from(scaled).ok())
                .and_then(Ratio::of_hundredths);
            let Some(above) = scaled else {
                return Err(self
                    .error_here("a ratio above 1, like `1.5` — what stands here is not above 1"));
            };
            self.advance();
            return Ok(above);
        }
        let times = match self.peek() {
            Some(Token::Number(Number::Float(written))) => *written,
            _ => {
                return Err(self.error_here(
                    "`ABOVE` takes a ratio above 1, like `1.5` — what stands here is not above 1",
                ));
            }
        };
        let Some(above) = Ratio::nearest(times) else {
            return Err(self.error_here("a ratio above 1 — what was written is not above 1"));
        };
        self.advance();
        Ok(above)
    }

    /// `DEFINE POOL web WITH MAX CONCURRENT 10 [FOR DATABASE d] [FOR USER u]
    /// [FOR READS, WRITES, …]` — an admission limit as a namespace-scoped
    /// catalog row.
    ///
    /// `WITH MAX CONCURRENT n` is required: the limit is what a pool is. The
    /// classifier clauses follow in any order — at most one `FOR DATABASE`,
    /// at most one `FOR USER`, at most one `FOR` kind list — and the
    /// relations a misshapen classifier breaks (a limit of zero refuses
    /// everything the classifier names) are checked here, where the words
    /// are, the way the balancer's margin is.
    pub(super) fn define_pool(&mut self) -> Result<StatementKind> {
        let name = self.name()?;
        self.expect_word(
            "with",
            "`WITH MAX CONCURRENT` and the limit the pool exists for",
        )?;
        self.expect_word(
            "max",
            "`MAX CONCURRENT` and how much work the pool admits at once",
        )?;
        self.expect_word("concurrent", "`CONCURRENT` after `MAX`")?;
        let max_concurrent = self.pool_limit()?;
        let mut database = None;
        let mut user = None;
        let mut kinds = None;
        loop {
            if self.eat_word("for") {
                if self.eat_keyword(Keyword::Database) {
                    if database.is_some() {
                        return Err(
                            self.error_here("one `FOR DATABASE` — a pool classifies one database")
                        );
                    }
                    database = Some(self.name()?);
                } else if self.eat_keyword(Keyword::User) {
                    if user.is_some() {
                        return Err(self.error_here("one `FOR USER` — a pool classifies one user"));
                    }
                    user = Some(self.name()?);
                } else {
                    let mut read = PoolKinds {
                        reads: false,
                        writes: false,
                        definitions: false,
                    };
                    loop {
                        if self.eat_word("reads") {
                            read.reads = true;
                        } else if self.eat_word("writes") {
                            read.writes = true;
                        } else if self.eat_word("definitions") {
                            read.definitions = true;
                        } else {
                            return Err(
                                self.error_here("`READS`, `WRITES` or `DEFINITIONS` after `FOR`")
                            );
                        }
                        if !self.eat_punct(Punct::Comma) {
                            break;
                        }
                    }
                    if kinds.is_some() {
                        return Err(
                            self.error_here("one `FOR` kind list — the kinds one pool bounds")
                        );
                    }
                    kinds = Some(read);
                }
            } else {
                break;
            }
        }
        Ok(StatementKind::DefinePool {
            name,
            max_concurrent,
            database,
            user,
            kinds: kinds.unwrap_or(PoolKinds::ALL),
        })
    }

    /// The limit after `MAX CONCURRENT`, refused where the number is when it
    /// is not a limit — zero or negative would refuse everything the
    /// classifier names, a deny-list wearing a pool's clothes.
    fn pool_limit(&mut self) -> Result<u64> {
        let Some(Token::Number(Number::Integer(written))) = self.peek() else {
            return Err(self.error_here("a limit, like `10`"));
        };
        if *written <= 0 {
            return Err(self.error_here("a limit — what stands here is not a limit"));
        }
        let limit = u64::try_from(*written)
            .map_err(|_| self.error_here("a limit within what a count of running work holds"))?;
        self.advance();
        Ok(limit)
    }

    /// `DEFINE TOPOLOGY main REALM 'dc-a' [DOMAIN 'az-1', …] … [SURVIVES REALM LOSS]`
    ///
    /// One `REALM` clause per realm, at least one, each naming the realm as
    /// text — the same text a member row names with `REGION`, for the reason
    /// `REGION` is text: `dc-a` is a label, not an identifier. The claim is
    /// last and optional, and absent means the structure is declared with no
    /// claim made — silence is a third state, not the weakest claim.
    ///
    /// The structural checks (a realm named twice, a domain named twice, an
    /// empty name, no realms at all) are NOT made here. They are made by
    /// `Realm::stated` / `Topology::stated`, which is the only way to build
    /// the checked value, so the refusal names the direction and there is
    /// exactly one place that knows the directions — the same split
    /// [`Self::define_failover`] keeps for the policy's relations.
    pub(super) fn define_topology(&mut self) -> Result<StatementKind> {
        let name = self.name()?;
        let mut realms = vec![self.realm_clause()?];
        while self.peek_word("realm") {
            realms.push(self.realm_clause()?);
        }
        let survives_realm_loss = if self.eat_word("survives") {
            self.expect_word("realm", "`REALM` after `SURVIVES`")?;
            self.expect_word("loss", "`LOSS` after `SURVIVES REALM`")?;
            true
        } else {
            false
        };
        Ok(StatementKind::DefineTopology {
            name,
            realms,
            survives_realm_loss,
        })
    }

    /// One `REALM` clause of a topology statement — the name, and the
    /// `DOMAIN` list when one stands. Read for `DEFINE TOPOLOGY` and
    /// `ALTER TOPOLOGY … ADD REALM`, so the two cannot come to accept
    /// different spellings of one clause.
    pub(in crate::parser::statement) fn realm_clause(&mut self) -> Result<TopologyRealm> {
        if !self.eat_word("realm") {
            return Err(self.error_here("`REALM` and a realm's name — at least one"));
        }
        let name = self.text("the realm's name, as text")?.0;
        let domains = if self.eat_word("domain") {
            let mut named = vec![self.text("a domain's name, as text")?.0];
            while self.eat_punct(Punct::Comma) {
                named.push(self.text("a domain's name, as text")?.0);
            }
            named
        } else {
            Vec::new()
        };
        Ok(TopologyRealm { name, domains })
    }

    /// The one clause an `ALTER TOPOLOGY` changes, read by the readers
    /// `DEFINE TOPOLOGY` uses for the same clause (Q-892's rule, on the
    /// scheme the way it runs on a member row).
    pub(in crate::parser::statement) fn topology_change(&mut self) -> Result<TopologyChange> {
        if self.eat_word("add") {
            let TopologyRealm { name, domains } = self.realm_clause()?;
            return Ok(TopologyChange::AddRealm { name, domains });
        }
        // `DROP` is reserved (it leads the drop statements), so it arrives as
        // a keyword; `ADD` is contextual, read as a word. Both halves are
        // checked because the clause is spelled the same whichever token kind
        // the lexer hands over.
        if self.eat_word("drop") || self.eat_keyword(Keyword::Drop) {
            if !self.eat_word("realm") {
                return Err(self.error_here("`REALM` after `DROP`"));
            }
            return Ok(TopologyChange::DropRealm(
                self.text("the realm's name, as text")?.0,
            ));
        }
        Err(self.error_here("`ADD REALM` or `DROP REALM` — the one clause that changes"))
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
