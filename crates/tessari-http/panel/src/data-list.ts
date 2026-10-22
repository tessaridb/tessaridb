//! Reading what the store holds, for the Data screen.
//!
//! Three questions, each one statement: `INFO FOR STORE` for the namespaces,
//! `INFO FOR NAMESPACE` for its databases, and every table of the chosen
//! database described in ONE script of `INFO FOR TABLE` — a request per table
//! would make a database of two hundred tables two hundred round trips.

import { ask, held, Unreachable, valueOf, type Answer, type Result } from "./api.js";
import { openDialog } from "./dialog.js";
import { at, clear, made, say, setValue, value, write } from "./dom.js";
import { facts } from "./draw.js";
import { offer } from "./topic-list.js";
import { aName } from "./topic-names.js";
import { told } from "./session.js";
import { settled, state } from "./states.js";
import { onArrival, show } from "./tabs.js";

const SCREEN = "Data";

/** One table as `INFO FOR TABLE` describes it. */
interface Described {
  readonly name: string;
  readonly about: Record<string, unknown>;
}

let opened: Described | null = null;

const words = (failure: unknown): string =>
  failure instanceof Unreachable ? "the node did not answer — " + told(failure) : told(failure);

const names = (answered: Result | null, field: string): string[] => {
  const list = held(answered)?.[field];
  return Array.isArray(list) ? list.filter((each): each is string => typeof each === "string") : [];
};

/** What kind of table this is, in one word, from the flags the node reports. */
function kind(about: Record<string, unknown>): string {
  for (const flag of ["collection", "vault", "bucket", "edge"]) {
    if (about[flag] === true) return flag;
  }
  // A queue with a dead letter has no statement to read (its topic is held by
  // id), and only a queue reports one.
  if (typeof about["dead_letter"] === "string") return "queue";
  // A series, a space, a queue and a topic carry no flag of their own; the
  // statement that declared them says which, as the Series and Spaces panes
  // already read it.
  const definition = about["definition"];
  for (const declared of ["series", "space", "queue", "topic"]) {
    if (typeof definition === "string" && definition.startsWith(`DEFINE ${declared.toUpperCase()} `)) {
      return declared;
    }
  }
  return about["schemafull"] === true ? "table" : "table, schemaless";
}

const count = (about: Record<string, unknown>, field: string): string => {
  const list = about[field];
  return Array.isArray(list) ? String(list.length) : "";
};

/** The statement that selects this database, for every question asked inside it. */
function selection(): string | null {
  const namespace = aName(value("data-namespace"));
  const database = aName(value("data-database"));
  return namespace === null || database === null
    ? null
    : `USE NAMESPACE ${namespace}; USE DATABASE ${database}; `;
}

/** Who reads again when the chosen namespace or database changes. */
const listeners: (() => void)[] = [];

/** Read again whenever this page's namespace and database are chosen. */
export function onChosen(listener: () => void): void {
  listeners.push(listener);
}

const chosen = (): void => {
  for (const listener of listeners) listener();
};

async function readNamespaces(): Promise<void> {
  state("data-status", "waiting", "asking…");
  try {
    offer("data-namespace", names(await valueOf("INFO FOR STORE;", SCREEN), "namespaces"));
  } catch (failure) {
    state("data-status", "wrong", words(failure));
    return;
  }
  await readDatabases();
}

async function readDatabases(): Promise<void> {
  const namespace = aName(value("data-namespace"));
  if (namespace === null) {
    offer("data-database", []);
    draw([]);
    chosen();
    state("data-status", "empty", "No namespace here — declare one on Run with DEFINE NAMESPACE.");
    return;
  }
  try {
    const answered = await valueOf(`USE NAMESPACE ${namespace}; INFO FOR NAMESPACE;`, SCREEN);
    offer("data-database", names(answered, "databases"));
  } catch (failure) {
    state("data-status", "wrong", words(failure));
    return;
  }
  await readTables();
}

async function readTables(): Promise<void> {
  chosen();
  const selecting = selection();
  if (selecting === null) {
    draw([]);
    state("data-status", "empty", "This namespace holds no database yet.");
    return;
  }
  try {
    const tables = names(await valueOf(selecting + "INFO FOR DATABASE;", SCREEN), "tables")
      .filter((name) => aName(name) !== null);
    if (tables.length === 0) {
      draw([]);
      state("data-status", "empty", "This database holds no table yet — define one on Run.");
      return;
    }
    const { text } = await ask(
      selecting + tables.map((name) => `INFO FOR TABLE ${name};`).join(" "),
      SCREEN,
    );
    const body = JSON.parse(text) as Answer;
    if (!Array.isArray(body.results)) {
      throw new Error(typeof body.error === "string" ? body.error : text);
    }
    // The two `USE` results come first; each table's answer follows in order.
    const answers = body.results.slice(2);
    draw(tables.map((name, at_) => ({ name, about: held(answers[at_] ?? null) ?? {} })));
    settled("data-status");
  } catch (failure) {
    state("data-status", "wrong", words(failure));
  }
}

function draw(read: readonly Described[]): void {
  clear("data-list");
  if (read.length === 0) return;
  const table = made("table");
  const head = table.createTHead().insertRow();
  for (const column of ["table", "kind", "fields", "indexes", ""]) {
    const cell = made("th");
    cell.textContent = column;
    head.appendChild(cell);
  }
  const body = table.createTBody();
  for (const each of read) {
    const row = body.insertRow();
    const name = made("button", "link");
    name.type = "button";
    name.textContent = each.name;
    name.addEventListener("click", () => openTable(each));
    row.insertCell().appendChild(name);
    row.insertCell().textContent = kind(each.about);
    row.insertCell().textContent = count(each.about, "fields");
    row.insertCell().textContent = count(each.about, "indexes");
    const actions = row.insertCell();
    actions.className = "actions";
    const browse = made("button", "quiet small");
    browse.type = "button";
    browse.textContent = "Open on Run";
    browse.setAttribute("aria-label", `Open ${each.name} on Run`);
    browse.addEventListener("click", () => browseOnRun(each));
    actions.appendChild(browse);
  }
  at("data-list").appendChild(table);
}

function openTable(one: Described): void {
  opened = one;
  write("table-dialog-title", one.name);
  say("table-status", kind(one.about));
  // A summary, not the flags: the kind is said above and the definition below
  // carries every detail the node holds.
  const named = (field: string): string => {
    const list = one.about[field];
    return Array.isArray(list)
      ? list
          .map((each) =>
            typeof each === "object" && each !== null && "name" in each ? String(each.name) : String(each),
          )
          .join(", ")
      : "";
  };
  facts("table-facts", {
    identity: one.about["identity"] ?? "",
    fields: named("fields") || "none declared",
    indexes: named("indexes") || "none",
    events: named("events") || "none",
    ...(typeof one.about["dead_letter"] === "string" ? { "dead letter": one.about["dead_letter"] } : {}),
  });
  write("table-definition", typeof one.about["definition"] === "string" ? one.about["definition"] : "");
  openDialog("table-dialog");
}

/** The statement that reads the table, put on Run and run, so the answer is the first thing seen. */
function browseOnRun(one: Described): void {
  const selecting = selection();
  if (selecting === null) return;
  setValue("script", `${selecting}SELECT * FROM ${one.name} LIMIT 50;`);
  show("run");
  at("run-script").click();
}

export function wire(): void {
  at("data-namespace").addEventListener("change", () => void readDatabases());
  at("data-database").addEventListener("change", () => void readTables());
  at("data-refresh").addEventListener("click", () => void readNamespaces());
  at("table-browse").addEventListener("click", () => {
    const one = opened;
    const shown = at("table-dialog");
    if (shown instanceof HTMLDialogElement) shown.close();
    if (one !== null) browseOnRun(one);
  });
  // Read again every time the screen is reached — the identity, or the
  // catalog, may have changed since.
  onArrival(["data"], () => void readNamespaces());
}
