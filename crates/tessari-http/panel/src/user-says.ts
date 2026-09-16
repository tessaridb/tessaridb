//! What a button will do, said in the reader's language.
//!
//! Three sentences and nothing else. They take what they describe as ARGUMENTS
//! rather than reading the form, which is what lets them be read — and one day
//! tested — without a form existing at all. `user-forms.ts` reads the controls
//! and calls these; the split is along that line and not an arbitrary one.
//!
//! The rule every sentence here obeys: **say what the statement changes and
//! stop.** A consequence the engine has not been asked about is the panel
//! inventing an answer, which is the same defect as drawing a metric it does not
//! have. Where a sentence does claim a consequence — that a session ends — the
//! claim is backed by a test in the suite and not by reasoning from the
//! mechanism.

import { lookup } from "./roster.js";

/**
 * What pressing the button will do, in words.
 *
 * It says what is created and where, and stops. What the new account will then
 * be able to reach is the role's business and is already said beside the role
 * control — repeating it here would be the pane explaining the same thing twice
 * and drifting on one of them.
 */
export function definitionSays(name: string, role: string, space: string): string {
  return (
    "Creates " +
    name +
    " as " +
    role +
    (space === "" ? " of the whole node — an administrator" : " in " + space) +
    ", with the password typed above."
  );
}

/**
 * What this change will do to the person it is about.
 *
 * Both branches say the session ends, and that is measured rather than assumed:
 * a token stands for the account record it was issued against, so a node that
 * finds the record has moved refuses it and drops it. Whether a *promotion* does
 * the same as a demotion is asserted by the probe in the suite, not by this
 * sentence — the pane says what is proven elsewhere and invents nothing.
 */
export function alterationSays(name: string, what: "password" | "role", role: string): string {
  const today = lookup(name);
  const standing =
    today === null
      ? " This panel has not been told what " + name + " reaches — press List to find out."
      : " Today " + name + " is " + today.role + " in " + today.reach + ", and that is unchanged.";
  if (what === "password") {
    return (
      "Sets a new password for " +
      name +
      ". The one they have stops working and any session they are holding ends, " +
      "so they sign in again with the new one." +
      standing
    );
  }
  return (
    "Makes " +
    name +
    " " +
    role +
    ". Any session they are holding ends, so they sign in again." +
    standing
  );
}

/**
 * Who loses what, said before the statement rather than after it.
 *
 * Drawn from the listing the node already answered and from nowhere else. When
 * this panel has not been told, it says so: a reach guessed from a name would be
 * the console narrating an answer the node never gave, and the one place that is
 * least affordable is the pane that does not come back.
 */
export function removalSays(name: string): string {
  const today = lookup(name);
  if (today === null) {
    return (
      name +
      " loses access entirely, and their grants go with them. This panel has not been" +
      " told what " +
      name +
      " reaches — press List above to find out before you do this."
    );
  }
  return (
    name +
    " loses access entirely: " +
    today.role +
    " in " +
    today.reach +
    ", and the grants go with them. A new user of the same name inherits none of it."
  );
}
