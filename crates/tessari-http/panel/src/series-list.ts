//! The Series pane's reading: where, and which tables there are series.
//!
//! Everything here reads. A table's kind is not a field of `INFO FOR DATABASE`,
//! so each table is asked for `INFO FOR TABLE`: a series writes back the
//! `DEFINE SERIES` statement that made it, and a rollup says it is one.

import { held as valueHeld, ask, Unreachable, valueOf, type Answer, type Result } from "./api.js";
import { at, clear, made, value } from "./dom.js";
import { told } from "./session.js";
import { settled, state } from "./states.js";
import { onArrival } from "./tabs.js";
import { fieldsOf } from "./topic-info.js";
import { offer } from "./topic-list.js";
import { aName, tenancy } from "./topic-names.js";

const SCREEN = "Run";

/** The most tables asked about; the rest are counted, not read. */
const TABLES_ASKED = 200;

/** What a table turned out to be, or `null` when it is neither kind. */
interface Kind {
  readonly name: string;
  readonly rollup: boolean;
  /** The time field, or `null` when the series is ordered by arrival. */
  readonly time: string | null;
  readonly retain: string;
}

const DECLARED = /^DEFINE SERIES \S+ RETAIN (\S+?)(?: TIME (\S+?))?;/;

function kind(value: unknown): Kind | null {
  const fields = fieldsOf(value);
  const name = fields?.["table"];
  if (typeof name !== "string") {
    return null;
  }
  const undefinable = fields?.["undefinable"];
  if (typeof undefinable === "string" && undefinable.includes("is a rollup")) {
    return { name, rollup: true, time: "window", retain: "set by its DEFINE ROLLUP" };
  }
  const definition = fields?.["definition"];
  const declared = typeof definition === "string" ? DECLARED.exec(definition) : null;
  if (declared === null) {
    return null;
  }
  return { name, rollup: false, time: declared[2] ?? null, retain: declared[1] ?? "" };
}

const words = (failure: unknown): string =>
  failure instanceof Unreachable ? "the node did not answer — " + told(failure) : told(failure);

function strings(answered: Result | null, field: string): string[] {
  const list = valueHeld(answered)?.[field];
  return Array.isArray(list) ? list.filter((each): each is string => typeof each === "string") : [];
}

async function readNamespaces(): Promise<void> {
  state("series-status", "waiting", "asking…");
  try {
    offer("series-namespace", strings(await valueOf("INFO FOR STORE;", SCREEN), "namespaces"));
  } catch (failure) {
    state("series-status", "wrong", words(failure));
    return;
  }
  await readDatabases();
}

async function readDatabases(): Promise<void> {
  const namespace = aName(value("series-namespace"));
  if (namespace === null) {
    offer("series-database", []);
    draw([]);
    state("series-status", "empty", "No namespace here — declare one with DEFINE NAMESPACE.");
    return;
  }
  try {
    const answered = await valueOf(`USE NAMESPACE ${namespace}; INFO FOR NAMESPACE;`, SCREEN);
    offer("series-database", strings(answered, "databases"));
  } catch (failure) {
    state("series-status", "wrong", words(failure));
    return;
  }
  await readSeries();
}

async function readSeries(): Promise<void> {
  const namespace = aName(value("series-namespace"));
  const database = aName(value("series-database"));
  if (namespace === null || database === null) {
    draw([]);
    state("series-status", "empty", "Choose a namespace and a database that exist.");
    return;
  }
  const start = tenancy(namespace, database);
  state("series-status", "waiting", "asking…");
  try {
    const tables = strings(await valueOf(start + "INFO FOR DATABASE;", SCREEN), "tables")
      .filter((name) => aName(name) !== null);
    const asked = tables.slice(0, TABLES_ASKED);
    let answered: readonly Result[] = [];
    if (asked.length > 0) {
      const { text } = await ask(start + asked.map((name) => `INFO FOR TABLE ${name};`).join(" "), SCREEN);
      const body = JSON.parse(text) as Answer;
      if (!Array.isArray(body.results)) {
        throw new Error(typeof body.error === "string" ? body.error : text);
      }
      answered = body.results;
    }
    const found = answered.map((each) => kind(each.value)).filter((each): each is Kind => each !== null);
    draw(found);
    if (found.length === 0) {
      state("series-status", "empty", `${namespace}.${database} holds no series — DEFINE SERIES declares one.`);
    } else if (tables.length > asked.length) {
      state("series-status", "partial", `asked about ${asked.length} of ${tables.length} tables`);
    } else {
      settled("series-status");
    }
  } catch (failure) {
    draw([]);
    state("series-status", "wrong", words(failure));
  }
}

function draw(found: readonly Kind[]): void {
  clear("series-list");
  if (found.length === 0) {
    return;
  }
  const table = made("table");
  const head = table.createTHead().insertRow();
  for (const name of ["table", "kind", "ordered by", "answers for"]) {
    const column = made("th");
    column.textContent = name;
    head.appendChild(column);
  }
  const body = table.createTBody();
  for (const each of found) {
    const line = body.insertRow();
    line.insertCell().textContent = each.name;
    line.insertCell().textContent = each.rollup ? "rollup" : "series";
    line.insertCell().textContent = each.time ?? "arrival";
    line.insertCell().textContent = each.retain;
  }
  at("series-list").appendChild(table);
}

export function wire(): void {
  at("series-namespace").addEventListener("change", () => void readDatabases());
  at("series-database").addEventListener("change", () => void readSeries());
  at("series-refresh").addEventListener("click", () => void readNamespaces());
  onArrival(["run"], () => void readNamespaces());
}
