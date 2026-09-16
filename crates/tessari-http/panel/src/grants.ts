//! Giving and taking away what one account may reach.
//!
//! The third action S2.1 names, beside a role change and a removal, and the one
//! the console had no surface for. `GRANT` and `REVOKE` were in the language the
//! whole time; what was missing was a screen.
//!
//! # Two counterintuitive rules, both stated before the button
//!
//! The engine's own doc carries them and they are the reason this screen needs a
//! blast radius rather than a confirmation:
//!
//! 1. **A user's first table grant NARROWS them.** A user with no grants is
//!    governed by their role; a user with one reaches exactly what they were
//!    granted. So giving `read` on one table takes away everything else the role
//!    allowed — the opposite of what "grant" sounds like.
//! 2. **Taking away the LAST table grant WIDENS them**, back to their whole
//!    role, so the node refuses it. An operator running a revoke is thinking
//!    about narrowing, and this is the one revoke that does the reverse.
//!
//! Neither is discoverable from the form. Both are in the radius line.
//!
//! # Authorities are a different question from table grants
//!
//! One asks *which of my tables*, the other *how much of this store*, and the
//! language keeps them as separate statements for that reason. This screen keeps
//! them as one control with a reach chooser, because to the operator they are
//! one decision — who may do what, and how far.

import { valueOf } from "./api.js";
import { at, hide, say, trimmed, value } from "./dom.js";
import { lookup } from "./roster.js";
import { told } from "./session.js";

/** Where a grant reaches. `table` is the narrowing kind; the rest are authorities. */
const reachOf = (): string => value("grant-reach");

/** The statement this form describes, or `null` while it is incomplete. */
export function statement(): string | null {
  const who = trimmed("grant-who");
  const what = trimmed("grant-what");
  const reach = reachOf();
  const name = trimmed("grant-name");
  const giving = value("grant-direction") === "give";
  if (who === "" || what === "" || (reach !== "store" && name === "")) {
    return null;
  }
  if (reach === "table") {
    // A table grant is read inside the SELECTED namespace and database, and
    // every `/script` request is its own session — so a `USE` run anywhere else
    // does not carry here. Measured, not assumed: the first attempt answered
    // "no namespace selected". The form therefore takes the table in full and
    // the statement carries its own selection.
    const parts = name.split(".");
    if (parts.length !== 3) {
      return null;
    }
    const [namespaceOf, databaseOf, table] = parts as [string, string, string];
    const act = giving
      ? `GRANT ${what} ON ${table} TO ${who};`
      : `REVOKE ${what} ON ${table} FROM ${who};`;
    return `USE NAMESPACE ${namespaceOf}; USE DATABASE ${databaseOf}; ${act}`;
  }
  const target =
    reach === "store" ? "STORE" : `${reach === "namespace" ? "NAMESPACE" : "DATABASE"} ${name}`;
  return giving
    ? `GRANT ${what} ON ${target} TO ${who};`
    : `REVOKE ${what} ON ${target} FROM ${who};`;
}

/** Which field is still empty, named rather than left to be guessed. */
function missing(): string {
  if (trimmed("grant-who") === "") {
    return "a name is needed";
  }
  if (trimmed("grant-what") === "") {
    return "say what — read, write, manage, operate or replicate";
  }
  if (reachOf() === "table" && trimmed("grant-name").split(".").length !== 3) {
    return "name the table in full, as namespace.database.table";
  }
  return "name the table, namespace or database it is on";
}

/** What this will do, including the part the form cannot show. */
export function says(): string {
  const who = trimmed("grant-who");
  const what = trimmed("grant-what");
  const giving = value("grant-direction") === "give";
  const table = reachOf() === "table";
  const today = lookup(who);
  const standing =
    today === null
      ? ` This panel has not been told what ${who} reaches — press List to find out.`
      : ` Today ${who} is ${today.role} in ${today.reach}.`;

  if (table && giving) {
    return (
      `Gives ${who} ${what} on ${trimmed("grant-name")}. If they hold no table grant ` +
      `yet this NARROWS them: a user with grants reaches exactly what they were ` +
      `granted, and nothing else their role would have allowed.` +
      standing
    );
  }
  if (table) {
    return (
      `Takes ${what} on ${trimmed("grant-name")} away from ${who}. If it is their ` +
      `LAST table grant the node will refuse it — going from one grant to none ` +
      `widens them back to their whole role, which is the opposite of a revoke.` +
      standing
    );
  }
  const where = reachOf() === "store" ? "the whole store" : trimmed("grant-name");
  return giving
    ? `Gives ${who} ${what} over ${where}.${standing}`
    : `Takes ${what} over ${where} away from ${who}. An authority going to none ` +
        `leaves them holding nothing there, which the node allows.${standing}`;
}

function shape(): void {
  hide("grant-name-field", reachOf() === "store");
  say("grant-says", statement() === null ? missing() : says());
}

export function wire(): void {
  for (const field of [
    "grant-who",
    "grant-what",
    "grant-reach",
    "grant-name",
    "grant-direction",
    "grant-why",
  ]) {
    at(field).addEventListener("input", shape);
    at(field).addEventListener("change", shape);
  }

  at("grant-apply").addEventListener("click", async () => {
    const sending = statement();
    if (sending === null) {
      say("grant-status", missing(), true);
      return;
    }
    // The same rule the other two access-affecting screens hold to: refused at
    // the click, with the consequence named, rather than by a dead button.
    const why = trimmed("grant-why");
    if (why === "") {
      say("grant-status", "say why — this changes what somebody may reach", true);
      return;
    }
    say("grant-status", "running…");
    try {
      const answered = await valueOf(sending, "Access · grant", why);
      say("grant-status", answered !== null && answered.kind === "done" ? "done" : "");
    } catch (failure) {
      // The node's own words. The refusal on a last-grant revoke explains the
      // widening, and paraphrasing it would lose the remedy it names.
      say("grant-status", told(failure), true);
    }
  });

  shape();
}
