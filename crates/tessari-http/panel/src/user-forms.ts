//! The three user forms: what they describe, and what they show while typing.
//!
//! Only the statements and the previews live here. The buttons that RUN them
//! are in `users.ts`, beside the listing they have to redraw — which keeps the
//! two modules pointing one way instead of at each other.

import { at, disable, hide, trimmed, value, write } from "./dom.js";

/** Text into a TessariQL string literal, escaped the way the language escapes. */
export function quoted(text: string): string {
  let out = "'";
  for (const character of text) {
    if (character === "'") {
      out += "\\'";
    } else if (character === "\\") {
      out += "\\\\";
    } else if (character === "\n") {
      out += "\\n";
    } else if (character === "\r") {
      out += "\\r";
    } else if (character === "\t") {
      out += "\\t";
    } else {
      out += character;
    }
  }
  return out + "'";
}

/** What each role means, said beside the control rather than inside it. */
const MEANS: Readonly<Record<string, string>> = {
  viewer: "reads what the space holds, and nothing else.",
  editor: "reads and writes records, and declares structure.",
  owner: "everything in the space, users included.",
  other:
    "a role this build may not know. It will be sent as typed, and the node's" +
    " refusal is what you will see if it does not exist.",
};

/**
 * The tenancy the form describes: a space, or none at all.
 *
 * `null` means the form is asking for a space and has not been given one. It is
 * distinct from `""`, which means the whole node — and conflating the two is how
 * an empty field silently produces an administrator of everything. That is the
 * exact failure this form was split in two to prevent, and it happened here
 * before this returned three answers instead of two.
 */
function reach(): string | null {
  if (value("new-reach") === "node") {
    return "";
  }
  const space = trimmed("new-scope");
  return space === "" ? null : space;
}

/** The role the form describes, which may be one this build has never heard of. */
function role(): string {
  const chosen = value("new-role");
  return chosen === "other" ? trimmed("new-role-other") : chosen;
}

/** The statement the form describes, shown before it is run and never after. */
export function definition(): string | null {
  const name = trimmed("new-name");
  const named = role();
  const space = reach();
  if (name === "" || named === "" || space === null) {
    return null;
  }
  return (
    "DEFINE USER " +
    name +
    (space === "" ? "" : " ON " + space) +
    " ROLE " +
    named +
    " PASSWORD " +
    quoted(value("new-password")) +
    ";"
  );
}

/**
 * A statement with its password drawn as the store draws it.
 *
 * The preview is the one place a password would appear in plain view on
 * somebody's screen, and a shoulder is a threat this page can actually do
 * something about. The statement that runs carries the real one.
 */
const redacted = (statement: string): string =>
  statement.replace(/PASSWORD '.*';$/, "PASSWORD '…';");

/** Which field is still empty, named rather than left to be guessed. */
export function missing(): string {
  if (trimmed("new-name") === "") {
    return "a name is needed";
  }
  if (role() === "") {
    return "a role is needed";
  }
  return "a space is needed — or choose the whole node, which is not the same thing";
}

/** Keep the preview current, with the password shown as the store shows it. */
function preview(): void {
  const statement = definition();
  write("role-says", MEANS[value("new-role")] ?? "");
  write("define-preview", statement === null ? missing() : redacted(statement));
}

/** Show only the fields the chosen reach and role actually need. */
export function shapeTheForm(): void {
  hide("scope-field", value("new-reach") === "node");
  hide("role-other-field", value("new-role") !== "other");
  preview();
}

/** The role the change form describes, which may be one typed by hand. */
function changedRole(): string {
  const chosen = value("change-role");
  return chosen === "other" ? trimmed("change-role-other") : chosen;
}

/**
 * The `ALTER USER` this form describes, or `null` while it is incomplete.
 *
 * A password of `''` is a real password and not an empty field, so it is the
 * one input here with no emptiness check — the statement is complete the moment
 * a name is present.
 */
export function alteration(): string | null {
  const name = trimmed("change-name");
  if (name === "") {
    return null;
  }
  if (value("change-what") === "password") {
    return "ALTER USER " + name + " SET PASSWORD " + quoted(value("change-password")) + ";";
  }
  const named = changedRole();
  return named === "" ? null : "ALTER USER " + name + " SET ROLE " + named + ";";
}

/**
 * Which field is still empty, named rather than left to be guessed.
 *
 * It names only what the chosen change actually needs: a hint that mentions a
 * role while somebody is typing a password reads as a second missing field, and
 * they go looking for a control that is not on screen.
 */
export function changeMissing(): string {
  if (trimmed("change-name") === "") {
    return "a name is needed";
  }
  return "a role is needed";
}

/** Show the fields this change needs, and the statement it would run. */
export function shapeTheChange(): void {
  const changing = value("change-what");
  hide("change-password-field", changing !== "password");
  hide("change-role-field", changing !== "role");
  hide("change-role-other-field", changing !== "role" || value("change-role") !== "other");
  const statement = alteration();
  write("change-preview", statement === null ? changeMissing() : redacted(statement));
}

/**
 * The `DROP USER` this form describes, or `null` while it is not confirmed.
 *
 * The name must be typed twice and match. A single click is the wrong shape for
 * this one: the grants go with the user, a new user of the same name inherits
 * none of them, and if it was the last owner of the whole node there is no way
 * back in at all. Typing the name is the cheapest control that makes the reader
 * name who they mean.
 */
export function removal(): string | null {
  const name = trimmed("remove-name");
  const again = trimmed("remove-confirm");
  return name !== "" && name === again ? "DROP USER " + name + ";" : null;
}

/** Keep the button and the preview honest about whether the two names agree. */
export function shapeTheRemoval(): void {
  const statement = removal();
  disable("remove", statement === null);
  const name = trimmed("remove-name");
  write(
    "remove-preview",
    statement !== null
      ? statement
      : name === ""
        ? "a name is needed"
        : "type the same name again to confirm",
  );
}

export function wire(): void {
  for (const field of [
    "new-name",
    "new-scope",
    "new-role",
    "new-role-other",
    "new-password",
    "new-reach",
  ]) {
    at(field).addEventListener("input", shapeTheForm);
    at(field).addEventListener("change", shapeTheForm);
  }

  for (const field of [
    "change-name",
    "change-what",
    "change-password",
    "change-role",
    "change-role-other",
  ]) {
    at(field).addEventListener("input", shapeTheChange);
    at(field).addEventListener("change", shapeTheChange);
  }

  for (const field of ["remove-name", "remove-confirm"]) {
    at(field).addEventListener("input", shapeTheRemoval);
  }

  shapeTheForm();
  shapeTheChange();
  shapeTheRemoval();
}
