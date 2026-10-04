use super::*;

impl Parser<'_> {
    pub(in crate::parser::statement) fn define_node(&mut self) -> Result<StatementKind> {
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
    pub(in crate::parser::statement) fn define_replica(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        if !self.eat_word("at") {
            return Err(self.error_here("`AT` and where the peer is reached"));
        }
        let (endpoint, _) = self.text("the endpoint, as text")?;
        // ADR-0101: where a client reaches the peer, beside where a peer does.
        // Optional and in this order, so every declaration written before them
        // reads exactly as it did.
        let clients = if self.eat_word("clients") {
            if !self.eat_word("at") {
                return Err(self.error_here("`AT` and where a client reaches the peer"));
            }
            Some(self.text("the client address, as text")?.0)
        } else {
            None
        };
        let http = if self.eat_word("http") {
            if !self.eat_word("at") {
                return Err(self.error_here("`AT` and the peer's HTTP base"));
            }
            Some(self.text("the HTTP base, as text")?.0)
        } else {
            None
        };
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
            Some(self.placed_range()?)
        } else {
            None
        };
        let preferred = leads.is_some() && self.eat_word("preferred");
        // ADR-0108 D9: the certificate that may bind this row, when it is not
        // bound by `NODE` and not to wait on a join token.
        let fingerprint = if self.eat_word("fingerprint") {
            Some(self.fingerprint()?)
        } else {
            None
        };
        // G057 C3: the region a `LOCAL MAJORITY` counts this peer in. Last, so
        // every declaration written before it reads as it did.
        let region = if self.eat_word("region") {
            Some(self.text("the region, as text")?.0)
        } else {
            None
        };
        Ok(StatementKind::DefineReplica {
            name,
            endpoint,
            clients,
            http,
            roles,
            node,
            replicates,
            leads,
            preferred,
            fingerprint,
            region,
            if_not_exists,
        })
    }

    /// The one clause an `ALTER REPLICA` changes, read by the readers
    /// `DEFINE REPLICA` uses for the same clause (Q-892).
    pub(in crate::parser::statement) fn replica_change(&mut self) -> Result<ReplicaChange> {
        if self.eat_word("leads") {
            if self.eat_keyword(Keyword::None) {
                return Ok(ReplicaChange::Leads {
                    range: None,
                    preferred: false,
                });
            }
            let range = Some(self.placed_range()?);
            return Ok(ReplicaChange::Leads {
                range,
                preferred: self.eat_word("preferred"),
            });
        }
        if self.eat_word("at") {
            return Ok(ReplicaChange::At(self.text("the endpoint, as text")?.0));
        }
        if self.eat_word("roles") {
            let mut named = vec![self.name()?];
            while self.eat_punct(Punct::Comma) {
                named.push(self.name()?);
            }
            return Ok(ReplicaChange::Roles(named));
        }
        if self.eat_word("clients") {
            return Ok(ReplicaChange::ClientsAt(self.address_or_none(
                "`AT` and where a client reaches the peer, or `NONE`",
                "the client address, as text",
            )?));
        }
        if self.eat_word("http") {
            return Ok(ReplicaChange::HttpAt(self.address_or_none(
                "`AT` and the peer's HTTP base, or `NONE`",
                "the HTTP base, as text",
            )?));
        }
        if self.eat_word("region") {
            if self.eat_keyword(Keyword::None) {
                return Ok(ReplicaChange::Region(None));
            }
            return Ok(ReplicaChange::Region(Some(
                self.text("the region, as text, or `NONE`")?.0,
            )));
        }
        Err(self.error_here(
            "`LEADS`, `AT`, `ROLES`, `CLIENTS AT`, `HTTP AT` or `REGION` — the one clause \
             that changes",
        ))
    }

    /// `AT '…'` or `NONE`, after `CLIENTS` or `HTTP`.
    fn address_or_none(
        &mut self,
        expected: &'static str,
        text: &'static str,
    ) -> Result<Option<String>> {
        if self.eat_keyword(Keyword::None) {
            return Ok(None);
        }
        if !self.eat_word("at") {
            return Err(self.error_here(expected));
        }
        Ok(Some(self.text(text)?.0))
    }

    /// The range after `LEADS`, read for `DEFINE REPLICA` and `ALTER REPLICA`.
    pub(in crate::parser::statement) fn placed_range(&mut self) -> Result<ReachRef> {
        if self.eat_word("shard") {
            return self.shard_reach();
        }
        match self.reach_keyword()? {
            Some(ReachRef::Store) | None => Err(self.error_here(
                "`NAMESPACE`, `DATABASE` or `SHARD` after `LEADS` \
                 (every standing node already stands for the store)",
            )),
            Some(reach) => Ok(reach),
        }
    }
}
