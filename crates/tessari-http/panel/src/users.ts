//! Who exists, and the buttons that change that.
//!
//! Everything here goes through a statement over `POST /script`. There is no
//! request on this page that a `curl` could not make, which is what keeps a
//! console feature from becoming a capability only the console has.
//!
//! # The node answers in full and this file renders a page
//!
//! `INFO FOR USERS` refuses rather than filters: it is answered only to a caller
//! who administers the tenancy, and then in full for that tenancy. So every row
//! in the answer is a row the reader may see, and holding them all here crosses
//! no boundary — which is what makes rendering a page, rather than paging the
//! statement, an honest arrangement rather than a shortcut.
//!
//! It is an arrangement with a MEASUREMENT behind it and a trigger for undoing
//! it. Measured on a skewed 5 000-account store: the listing costs about 4.9 µs
//! and 74 bytes per account, beside a fixed ~30 ms of password verification that
//! a signed-in panel pays once rather than per request. The node is not what
//! costs; five thousand table rows in a browser are. Past **10 000 accounts in
//! one tenancy** the paging belongs in the statement instead — that is engine
//! work, and it is recorded as Q-678 rather than left to be noticed.

import { held, valueOf } from "./api.js";
import { at, clear, made, say, setValue, trailer, trimmed, write } from "./dom.js";
import { facts, put } from "./draw.js";
import { forget, remember } from "./roster.js";
import { told } from "./session.js";
import { settled, state } from "./states.js";
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

/**
 * How many rows reach the page at once.
 *
 * A ceiling on the RENDER and not on the answer. It is deliberately not a pager:
 * an operator who has to walk pages of accounts is missing a filter, not a
 * control, and the field above the list is the cheaper answer to the same need.
 */
const SHOWN = 200;

/** The whole answer, held unrendered so the filter does not re-ask the node. */
let everybody: readonly Listed[] = [];

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

/** What the reader typed into the filter, folded so case is not a trap. */
const wanted = (): string => trimmed("user-filter").toLowerCase();

/** The accounts the filter admits, in the order the node listed them. */
function matching(): readonly Listed[] {
  const needle = wanted();
  if (needle === "") {
    return everybody;
  }
  return everybody.filter((one) => (one.user ?? "").toLowerCase().includes(needle));
}

/**
 * How many there are, how many are on screen, and — when those differ — what to
 * do about it.
 *
 * The total is exact because it is free: the answer is already here, so there is
 * no count query to decide whether to afford. What is not free is pretending the
 * page is the list, which is how an operator concludes an account does not exist
 * when it is simply row 4 000.
 */
function tally(matched: readonly Listed[]): string {
  const held = everybody.length;
  const filtered = wanted() !== "";
  if (matched.length <= SHOWN) {
    return filtered ? `${matched.length} of ${held}` : `${held}`;
  }
  return `showing ${SHOWN} of ${matched.length}${filtered ? "" : ` — type a name to narrow`}`;
}

/**
 * One page of the listing.
 *
 * It renders what it is given and decides nothing about which rows those are —
 * the bound belongs to the caller, so that the roster and the render can be fed
 * from different sets without this function knowing there is a difference.
 */
function listing(rows: readonly Listed[]): HTMLTableElement {
  const table = made("table");
  const head = table.createTHead().insertRow();
  for (const column of ["user", "role", "reach"]) {
    const cell = made("th");
    cell.textContent = column;
    head.appendChild(cell);
  }
  const body = table.createTBody();
  for (const one of rows) {
    const row = body.insertRow();
    const name = one.user ?? "";
    row.insertCell().textContent = name;
    row.insertCell().textContent = said(one);
    row.insertCell().textContent = reach(one);
    // A name in a listing is there to be clicked; typing it again is the kind
    // of small tax that makes an operator go back to `curl`.
    row.addEventListener("click", () => pick(name));
  }
  return table;
}

/**
 * The role, in the word people use.
 *
 * An owner with no space is the node's administrator. The listing says so rather
 * than leaving it to be inferred from an empty cell — which is what the panel did
 * before, and nobody inferred it.
 */
const said = (one: Listed): string =>
  one.role === "owner" && one.namespace === undefined ? "owner · admin" : (one.role ?? "");

const reach = (one: Listed): string =>
  one.namespace === undefined
    ? "the whole node"
    : one.namespace + (one.database === undefined ? "" : "." + one.database);

/** Everybody the caller is allowed to be told about. */
export async function listUsers(): Promise<void> {
  state("user-status", "waiting", "asking the node who it knows about…");
  try {
    const answer = held(await valueOf("INFO FOR USERS;", "Users · list"));
    const listed = answer === null ? [] : answer["users"];
    everybody = Array.isArray(listed) ? (listed as readonly Listed[]) : [];
    // The roster is fed from the WHOLE answer and never from the page. It stands
    // for what the node said, so a destructive form can draw the radius of an
    // account that was filtered off the screen — and it starts again with every
    // listing, because a name kept across a removal would draw a reach that is
    // no longer there.
    forget();
    for (const one of everybody) {
      remember(one.user ?? "", { role: said(one), reach: reach(one) });
    }
    redraw();
  } catch (failure) {
    clear("user-list");
    // The node's own words, and then what to do with them — a refusal here is
    // usually the tenancy rule working, and an operator who is told only that
    // it refused goes looking for a bug instead of for an owner.
    state(
      "user-status",
      "wrong",
      `${told(failure)} — a listing is answered to whoever administers the tenancy.`,
    );
  }
}

/** Redraw from the answer already held — the filter never re-asks the node. */
function redraw(): void {
  const matched = matching();
  clear("user-list");
  if (matched.length === 0) {
    // Two different emptinesses, and conflating them sends the reader to the
    // wrong remedy: one is a store with no accounts, the other is a filter that
    // excluded every account there is.
    state(
      "user-status",
      "empty",
      everybody.length === 0
        ? "No accounts — this store is open to anybody, and the first DEFINE USER closes it."
        : "No account matches that — clear the filter to see all " +
            `${everybody.length} of them.`,
    );
    at("user-list").appendChild(trailer("(nothing to show)"));
  } else if (matched.length > SHOWN) {
    state(
      "user-status",
      "partial",
      `Showing ${SHOWN} of ${matched.length} — type part of a name to narrow it.`,
    );
    at("user-list").appendChild(listing(matched.slice(0, SHOWN)));
  } else {
    settled("user-status");
    at("user-list").appendChild(listing(matched));
  }
  write("user-count", tally(matched));
}

export function wire(): void {
  at("list").addEventListener("click", listUsers);

  // The filter redraws from the answer already held rather than asking again:
  // the node has told us everything it is going to tell us, and a request per
  // keystroke would be a cost this arrangement was chosen to avoid.
  at("user-filter").addEventListener("input", redraw);

  at("tab-access").addEventListener("click", () => {
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
