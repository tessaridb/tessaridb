use super::*;

impl Session<'_> {
    /// Every record of the search that answers the query, ranked.
    pub(crate) fn rank_search(
        &self,
        transaction: &mut Transaction<'_>,
        resolved: &Resolved,
        (operator, text, span): (SearchOperator, &str, Span),
        condition: Option<&Expr>,
        noticed: &Noticed,
    ) -> Result<(Query, Ranking)> {
        let query = read_query(
            &resolved.analyzer,
            operator,
            text,
            &resolved.stopwords,
            span,
        )?;
        if query.is_empty() {
            return Ok((
                query,
                Ranking {
                    found: Vec::new(),
                    served: true,
                    from_postings: false,
                },
            ));
        }
        let mut served = true;
        let mut expansions: Vec<Vec<Option<BTreeSet<String>>>> = Vec::new();
        for member in &resolved.members {
            let mut per_probe = Vec::new();
            for probe in query.probes() {
                per_probe.push(expand(transaction, member, probe)?);
            }
            expansions.push(per_probe);
        }
        // One document frequency per scored word, blended over the terms that
        // answer it and summed over the members the read reaches.
        let probes = query.probes();
        let mut holding = Vec::with_capacity(query.scored.len());
        for probe in &query.scored {
            let at = probes.iter().position(|held| *held == probe).unwrap_or(0);
            let mut terms: BTreeSet<&String> = BTreeSet::new();
            for member in &expansions {
                if let Some(Some(found)) = member.get(at) {
                    terms.extend(found);
                }
            }
            let mut largest = 0_u64;
            for term in terms {
                let mut documents = 0_u64;
                for member in &resolved.members {
                    documents = documents
                        .saturating_add(transaction.document_frequency(&member.index, term)?);
                }
                largest = largest.max(documents);
            }
            holding.push((probe, total(largest)));
        }
        let collection = Collection {
            documents: total(
                resolved
                    .members
                    .iter()
                    .fold(0_u64, |sum, member| sum.saturating_add(member.documents)),
            ),
        };

        // The candidates of one read share their words, so each is analysed
        // once per read (Q-870).
        let mut memo = Memo::default();
        // Q-870: whether the postings can decide this read, member by member.
        let decidable = condition.is_none()
            && super::super::postings::decidable(&query)
            && indexes_agree(transaction, &resolved.members)?;
        let mut from_postings = decidable;
        let mut found = Vec::new();
        for (at, member) in resolved.members.iter().enumerate() {
            if decidable
                && !transaction.writes_in(
                    member.index.namespace,
                    member.index.database,
                    member.index.table,
                )
                && let Some(reached) = expansions[at].iter().cloned().collect::<Option<Vec<_>>>()
            {
                let terms: Vec<String> = reached
                    .iter()
                    .flatten()
                    .cloned()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                if let Some(held) = transaction.member_postings(&member.index, &terms)? {
                    let visible = self.visible_in(transaction, member.index.table)?;
                    for (id, score) in super::super::postings::ranked(
                        member,
                        &query,
                        &reached,
                        &held,
                        (collection, &holding),
                    ) {
                        let address = RecordAddress::new(
                            member.index.namespace,
                            member.index.database,
                            member.index.table,
                            id.clone(),
                        );
                        let Some(payload) = transaction.get(&address)? else {
                            continue;
                        };
                        found.push(Found {
                            member: at,
                            id,
                            record: self.record_of(&payload, &visible)?,
                            score,
                        });
                    }
                    continue;
                }
            }
            from_postings = false;
            let nominated = match groups_of(&query, &probes, &expansions[at]) {
                Some(groups) => transaction
                    .member_candidates(&member.index, &groups)?
                    .into_iter()
                    .collect::<Vec<_>>(),
                None => {
                    served = false;
                    transaction
                        .scan_table(
                            member.index.namespace,
                            member.index.database,
                            member.index.table,
                        )?
                        .into_iter()
                        .map(|(id, _)| id)
                        .collect()
                }
            };
            let visible = self.visible_in(transaction, member.index.table)?;
            for id in nominated {
                let address = RecordAddress::new(
                    member.index.namespace,
                    member.index.database,
                    member.index.table,
                    id.clone(),
                );
                let Some(payload) = transaction.get(&address)? else {
                    continue;
                };
                let record = self.record_of(&payload, &visible)?;
                let analysed: Vec<(Vec<String>, Vec<String>)> = member
                    .fields
                    .iter()
                    .map(|field| match field.path.resolve(&record) {
                        Some(Value::String(text)) => resolved.analyzer.analysed(text, &mut memo),
                        _ => (Vec::new(), Vec::new()),
                    })
                    .collect();
                let texts: Vec<Text<'_>> = analysed
                    .iter()
                    .map(|(terms, surfaces)| Text { terms, surfaces })
                    .collect();
                let fields: Vec<(&Answering, Text<'_>)> = member
                    .fields
                    .iter()
                    .zip(&texts)
                    .map(|(field, text)| (&field.answering, *text))
                    .collect();
                if !holds(&query, &fields) {
                    continue;
                }
                if let Some(condition) = condition {
                    let held = self.evaluate_in(
                        transaction,
                        condition,
                        Scope::searching(&record, &Searched::default())
                            .identified(&id)
                            .noticing(noticed),
                    )?;
                    if !boolean(&held, condition.span)? {
                        continue;
                    }
                }
                let scored: Vec<Scored<'_>> = member
                    .fields
                    .iter()
                    .zip(&texts)
                    .map(|(field, text)| Scored {
                        answering: &field.answering,
                        text: *text,
                        weight: field.weight,
                        average: field.average,
                    })
                    .collect();
                let score = bm25f(collection, &holding, &scored);
                found.push(Found {
                    member: at,
                    id,
                    record,
                    score,
                });
            }
        }
        found.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| {
                    resolved.members[left.member]
                        .table
                        .cmp(&resolved.members[right.member].table)
                })
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok((
            query,
            Ranking {
                found,
                served,
                from_postings,
            },
        ))
    }
}
