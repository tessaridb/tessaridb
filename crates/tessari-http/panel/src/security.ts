//! Security — row guards, and the switch that turns them on.
//!
//! The first shape of this surface, laid as the foundation the owner's wave
//! asked for (G094 SG5): see which tables enforce row security, flip the
//! switch, and reach the grant dialog with a row predicate already in hand.
//! Defining and dropping the guards themselves is the grant dialog's job —
//! this pane hands it a prefilled form rather than growing a second one.
//!
//! # Everything here is a statement
//!
//! The console has no private route: the list below is `INFO FOR DATABASE`
//! and `INFO FOR TABLE` through `/script`, the toggle is `ALTER TABLE …
//! ENABLE ROW SECURITY`, and the node refuses both with the caller's own
//! authority — a reader sees nothing here and cannot flip anything, which is
//! this screen's access check and needs no code of its own.

import { held, valueOf, type Result } from "./api.js";
import { at, clear, made, say, value } from "./dom.js";
import { to } from "./destinations.js";
import { offer } from "./grants.js";
import { el, type Node } from "./html.js";
import { button, field, note, pane, paneHead, panel, row, status, text } from "./ui.js";

/** The screen every statement below is recorded under. */
const SCREEN = "Security";

/** A table the database declares, with what this pane knows about its guards. */
interface Guarded {
  readonly name: string;
  readonly enforcing: boolean;
  readonly guards: string;
}

/** The string names one array-of-strings field of an answer carries. */
const names = (answered: Result | null, field: string): string[] => {
  const list = held(answered)?.[field];
  return Array.isArray(list) ? list.filter((each): each is string => typeof each === "string") : [];
};

/** The account names `INFO FOR USERS` answers with. */
const accounts = (answered: Result | null): string[] => {
  const list = held(answered)?.["users"];
  if (!Array.isArray(list)) return [];
  const out: string[] = [];
  for (const one of list) {
    if (typeof one === "object" && one !== null) {
      const name = (one as Record<string, unknown>)["user"];
      if (typeof name === "string") out.push(name);
    }
  }
  return out;
};

/** The namespace.database prefix every statement below carries. */
function selection(): string | null {
  const namespace = trimmed("security-namespace");
  const database = trimmed("security-database");
  return namespace === null || database === null
    ? null
    : `USE NAMESPACE ${namespace}; USE DATABASE ${database}; `;
}

const trimmed = (id: string): string | null => {
  const found = value(id).trim();
  return found === "" ? null : found;
};

/** The guard texts held over one table, across every user the tenancy names. */
async function guardsOn(selecting: string, table: string): Promise<string> {
  const found: string[] = [];
  for (const who of accounts(await valueOf(selecting + "INFO FOR USERS;", SCREEN))) {
    const about = held(await valueOf(`${selecting}INFO FOR USER ${who};`, SCREEN));
    const grants = about === null || !Array.isArray(about["grants"]) ? [] : about["grants"];
    for (const grant of grants) {
      if (typeof grant !== "object" || grant === null) continue;
      const fields = grant as Record<string, unknown>;
      if (fields["table"] === table && typeof fields["guard"] === "string") {
        const verbs = Array.isArray(fields["verbs"]) ? fields["verbs"].map(String).join(", ") : "";
        found.push(`${who}: ${verbs} WHERE (${fields["guard"]})`);
      }
    }
  }
  return found.join("; ");
}

/** One rendered row per guarded table; an empty state rather than a blank pane. */
function draw(list: readonly Guarded[]): void {
  clear("security-list");
  const host = at("security-list");
  if (list.length === 0) {
    const none = made("p", "note");
    none.textContent = "Nothing here carries row security yet.";
    host.appendChild(none);
    return;
  }
  const table = made("table");
  const head = table.createTHead().insertRow();
  for (const column of ["table", "row security", "guards", ""]) {
    const cell = made("th");
    cell.textContent = column;
    head.appendChild(cell);
  }
  const body = table.createTBody();
  for (const one of list) {
    const line = body.insertRow();
    line.insertCell().textContent = one.name;
    line.insertCell().textContent = one.enforcing ? "on" : "off";
    line.insertCell().textContent = one.guards;
    const actions = line.insertCell();
    actions.className = "actions";
    const toggle = made("button", "quiet small");
    toggle.type = "button";
    toggle.textContent = one.enforcing ? "Turn off…" : "Turn on…";
    toggle.setAttribute("aria-label", `Row security on ${one.name}`);
    toggle.dataset["table"] = one.name;
    toggle.dataset["turn"] = one.enforcing ? "off" : "on";
    actions.appendChild(toggle);
    const grant = made("button", "quiet small");
    grant.type = "button";
    grant.textContent = "Grant with rows…";
    grant.setAttribute("aria-label", `Grant on ${one.name} with a row guard`);
    grant.dataset["grant"] = one.name;
    actions.appendChild(grant);
  }
  host.appendChild(table);
}

/** Read the tables and their guard state, and draw them. */
async function read(): Promise<void> {
  say("security-status", "asking…");
  const selecting = selection();
  if (selecting === null) {
    draw([]);
    say("security-status", "choose a namespace and a database");
    return;
  }
  try {
    const tables = names(await valueOf(selecting + "INFO FOR DATABASE;", SCREEN), "tables");
    const guarded: Guarded[] = [];
    for (const name of tables) {
      const shape = held(await valueOf(`${selecting}INFO FOR TABLE ${name};`, SCREEN));
      const enforcing = shape !== null && shape["row_security"] === true;
      const guards = await guardsOn(selecting, name);
      // A table the switch never touched and no grant guards has no reason to
      // be on this screen, and a long unguarded list would say nothing.
      if (enforcing || guards !== "") {
        guarded.push({ name, enforcing, guards });
      }
    }
    draw(guarded);
    say(
      "security-status",
      guarded.length === 0
        ? "nothing guards these tables yet"
        : `${guarded.length} table${guarded.length === 1 ? "" : "s"} carry row security`,
    );
  } catch (failure) {
    draw([]);
    say("security-status", failure instanceof Error ? failure.message : String(failure), true);
  }
}

/** `namespace.database.table`, the full spelling the grant dialog asks for. */
function fullname(selecting: string, table: string): string {
  const matched = /USE NAMESPACE (\S+); USE DATABASE (\S+);/.exec(selecting);
  return matched === null ? table : `${matched[1]}.${matched[2]}.${table}`;
}

export function wire(): void {
  at("security-refresh").addEventListener("click", () => {
    void read();
  });
  for (const id of ["security-namespace", "security-database"]) {
    at(id).addEventListener("change", () => {
      void read();
    });
  }
  // One listener for both actions. The toggle's refusal is the node's own
  // authority speaking — a reader cannot flip the switch — and the grant
  // button hands the dialog a prefilled form with the rows field showing.
  at("security-list").addEventListener("click", (event) => {
    const target = event.target;
    if (!(target instanceof HTMLElement)) return;
    const selecting = selection();
    if (selecting === null) return;
    const table = target.dataset["table"];
    if (table !== undefined && target.dataset["turn"] !== undefined) {
      const turning = target.dataset["turn"] === "on" ? "ENABLE" : "DISABLE";
      void valueOf(
        `${selecting}ALTER TABLE ${table} ${turning} ROW SECURITY;`,
        SCREEN,
        "row security switch",
      )
        .then(() => read())
        .catch((failure: unknown) => {
          say("security-status", failure instanceof Error ? failure.message : String(failure), true);
        });
      return;
    }
    const grant = target.dataset["grant"];
    if (grant !== undefined) {
      offer({ who: "", reach: "table", name: fullname(selecting, grant), rows: true });
    }
  });
}

/** The Security destination: one pane, read on demand. */
export function security(): Node {
  return panel(
    to("security"),
    false,
    pane(
      paneHead(
        "Row security",
        row("tight", status("security-status"), button("security-refresh", "Refresh", "quiet")),
      ),
      note(
        "A row guard narrows a grant to the rows a predicate reaches — " +
          "GRANT read ON orders WHERE (tenant = 'a') TO ada. The table's switch " +
          "decides whether guards are enforced; without it they sleep.",
      ),
      note(
        "The node answers this list — and refuses its changes — with the signed-in " +
          "caller's own authority.",
      ),
      row(
        "spread",
        field("Namespace", text("security-namespace")),
        field("Database", text("security-database")),
      ),
      el("div", { id: "security-list", class: "answer small" }),
    ),
  );
}
