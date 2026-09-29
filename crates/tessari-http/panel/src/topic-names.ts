//! What the Topics screen may write into a statement.
//!
//! A topic, a group, a namespace and a database are grammar: the node cannot take
//! them as parameters, so this screen writes them into the text. That is safe
//! only behind a check narrower than the node's own lexer, and these are the same
//! two patterns every client applies (consumer contract 1.0, section 3). A name
//! that fails is refused here, before anything is sent, rather than quoted.
//!
//! Numbers and durations are grammar too — `AFTER 40`, `ACK DEADLINE 30s` — and
//! are checked for the same reason.

const NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;
const GROUP = /^[A-Za-z0-9_.:-]{1,128}$/;
const DURATION = /^[0-9]{1,9}(ms|s|m|h|d|w)$/;
const WHOLE = /^[0-9]{1,15}$/;

/** A namespace, database or topic name, or `null` when it is not one. */
export const aName = (text: string): string | null => (NAME.test(text) ? text : null);

/** A group name, or `null` when it is not one. */
export const aGroup = (text: string): string | null => (GROUP.test(text) ? text : null);

/** A duration the grammar reads, such as `30s` or `7d`, or `null`. */
export const aDuration = (text: string): string | null => (DURATION.test(text) ? text : null);

/** A whole number of at most fifteen digits, or `null`. */
export function aWhole(text: string): number | null {
  return WHOLE.test(text) ? Number(text) : null;
}

/**
 * The tenancy every topic statement starts with.
 *
 * Sent with each script rather than once: the console's statements travel over
 * HTTP, and each request is its own session.
 */
export const tenancy = (namespace: string, database: string): string =>
  `USE NAMESPACE ${namespace}; USE DATABASE ${database}; `;
