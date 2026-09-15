//! Who exists, and the buttons that change that.
//!
//! Everything here goes through a statement over `POST /script`. There is no
//! request on this page that a `curl` could not make, which is what keeps a
//! console feature from becoming a capability only the console has.

import { held, valueOf } from "./api.js";
import { at, clear, made, say, setValue, trailer, trimmed } from "./dom.js";
import { facts, put } from "./draw.js";
import { forget, remember } from "./roster.js";
import { told } from "./session.js";
import {
  alteration,
  changeMissing,
  changeWhy,
  definition,
  missing,
  removal,
  removeWhy,
  shapeTheChange,
  shapeTheRemoval,
} from "./user-forms.js";

/** One row of `INFO FOR USERS`. */
interface Listed {
  readonly user?: string;
  readonly role?: string;
  readonly namespace?: string;
  readonly database?: string;
}

/** Fill both name fields from a listing row, and reshape what depends on them. */
function pick(name: string): void {
  setValue("lookup-name", name);
  // Both forms, because a name picked out of a listing is picked in order to do
  // something to it, and which of the two comes next is not knowable from the
  // click.
  setValue("change-name", name);
  setValue("remove-name", name);
  shapeTheChange();
  // Deliberately NOT the confirmation field: a click that filled both would arm
  // the destructive button by itself.
  shapeTheRemoval();
  at("lookup").click();
}

function listing(everybody: readonly Listed[]): HTMLTableElement {
  const table = made("table");
  const head = table.createTHead().insertRow();
  for (const column of ["user", "role", "reach"]) {
    const cell = made("th");
    cell.textContent = column;
    head.appendChild(cell);
  }
  const body = table.createTBody();
  // The roster starts again with every listing: it stands for what the node just
  // said, and a name kept from a listing before a removal would let a destructive
  // form draw a reach that is no longer there.
  forget();
  for (const one of everybody) {
    const row = body.insertRow();
    const name = one.user ?? "";
    row.insertCell().textContent = name;
    // An owner with no space is the node's administrator. The listing says so
    // in the word people use, rather than leaving it to be inferred from an
    // empty cell — which is what the panel did before, and nobody inferred it.
    const said =
      one.role === "owner" && one.namespace === undefined ? "owner · admin" : (one.role ?? "");
    row.insertCell().textContent = said;
    const reach =
      one.namespace === undefined
        ? "the whole node"
        : one.namespace + (one.database === undefined ? "" : "." + one.database);
    row.insertCell().textContent = reach;
    // The same two strings the reader is looking at, so the radius a destructive
    // form draws and the row it was picked from cannot disagree.
    remember(name, { role: said, reach });
    // A name in a listing is there to be clicked; typing it again is the kind
    // of small tax that makes an operator go back to `curl`.
    row.addEventListener("click", () => pick(name));
  }
  return table;
}

/** Everybody the caller is allowed to be told about. */
export async function listUsers(): Promise<void> {
  say("user-status", "asking…");
  try {
    const answer = held(await valueOf("INFO FOR USERS;", "Users · list"));
    const everybody = answer === null ? [] : answer["users"];
    clear("user-list");
    if (!Array.isArray(everybody) || everybody.length === 0) {
      at("user-list").appendChild(trailer("(no users — this store is open to anybody)"));
      say("user-status", "");
      return;
    }
    at("user-list").appendChild(listing(everybody as readonly Listed[]));
    say("user-status", "");
  } catch (failure) {
    clear("user-list");
    say("user-status", told(failure), true);
  }
}

export function wire(): void {
  at("list").addEventListener("click", listUsers);

  at("tab-users").addEventListener("click", () => {
    if (at("user-list").textContent === "") {
      void listUsers();
    }
  });

  at("lookup").addEventListener("click", async () => {
    const name = trimmed("lookup-name");
    if (name === "") {
      say("user-status", "a name is needed — there is no listing to pick from", true);
      return;
    }
    say("user-status", "asking…");
    try {
      const answered = await valueOf("INFO FOR USER " + name + ";", "Users · detail");
      const one = held(answered);
      if (one !== null) {
        facts("user-answer", one);
      } else {
        put("user-answer", answered);
      }
      say("user-status", "");
    } catch (failure) {
      clear("user-answer");
      say("user-status", told(failure), true);
    }
  });

  at("define").addEventListener("click", async () => {
    const statement = definition();
    if (statement === null) {
      say("define-status", missing(), true);
      return;
    }
    say("define-status", "running…");
    try {
      const answered = await valueOf(statement, "Users · define");
      const finished = answered !== null && answered.kind === "done";
      say("define-status", finished ? "ok" : "");
      if (!finished) {
        put("user-answer", answered);
      }
    } catch (failure) {
      say("define-status", told(failure), true);
    }
  });

  at("change").addEventListener("click", async () => {
    const statement = alteration();
    if (statement === null) {
      say("change-status", changeMissing(), true);
      return;
    }
    // Refused here rather than by a dead button: a control that will not press
    // and says nothing leaves the operator looking for the field that is wrong,
    // and the whole point of the reason is that somebody articulates one.
    const why = changeWhy();
    if (why === "") {
      say("change-status", "say why — this ends their session and they sign in again", true);
      return;
    }
    say("change-status", "running…");
    try {
      const answered = await valueOf(statement, "Users · change", why);
      say("change-status", answered !== null && answered.kind === "done" ? "ok" : "");
      // The listing carries the role, so a role change that is not redrawn
      // leaves the old one on screen looking current.
      await listUsers();
    } catch (failure) {
      say("change-status", told(failure), true);
    }
  });

  at("remove").addEventListener("click", async () => {
    const statement = removal();
    if (statement === null) {
      say("remove-status", "the two names do not match", true);
      return;
    }
    const why = removeWhy();
    if (why === "") {
      say("remove-status", "say why — this does not come back", true);
      return;
    }
    say("remove-status", "running…");
    try {
      const answered = await valueOf(statement, "Users · remove", why);
      say("remove-status", answered !== null && answered.kind === "done" ? "removed" : "");
      // Cleared only on success, so a refusal leaves the name on screen to be
      // read — and never leaves a confirmed form one click from firing again.
      setValue("remove-name", "");
      setValue("remove-confirm", "");
      setValue("remove-why", "");
      shapeTheRemoval();
      await listUsers();
    } catch (failure) {
      // The node's own words. A refusal here is the permission system working,
      // and paraphrasing it would hide which of the several reasons it was.
      say("remove-status", told(failure), true);
    }
  });
}
