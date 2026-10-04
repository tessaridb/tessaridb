//! Reading one TessariQL value written as text, without a store.

/// The value a piece of text denotes, when it denotes one by itself.
///
/// How a **supplied** value is written on every surface outside a script: the
/// CLI's `--param x=3`, and the `parameters` of an HTTP request body. TessariQL
/// rather than each surface's own notation, because there is one value syntax
/// here and the console already reads and writes it — what an answer prints
/// pastes back into the next statement, and `dec 12.34`, `2s` and
/// `datetime '…'` all say themselves.
///
/// Read **in isolation**, so it is a value or it is nothing: `1; DROP TABLE
/// users` is refused as a literal rather than smuggled in as a statement. A
/// read, a path or anything needing a record is likewise not a value here — an
/// argument that had to consult the store to say what it is would be a statement
/// wearing a value's clothes.
///
/// It lives in the facade because two surfaces need it and neither is above the
/// other (ADR-0012).
///
/// # Errors
///
/// Returns [`NotAValue`], naming the text that is not a value.
pub fn value_of(written: &str) -> core::result::Result<tessari_types::Value, NotAValue> {
    let refusal = || NotAValue {
        written: written.to_owned(),
    };
    constant(tessari_ql::parse_expression(written).map_err(|_| refusal())?).ok_or_else(refusal)
}

/// The value an expression denotes when it is made of literals alone — arrays
/// and objects of them included, so a batch of events is one value (G044 C12).
/// Anything that would have to be evaluated is not a value here.
fn constant(expression: tessari_ql::Expr) -> Option<tessari_types::Value> {
    match expression.kind {
        tessari_ql::ExprKind::Literal(value) => Some(value),
        tessari_ql::ExprKind::Array(items) => items
            .into_iter()
            .map(constant)
            .collect::<Option<Vec<_>>>()
            .map(tessari_types::Value::Array),
        tessari_ql::ExprKind::Object(fields) => fields
            .into_iter()
            .map(|field| constant(field.value).map(|value| (field.name.text, value)))
            .collect::<Option<std::collections::BTreeMap<_, _>>>()
            .map(tessari_types::Value::Object),
        _ => None,
    }
}

/// Text [`value_of`] could not read as a value on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotAValue {
    written: String,
}

impl NotAValue {
    /// The text that was refused.
    #[must_use]
    pub fn written(&self) -> &str {
        &self.written
    }
}

impl core::fmt::Display for NotAValue {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            formatter,
            "{:?} is not a value TessariQL can read on its own",
            self.written
        )
    }
}

impl std::error::Error for NotAValue {}
