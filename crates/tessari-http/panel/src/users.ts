//! Who exists, and what may be done to each of them, beside them.
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

import { held, Unreachable, valueOf } from "./api.js";
import { openDialog } from "./dialog.js";
import { at, clear, made, setValue, trailer, trimmed, write } from "./dom.js";
import { forget, remember } from "./roster.js";
import { told } from "./session.js";
import { settled, state } from "./states.js";
import { onArrival } from "./tabs.js";
import { act, type Act } from "./user-detail.js";
import { shapeTheForm } from "./user-forms.js";

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

/** The small buttons beside a row, each opening one action. */
const ACTIONS: readonly { readonly kind: Act; readonly label: string }[] = [
  { kind: "password", label: "Password…" },
  { kind: "role", label: "Role…" },
  { kind: "grant", label: "Give…" },
  { kind: "remove", label: "Remove…" },
];

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
  for (const column of ["user", "role", "reach", ""]) {
    const cell = made("th");
    cell.textContent = column;
    head.appendChild(cell);
  }
  const body = table.createTBody();
  for (const one of rows) {
    const row = body.insertRow();
    const name = one.user ?? "";
    // The name opens the account; the buttons at the end of the row act on it.
    const opener = made("button", "link");
    opener.type = "button";
    opener.textContent = name;
    opener.addEventListener("click", () => act("detail", name));
    row.insertCell().appendChild(opener);
    row.insertCell().textContent = said(one);
    row.insertCell().textContent = reach(one);
    const actions = row.insertCell();
    actions.className = "actions";
    for (const action of ACTIONS) {
      const pressed = made("button", action.kind === "remove" ? "quiet small danger" : "quiet small");
      pressed.type = "button";
      pressed.textContent = action.label;
      pressed.setAttribute("aria-label", `${action.label.replace("…", "")} — ${name}`);
      pressed.addEventListener("click", () => act(action.kind, name));
      actions.appendChild(pressed);
    }
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
    everybody = [];
    forget();
    clear("user-list");
    // The tally goes with the list. It read `6 of 29` beside an empty list and a
    // refusal, because it was the PREVIOUS identity's count — so a viewer who
    // had just been told they may not be told who exists was being told how
    // many there are, by a console that no longer knew.
    write("user-count", "");
    // The node's own words, and then what to do with them — a refusal here is
    // usually the tenancy rule working, and an operator who is told only that
    // it refused goes looking for a bug instead of for an owner.
    //
    // Only when the node actually answered, though. A request that never
    // arrived carried that same sentence in W320, so a stopped node read as a
    // permissions problem and sent the operator to check grants during an
    // outage. Two failures, two sentences.
    state(
      "user-status",
      "wrong",
      failure instanceof Unreachable
        ? `${told(failure)} — nothing about this listing is settled until it does.`
        : `${told(failure)} — a listing is answered to whoever administers the tenancy.`,
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

  // On arrival rather than on a click of the tab: ⌘3, the tablist's arrow keys,
  // a shared link and a reload all reach this screen without one, and each used
  // to land on an empty list with nothing said about why.
  //
  // Unconditionally, and that is the point. The guard here read "only if the
  // list is empty", which meant signing OUT left the previous identity's
  // accounts on screen — the list was not empty, so nothing asked again. Who
  // may be told who exists is the one question this screen answers, so it asks
  // it every time it is looked at rather than caching the answer across an
  // identity nobody holds any more.
  onArrival(["access"], () => void listUsers());

  at("new-user").addEventListener("click", () => {
    write("define-dialog-title", "New user");
    openDialog("define-dialog");
  });

  // An open store's one way out, offered from its banner on any screen: an
  // administrator of the whole node, which is the user the first DEFINE USER
  // has to be for anybody to administer it afterwards.
  at("define-owner").addEventListener("click", () => {
    setValue("new-reach", "node");
    setValue("new-role", "owner");
    shapeTheForm();
    write("define-dialog-title", "Define the owner");
    openDialog("define-dialog");
  });
}
