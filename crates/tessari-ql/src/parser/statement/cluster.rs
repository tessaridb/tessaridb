//! Nodes, failover, replicas and replication clauses.

use core::num::NonZeroU32;

use super::Parser;
use tessari_types::{Duration, Number, Replication, ReplicationClass, parse_uuid};

use crate::ast::{ReachRef, StatementKind};
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct, Token};

impl Parser<'_> {
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
    /// `DEFINE FAILOVER AWARENESS 10s COLLECTION 10s ROUND 1s CAMPAIGN 1s LEASE 30s`
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
        Ok(StatementKind::DefineFailover {
            awareness,
            collection,
            round,
            campaign,
            lease,
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

    pub(super) fn define_node(&mut self) -> Result<StatementKind> {
        let roles = if self.eat_word("roles") {
            if self.eat_keyword(Keyword::None) {
                Some(Vec::new())
            } else {
                let mut named = vec![self.name()?];
                while self.eat_punct(Punct::Comma) {
                    named.push(self.name()?);
                }
                Some(named)
            }
        } else {
            None
        };
        let endpoints = if self.eat_word("endpoints") {
            let (first, _) = self.text("an endpoint, as text")?;
            let mut found = vec![first];
            while self.eat_punct(Punct::Comma) {
                let (endpoint, _) = self.text("an endpoint, as text")?;
                found.push(endpoint);
            }
            Some(found)
        } else {
            None
        };
        let retain = self.retained_records()?;
        if roles.is_none() && endpoints.is_none() && retain.is_none() {
            return Err(self.error_here("`ROLES`, `ENDPOINTS` or `RETAIN` and what to set"));
        }
        Ok(StatementKind::DefineNode {
            roles,
            endpoints,
            retain,
        })
    }

    /// `DEFINE REPLICA second AT 'host:9001' NODE '<id>' ROLES serving, writable`
    ///
    /// The endpoint is text rather than a name because a host and port is not an
    /// identifier, and it is stored as written: whether it resolves is a
    /// question for whoever dials it, and refusing an unreachable address here
    /// would make the statement's success depend on the network being up at the
    /// moment it ran.
    ///
    /// `ROLES` is optional and spelled exactly as `DEFINE NODE`'s is, because it
    /// is the same field on the same membership row (ADR-0018 §2) seen from the
    /// other side — one written about a peer, one about this node. Two spellings
    /// for one set of words would be two things to keep in step.
    ///
    /// Left out, the peer is declared with no roles, and a peer with no roles
    /// takes no writes. That is the safe absence: the operator who forgot the
    /// clause gets a refusal naming it, where the opposite default would send a
    /// write to a node nobody said could take one.
    ///
    /// # `NODE`, and what saying it turns the row into
    ///
    /// `NODE` binds the row to one node by the id that node gave itself. It is
    /// optional, and without it the statement means what it has always meant.
    /// With it, the row stops being a note about somewhere else and becomes the
    /// **desired role** of a named machine: the node whose own id this is reads
    /// the row's `ROLES` as what it is supposed to be, and reconciles what it
    /// actually holds toward it the next time it opens the store.
    ///
    /// The value is written as text and is the spelling `INFO FOR NODE` prints
    /// for `id` — thirty-two hex digits — because an operator binds a node by
    /// copying that field, and a clause that would not take what the answer
    /// gives is a clause with a conversion step nobody documented. The canonical
    /// hyphenated form is taken too, since one reader already accepts both and a
    /// second reader would disagree with the first eventually.
    ///
    /// A malformed id is refused **here**, where the span is, rather than stored
    /// and puzzled over later: a row naming a node nobody will ever be is
    /// indistinguishable, afterwards, from a row nobody bound.
    pub(super) fn define_replica(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        if !self.eat_word("at") {
            return Err(self.error_here("`AT` and where the peer is reached"));
        }
        let (endpoint, _) = self.text("the endpoint, as text")?;
        let node = if self.eat_word("node") {
            let (written, at) = self.text("the node's id, as text")?;
            // The same refusal a `uuid` literal gets, from the same reader, so
            // the two spellings of one value cannot come to disagree about which
            // texts are ids.
            let bytes = parse_uuid(&written).ok_or(Error::InvalidUuid {
                text: written.clone(),
                span: at,
            })?;
            Some(bytes)
        } else {
            None
        };
        let roles = if self.eat_word("roles") {
            let mut named = vec![self.name()?];
            while self.eat_punct(Punct::Comma) {
                named.push(self.name()?);
            }
            Some(named)
        } else {
            None
        };
        // Read with the same reader `DEFINE USER … ON` uses, so the reach a
        // subscription names and the reach a grant names cannot come to accept
        // different spellings. `STORE`, `NAMESPACE x` and `DATABASE x.y` only —
        // the bare `x.y` that `ON` also takes is not offered here, because after
        // `REPLICATES` a bare pair would sit where a table name could and this
        // clause has no history to keep.
        let replicates = if self.eat_word("replicates") {
            if self.eat_word("shard") {
                Some(self.shard_reach()?)
            } else {
                match self.reach_keyword()? {
                    Some(reach) => Some(reach),
                    None => {
                        return Err(self.error_here(
                            "`STORE`, `NAMESPACE`, `DATABASE` or `SHARD` after `REPLICATES`",
                        ));
                    }
                }
            }
        } else {
            None
        };
        // ADR-0082. The same reader as `REPLICATES`, less `STORE`: the store is
        // what every standing node already stands for, so a placement naming it
        // would carve the whole store out of itself.
        let leads = if self.eat_word("leads") {
            if self.eat_word("shard") {
                Some(self.shard_reach()?)
            } else {
                match self.reach_keyword()? {
                    Some(ReachRef::Store) | None => {
                        return Err(self.error_here(
                            "`NAMESPACE`, `DATABASE` or `SHARD` after `LEADS` \
                             (every standing node already stands for the store)",
                        ));
                    }
                    Some(reach) => Some(reach),
                }
            }
        } else {
            None
        };
        Ok(StatementKind::DefineReplica {
            name,
            endpoint,
            roles,
            node,
            replicates,
            leads,
            if_not_exists,
        })
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
