//! The kind each of a page of keys holds, for `SCAN … TYPE`.

use tessaridb::{RecordId, Value};

use crate::surface::commands::kind;
use crate::surface::procedures::Against;
use crate::surface::running::Failed;
use crate::surface::statement::Binder;
use crate::surface::values;

impl Against<'_> {
    /// What each of `keys` holds, as `TYPE` names it, read in one statement.
    pub(crate) async fn kinds(&mut self, keys: &[&RecordId]) -> Result<Vec<&'static str>, Failed> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let mut binder = Binder::default();
        let reads: Vec<String> = keys
            .iter()
            .map(|key| {
                format!(
                    "GET {}:{}",
                    self.space.name,
                    binder.bind(values::resume_at(key))
                )
            })
            .collect();
        let held = self
            .value(format!("RETURN [{}]", reads.join(", ")), binder)
            .await?;
        Ok(match held {
            Value::Array(held) => held.iter().map(kind).collect(),
            _ => Vec::new(),
        })
    }
}
