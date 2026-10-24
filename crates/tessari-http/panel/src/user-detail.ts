//! One account, looked at: what it is, what it holds, and taking any of it away.
//!
//! `INFO FOR USER` answers the role and tenancy, every table grant and every
//! authority. Each grant and authority is drawn with a Revoke beside it, which
//! opens the same give-or-take dialog every grant goes through — so a revoke
//! still shows what it will do and still asks why before anything is sent.

import { held, valueOf } from "./api.js";
import { closeDialog, openDialog } from "./dialog.js";
import { at, clear, made, say, setValue, write } from "./dom.js";
import { facts } from "./draw.js";
import { offer, type Reach } from "./grants.js";
import { told } from "./session.js";
import { shapeTheChange, shapeTheRemoval } from "./user-forms.js";
import type { Alteration } from "./user-says.js";

/** The account the dialog is showing, so a grant applied elsewhere can redraw it. */
let showing: string | null = null;

/** What one held thing is, where, and how to take it away again. */
interface Held {
  readonly kind: string;
  readonly what: string;
  readonly on: string;
  readonly reach: Reach;
}

const words = (value: unknown): string =>
  Array.isArray(value) ? value.map(String).join(", ") : typeof value === "string" ? value : "";

/** Table grants and authorities as one list, each knowing its own revoke. */
function holdings(one: Record<string, unknown>): Held[] {
  const namespace = typeof one["namespace"] === "string" ? one["namespace"] : null;
  const database = typeof one["database"] === "string" ? one["database"] : null;
  const out: Held[] = [];
  for (const grant of Array.isArray(one["grants"]) ? one["grants"] : []) {
    if (typeof grant !== "object" || grant === null) continue;
    const fields = grant as Record<string, unknown>;
    const table = typeof fields["table"] === "string" ? fields["table"] : "a dropped table";
    // A table grant names the table alone; the revoke needs it in full, which
    // only an account confined to one database can supply. Anybody else's is
    // offered with the name alone and the dialog asks for the rest.
    const full = namespace !== null && database !== null ? `${namespace}.${database}.${table}` : table;
    out.push({ kind: "table grant", what: words(fields["verbs"]), on: full, reach: "table" });
  }
  for (const authority of Array.isArray(one["authorities"]) ? one["authorities"] : []) {
    if (typeof authority !== "object" || authority === null) continue;
    const fields = authority as Record<string, unknown>;
    const where = words(fields["reach"]);
    const reach: Reach = where === "store" ? "store" : where.includes(".") ? "database" : "namespace";
    out.push({ kind: "authority", what: words(fields["authority"]), on: where, reach });
  }
  return out;
}

function drawHoldings(name: string, list: readonly Held[]): void {
  clear("user-holds");
  if (list.length === 0) {
    const none = made("p", "note");
    none.textContent = "Holds what its role gives, and nothing granted besides.";
    at("user-holds").appendChild(none);
    return;
  }
  const table = made("table");
  const head = table.createTHead().insertRow();
  for (const column of ["holds", "what", "on", ""]) {
    const cell = made("th");
    cell.textContent = column;
    head.appendChild(cell);
  }
  const body = table.createTBody();
  for (const one of list) {
    const row = body.insertRow();
    row.insertCell().textContent = one.kind;
    row.insertCell().textContent = one.what;
    row.insertCell().textContent = one.on === "store" ? "the whole store" : one.on;
    const cell = row.insertCell();
    cell.className = "actions";
    const revoke = made("button", "quiet small danger");
    revoke.type = "button";
    revoke.textContent = "Revoke…";
    revoke.setAttribute("aria-label", `Revoke ${one.what} on ${one.on} — ${name}`);
    revoke.addEventListener("click", () =>
      offer({
        who: name,
        direction: "take",
        reach: one.reach,
        name: one.reach === "store" ? "" : one.on,
        what: one.what,
      }),
    );
    cell.appendChild(revoke);
  }
  at("user-holds").appendChild(table);
}

/** What may be done to one account, each from its own row. */
export type Act = "detail" | "password" | "role" | "lock" | "unlock" | "grant" | "remove";

/**
 * Open the dialog for one action on one account.
 *
 * The name goes in from the row and is shown read-only: an action on an account
 * is chosen by pointing at the account, and typing its name again was the tax
 * that made the old screen read like a terminal.
 */
export function act(kind: Act, name: string): void {
  if (kind === "detail") {
    void showUser(name);
    return;
  }
  if (kind === "grant") {
    offer({ who: name });
    return;
  }
  if (kind === "remove") {
    setValue("remove-name", name);
    // Deliberately NOT the confirmation field: a dialog that filled both would
    // arm the destructive button by itself.
    setValue("remove-confirm", "");
    setValue("remove-why", "");
    shapeTheRemoval();
    write("remove-dialog-title", `Remove ${name}`);
    openDialog("remove-dialog");
    return;
  }
  setValue("change-name", name);
  setValue("change-what", kind);
  setValue("change-password", "");
  setValue("change-why", "");
  shapeTheChange();
  write("change-dialog-title", CHANGE_TITLES[kind](name));
  openDialog("change-dialog");
}

/** The heading of the change dialog for each change it makes. */
const CHANGE_TITLES: Record<Alteration, (name: string) => string> = {
  password: (name) => `New password for ${name}`,
  role: (name) => `Role of ${name}`,
  lock: (name) => `Lock ${name}`,
  unlock: (name) => `Unlock ${name}`,
};

/** Open the account's dialog and read it from the node. */
export async function showUser(name: string): Promise<void> {
  showing = name;
  write("user-dialog-title", name);
  clear("user-answer");
  clear("user-holds");
  openDialog("user-dialog");
  say("user-detail-status", "asking…");
  try {
    const one = held(await valueOf("INFO FOR USER " + name + ";", "Users · detail"));
    if (one === null) {
      say("user-detail-status", "the node answered with nothing to show", true);
      return;
    }
    // The two lists are drawn below with their revokes, so the facts leave them out.
    const rest: Record<string, unknown> = { ...one };
    delete rest["grants"];
    delete rest["authorities"];
    facts("user-answer", rest);
    drawHoldings(name, holdings(one));
    say("user-detail-status", "");
  } catch (failure) {
    say("user-detail-status", told(failure), true);
  }
}

/** Read the shown account again — a grant just changed what it holds. */
export function refreshShown(): void {
  const dialog = at("user-dialog");
  if (showing !== null && dialog instanceof HTMLDialogElement && dialog.open) {
    void showUser(showing);
  }
}

/** The account's own buttons open the same dialogs its row does. */
export function wire(): void {
  const from = (kind: Exclude<Act, "detail">) => () => {
    if (showing === null) return;
    // Its own dialog closes first: two modals stacked is a maze, and the row's
    // action is one decision rather than a step inside this one. A grant is
    // the exception — it opens over the account so the list below redraws.
    if (kind !== "grant") closeDialog("user-dialog");
    act(kind, showing);
  };
  at("user-grant").addEventListener("click", from("grant"));
  at("user-password").addEventListener("click", from("password"));
  at("user-role").addEventListener("click", from("role"));
  at("user-remove").addEventListener("click", from("remove"));
}
