//! The Spaces pane's reading: which tables of a database are spaces, their keys by
//! prefix, and one key's value.
//!
//! The spaces are found as Series finds series — each table's `INFO FOR TABLE`
//! writes back the statement that made it. The keys and the value come from the
//! `/kv` routes, so what the pane shows is what any HTTP caller of those routes
//! would be answered.

import { ask, held as valueHeld, route, Unreachable, valueOf, type Answer, type Result } from "./api.js";
import { at, clear, made, value } from "./dom.js";
import { told } from "./session.js";
import { settled, state } from "./states.js";
import { onArrival } from "./tabs.js";
import { fieldsOf } from "./topic-info.js";
import { offer } from "./topic-list.js";
import { aName, tenancy } from "./topic-names.js";

const SCREEN = "Run";

/** The most tables asked about, as Series asks. */
const TABLES_ASKED = 200;

/** The most keys one listing shows. */
const KEYS_SHOWN = 100;

const words = (failure: unknown): string =>
  failure instanceof Unreachable ? "the node did not answer — " + told(failure) : told(failure);

function strings(answered: Result | null, field: string): string[] {
  const list = valueHeld(answered)?.[field];
  return Array.isArray(list) ? list.filter((each): each is string => typeof each === "string") : [];
}

function isSpace(result: Result): string | null {
  const fields = fieldsOf(result.value);
  const name = fields?.["table"];
  const definition = fields?.["definition"];
  return typeof name === "string" && typeof definition === "string" && definition.startsWith("DEFINE SPACE ")
    ? name
    : null;
}

async function readNamespaces(): Promise<void> {
  state("kv-status", "waiting", "asking…");
  try {
    offer("kv-namespace", strings(await valueOf("INFO FOR STORE;", SCREEN), "namespaces"));
  } catch (failure) {
    state("kv-status", "wrong", words(failure));
    return;
  }
  await readDatabases();
}

async function readDatabases(): Promise<void> {
  const namespace = aName(value("kv-namespace"));
  if (namespace === null) {
    offer("kv-database", []);
    offer("kv-space", []);
    state("kv-status", "empty", "No namespace here — declare one with DEFINE NAMESPACE.");
    return;
  }
  try {
    const answered = await valueOf(`USE NAMESPACE ${namespace}; INFO FOR NAMESPACE;`, SCREEN);
    offer("kv-database", strings(answered, "databases"));
  } catch (failure) {
    state("kv-status", "wrong", words(failure));
    return;
  }
  await readSpaces();
}

async function readSpaces(): Promise<void> {
  const namespace = aName(value("kv-namespace"));
  const database = aName(value("kv-database"));
  clear("kv-list");
  clear("kv-value");
  if (namespace === null || database === null) {
    offer("kv-space", []);
    state("kv-status", "empty", "Choose a namespace and a database that exist.");
    return;
  }
  const start = tenancy(namespace, database);
  state("kv-status", "waiting", "asking…");
  try {
    const tables = strings(await valueOf(start + "INFO FOR DATABASE;", SCREEN), "tables")
      .filter((name) => aName(name) !== null)
      .slice(0, TABLES_ASKED);
    let answered: readonly Result[] = [];
    if (tables.length > 0) {
      const { text } = await ask(start + tables.map((name) => `INFO FOR TABLE ${name};`).join(" "), SCREEN);
      const body = JSON.parse(text) as Answer;
      if (!Array.isArray(body.results)) {
        throw new Error(typeof body.error === "string" ? body.error : text);
      }
      answered = body.results;
    }
    const spaces = answered.map(isSpace).filter((name): name is string => name !== null);
    offer("kv-space", spaces);
    if (spaces.length === 0) {
      state("kv-status", "empty", `${namespace}.${database} holds no space — DEFINE SPACE declares one.`);
      return;
    }
  } catch (failure) {
    state("kv-status", "wrong", words(failure));
    return;
  }
  await listKeys();
}

/** The `/kv/{ns}/{db}/{space}` prefix, or `null` when a name is not chosen. */
function base(): string | null {
  const namespace = aName(value("kv-namespace"));
  const database = aName(value("kv-database"));
  const space = aName(value("kv-space"));
  return namespace === null || database === null || space === null
    ? null
    : `/kv/${namespace}/${database}/${space}`;
}

/** Which listing is the latest: two can be in flight at once (arriving on Run
 * and choosing a space both list), and only the newest may draw. */
let listing = 0;

async function listKeys(): Promise<void> {
  const mine = ++listing;
  const where = base();
  clear("kv-list");
  clear("kv-value");
  if (where === null) {
    state("kv-status", "empty", "Choose a space.");
    return;
  }
  const prefix = value("kv-prefix");
  const query = `?limit=${KEYS_SHOWN}` + (prefix === "" ? "" : `&prefix=${encodeURIComponent(prefix)}`);
  state("kv-status", "waiting", "asking…");
  try {
    const { status, body } = await route("GET", where + query, SCREEN);
    if (mine !== listing) {
      return;
    }
    clear("kv-list");
    const keys = (body as { keys?: unknown }).keys;
    if (status !== 200 || !Array.isArray(keys)) {
      const refused = (body as { error?: unknown }).error;
      throw new Error(typeof refused === "string" ? refused : `the node answered ${status}`);
    }
    drawKeys(keys.filter((each): each is string => typeof each === "string"));
    if (keys.length === 0) {
      state("kv-status", "empty", prefix === "" ? "The space holds no key." : `No key starts with ${prefix}.`);
    } else if (keys.length >= KEYS_SHOWN) {
      state("kv-status", "partial", `the first ${KEYS_SHOWN} keys`);
    } else {
      settled("kv-status");
    }
  } catch (failure) {
    state("kv-status", "wrong", words(failure));
  }
}

function drawKeys(keys: readonly string[]): void {
  const list = made("ul");
  for (const key of keys) {
    const item = made("li");
    const open = made("button");
    open.className = "quiet";
    open.textContent = key;
    open.addEventListener("click", () => void readKey(key));
    item.appendChild(open);
    list.appendChild(item);
  }
  at("kv-list").appendChild(list);
}

async function readKey(key: string): Promise<void> {
  const where = base();
  clear("kv-value");
  if (where === null) {
    return;
  }
  try {
    const { status, body } = await route("GET", `${where}/key/${encodeURIComponent(key)}`, SCREEN);
    const shown = made("pre");
    if (status === 404) {
      shown.textContent = `${key}: no such key — it may have expired`;
    } else {
      const { value: held, ttl } = body as { value?: unknown; ttl?: unknown };
      const left = typeof ttl === "string" ? `expires in ${ttl}` : "never expires";
      shown.textContent = `${key} — ${left}\n${JSON.stringify(held, null, 2)}`;
    }
    at("kv-value").appendChild(shown);
  } catch (failure) {
    state("kv-status", "wrong", words(failure));
  }
}

export function wire(): void {
  at("kv-namespace").addEventListener("change", () => void readDatabases());
  at("kv-database").addEventListener("change", () => void readSpaces());
  at("kv-space").addEventListener("change", () => void listKeys());
  at("kv-list-them").addEventListener("click", () => void listKeys());
  at("kv-refresh").addEventListener("click", () => void readNamespaces());
  onArrival(["run"], () => void readNamespaces());
}
