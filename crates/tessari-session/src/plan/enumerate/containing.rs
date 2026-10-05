use super::*;
use tessari_encoding::IndexValues;

impl Session<'_> {
    /// Offer a candidate on every containment index for each path a
    /// `CONTAINS <document>` conjunct of `condition` asks about (ADR-0116 D4).
    pub(super) fn offer_containing(
        &self,
        transaction: &mut Transaction<'_>,
        condition: &Expr,
        declared: &[IndexDefinition],
        offered: &mut Vec<Candidate>,
    ) -> Result<()> {
        // Once per path, as a region is: a second document on one field asks
        // about entries the first already walks, and the re-test checks both.
        let mut asked: BTreeSet<&Path> = BTreeSet::new();
        for (path, document) in containing(condition) {
            if !asked.insert(path) {
                continue;
            }
            let indexes = containing_indexes(declared, path);
            if indexes.is_empty() {
                continue;
            }
            // Evaluated once, here, like every other bound.
            let held = self.evaluate(transaction, document)?;
            let pairs: Vec<IndexValues> = tessari_types::containment::asked_pairs(&held)
                .iter()
                .map(|pair| IndexValues::of(pair))
                .collect();
            // Not a document, or one asking for nothing a pair can say — `{}`,
            // or only empty arrays and documents: every record holding a
            // document would be a candidate, and the scan answers it exactly.
            if pairs.is_empty() {
                continue;
            }
            for index in indexes {
                offered.push(Candidate {
                    served: Served::Containment(pairs.clone()),
                    index: index.clone(),
                    // How many records hold every pair is not knowable without
                    // walking them, which is the read itself.
                    rows: Rows::Unknown,
                    answers: None,
                });
            }
        }
        Ok(())
    }
}
