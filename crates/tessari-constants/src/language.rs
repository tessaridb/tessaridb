/// How deep a view may be expanded before the read is refused.
///
/// Unit: view expansions along one chain.
///
/// A view names a read, and that read may name another view, so expansion
/// recurses and needs a floor to stop on. Eight, for two reasons that pull in
/// opposite directions and meet here: deep enough that no view written by hand
/// meets it — a view over a view over a view is already unusual — and shallow
/// enough that the refusal arrives before the parse and materialisation cost of
/// eight nested reads has been paid.
///
/// **A cycle is caught by this and gets no second mechanism.** Two views naming
/// each other cannot avoid the counter, and the refusal prints the chain it
/// followed, so the cycle is legible in the message. A dedicated cycle detector
/// would produce a better sentence for a case this already stops, and would then
/// have to be kept in step with it.
///
/// A value of this layer, like the ceiling on a held read: nothing in a stored
/// definition records it, so a view means the same thing on a node that changes
/// it.
pub const MAX_VIEW_DEPTH: usize = 8;

/// How deep a chain of events may run before the write that started it is
/// refused (ADR-0110 D5).
///
/// Unit: nested event runs. An event's own writes fire the events of the
/// tables they reach, its own included, so a cycle would otherwise recurse
/// until the stack ran out. Sixteen is far past any honest chain — an audit
/// row, a counter, a denormalised copy and a topic append are each one level —
/// and close enough that a cycle fails at once with its name rather than after
/// minutes of writes that are then thrown away.
pub const EVENT_DEPTH_LIMIT: u8 = 16;
