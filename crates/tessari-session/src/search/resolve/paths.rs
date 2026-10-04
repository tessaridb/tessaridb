use super::*;

/// The paths a set of expressions searches, and the ones it ranks by.
///
/// A ranked path is also a searched one — it needs the analyzer as well — so it
/// is recorded in both places rather than the caller having to remember that.
pub(super) fn searched_paths<'a>(
    expr: &'a Expr,
    into: &mut Vec<&'a Path>,
    ranked: &mut Vec<(&'a Path, &'a Expr)>,
    prefixed: &mut Vec<(&'a Path, &'a Expr, BinaryOp)>,
    phrased: &mut Vec<(&'a Path, &'a Expr)>,
) {
    match &expr.kind {
        ExprKind::Binary {
            op: BinaryOp::Matches,
            left,
            right,
        } => {
            if let ExprKind::Path(field) = &left.kind {
                into.push(&field.path);
                phrased.push((&field.path, right));
            }
        }
        ExprKind::Binary {
            op: op @ (BinaryOp::MatchesPrefix | BinaryOp::MatchesFuzzy | BinaryOp::MatchesInfix),
            left,
            right,
        } => {
            if let ExprKind::Path(field) = &left.kind {
                into.push(&field.path);
                prefixed.push((&field.path, right, *op));
            }
        }
        ExprKind::Call {
            function: Function::SearchScore | Function::SearchExplain,
            arguments,
            ..
        } => {
            let (Some(first), Some(second)) = (arguments.first(), arguments.get(1)) else {
                return;
            };
            if let ExprKind::Path(field) = &first.kind {
                into.push(&field.path);
                ranked.push((&field.path, second));
            }
        }
        // A highlight names a searched field and asks nothing of its own: the
        // query it marks against is whatever the rest of the statement asked of
        // that same path. So it is registered as searched — which is what
        // resolves the analyzer — and contributes no query.
        ExprKind::Call {
            function: Function::SearchHighlight,
            arguments,
            ..
        } => {
            if let Some(ExprKind::Path(field)) = arguments.first().map(|first| &first.kind) {
                into.push(&field.path);
            }
        }
        ExprKind::Call { arguments, .. } => {
            for argument in arguments {
                searched_paths(argument, into, ranked, prefixed, phrased);
            }
        }
        ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Binary { left, right, .. }
        | ExprKind::Arithmetic { left, right, .. } => {
            searched_paths(left, into, ranked, prefixed, phrased);
            searched_paths(right, into, ranked, prefixed, phrased);
        }
        ExprKind::Not(inner) | ExprKind::Negate(inner) => {
            searched_paths(inner, into, ranked, prefixed, phrased);
        }
        _ => {}
    }
}

/// `PrefixTooShort` for the first prefix typed shorter than the floor.
///
/// The **typed** spelling decides, which is the first of the alternatives — the
/// rule the `MATCHES PREFIX` loop above states.
pub(super) fn too_short(prefixes: &[&[String]], span: Span) -> Result<()> {
    for alternatives in prefixes {
        if let Some(prefix) = alternatives.first()
            && prefix.chars().count() < SEARCH_PREFIX_MINIMUM
        {
            return Err(Error::PrefixTooShort {
                prefix: prefix.clone(),
                minimum: SEARCH_PREFIX_MINIMUM,
                span,
            });
        }
    }
    Ok(())
}

/// The terms one starred word blends, the most-held first (ADR-0104 D4).
///
/// Every term beginning with either spelling, up to the examination ceiling,
/// ranked by how many records hold it and cut at the expansion cap by that rank
/// — never in dictionary order, which would keep whichever rare words sort first
/// and drop the common one being typed. Ties fall to the term, so the same
/// dictionary always blends the same terms.
pub(super) fn blended(
    transaction: &Transaction<'_>,
    index: &IndexDefinition,
    alternatives: &[String],
) -> Result<Blend> {
    let mut reached = std::collections::BTreeSet::new();
    for spelling in alternatives {
        let found =
            transaction.terms_with_prefix(index, spelling, SEARCH_PREFIX_SCORE_EXAMINATION_CAP)?;
        reached.extend(found.terms);
    }
    let mut ranked = Vec::with_capacity(reached.len());
    for term in reached {
        ranked.push((transaction.document_frequency(index, &term)?, term));
    }
    ranked.sort_by(|(left, one), (right, other)| right.cmp(left).then_with(|| one.cmp(other)));
    ranked.truncate(SEARCH_PREFIX_EXPANSION_CAP);
    Ok(Blend {
        prefix: alternatives.first().cloned().unwrap_or_default(),
        documents: ranked.first().map_or(0, |(held, _)| *held),
        weights: vec![1.0; ranked.len()],
        expansions: ranked.into_iter().map(|(_, term)| term).collect(),
    })
}
