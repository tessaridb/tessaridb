//! The Topics screen's reading: where, which topics, the chosen one, its messages.
//!
//! Everything here reads. Nothing moves a position — the message browser uses
//! `READ FROM … AFTER n` with no consumer, which is a look and nothing more.

import { held as valueHeld, ask, Unreachable, valueOf, type Answer, type Result } from "./api.js";
import { at, clear, disable, made, say, setValue, trimmed, value } from "./dom.js";
import { facts } from "./draw.js";
import { told } from "./session.js";
import { settled, state } from "./states.js";
import { onArrival } from "./tabs.js";
import { behind, fieldsOf, held, topic as asTopic, type Topic } from "./topic-info.js";
import { aName, aWhole, tenancy } from "./topic-names.js";

const SCREEN = "Topics";

/** The most topics the list draws; the rest are counted, not drawn. */
const TOPICS_SHOWN = 200;

/** A namespace and database that passed the name check, or `null`. */
export function where(): { namespace: string; database: string } | null {
  const namespace = aName(value("topics-namespace"));
  const database = aName(value("topics-database"));
  return namespace === null || database === null ? null : { namespace, database };
}

let topics: Topic[] = [];
let chosenName: string | null = null;
const listeners: (() => void)[] = [];

/** The topic the reader chose, as the node last described it. */
export const chosen = (): Topic | null => topics.find((each) => each.name === chosenName) ?? null;

/** Every topic the list holds, for forms that offer one by name. */
export const known = (): readonly Topic[] => topics;

/** Run `todo` whenever what is chosen, or what is known about it, changes. */
export function whenChosen(todo: () => void): void {
  listeners.push(todo);
}

const words = (failure: unknown): string =>
  failure instanceof Unreachable ? "the node did not answer — " + told(failure) : told(failure);

/** Every result of one script, or a thrown refusal. */
async function results(source: string): Promise<readonly Result[]> {
  const { text } = await ask(source, SCREEN);
  const body = JSON.parse(text) as Answer;
  if (!Array.isArray(body.results)) {
    throw new Error(typeof body.error === "string" ? body.error : text);
  }
  return body.results;
}

function strings(answered: Result | null, field: string): string[] {
  const list = valueHeld(answered)?.[field];
  return Array.isArray(list) ? list.filter((each): each is string => typeof each === "string") : [];
}

/** Fill a select with names, keeping the choice when it is still offered. */
export function offer(id: string, names: readonly string[]): void {
  const select = at(id);
  const kept = value(id);
  clear(id);
  for (const name of names) {
    const option = made("option");
    option.value = name;
    option.textContent = name;
    select.appendChild(option);
  }
  if (names.includes(kept)) {
    setValue(id, kept);
  }
}

async function readNamespaces(): Promise<void> {
  state("topics-status", "waiting", "asking…");
  try {
    offer("topics-namespace", strings(await valueOf("INFO FOR STORE;", SCREEN), "namespaces"));
  } catch (failure) {
    state("topics-status", "wrong", words(failure));
    return;
  }
  await readDatabases();
}

async function readDatabases(): Promise<void> {
  const namespace = aName(value("topics-namespace"));
  if (namespace === null) {
    offer("topics-database", []);
    draw([]);
    state("topics-status", "empty", "No namespace here — declare one on Run with DEFINE NAMESPACE.");
    return;
  }
  try {
    const answered = await valueOf(`USE NAMESPACE ${namespace}; INFO FOR NAMESPACE;`, SCREEN);
    offer("topics-database", strings(answered, "databases"));
  } catch (failure) {
    state("topics-status", "wrong", words(failure));
    return;
  }
  await readTopics();
}

/** Read every topic in the chosen database, and what each holds. */
export async function readTopics(): Promise<void> {
  const place = where();
  if (place === null) {
    draw([]);
    state("topics-status", "empty", "Choose a namespace and a database that exist.");
    return;
  }
  const start = tenancy(place.namespace, place.database);
  state("topics-status", "waiting", "asking…");
  try {
    const listed = strings(await valueOf(start + "INFO FOR DATABASE;", SCREEN), "topics");
    const names = listed.filter((name) => aName(name) !== null);
    const shown = names.slice(0, TOPICS_SHOWN);
    const asked = shown.map((name) => `INFO FOR TOPIC ${name};`).join(" ");
    const answered = shown.length === 0 ? [] : await results(start + asked);
    const read = answered
      .map((each) => asTopic(each.value))
      .filter((each): each is Topic => each !== null);
    draw(read);
    if (names.length === 0) {
      const here = `${place.namespace}.${place.database}`;
      state("topics-status", "empty", `${here} holds no topics — create one below.`);
    } else if (names.length > shown.length) {
      state("topics-status", "partial", `showing ${shown.length} of ${names.length} topics`);
    } else {
      settled("topics-status");
    }
  } catch (failure) {
    draw([]);
    state("topics-status", "wrong", words(failure));
  }
}

function numberCell(row: HTMLTableRowElement, figure: number | string): void {
  const box = row.insertCell();
  box.textContent = String(figure);
  box.classList.add("number");
}

function headed(names: readonly string[]): HTMLTableElement {
  const table = made("table");
  const head = table.createTHead().insertRow();
  for (const name of names) {
    const column = made("th");
    column.textContent = name;
    head.appendChild(column);
  }
  return table;
}

function draw(read: Topic[]): void {
  topics = read;
  if (chosen() === null) {
    chosenName = null;
  }
  clear("topics-list");
  if (read.length > 0) {
    const table = headed(["topic", "held", "last", "keeps", "readers", "groups", "most behind"]);
    const body = table.createTBody();
    for (const each of read) {
      const row = body.insertRow();
      const pick = made("button", each.name === chosenName ? "quiet chosen" : "quiet");
      pick.type = "button";
      pick.textContent = each.name;
      pick.setAttribute("aria-pressed", String(each.name === chosenName));
      pick.addEventListener("click", () => choose(each.name));
      row.insertCell().appendChild(pick);
      numberCell(row, held(each));
      numberCell(row, each.last);
      row.insertCell().textContent = each.retain ?? "everything";
      numberCell(row, each.readers.size);
      numberCell(row, each.groups.size);
      numberCell(row, behind(each));
    }
    at("topics-list").appendChild(table);
  }
  drawChosen();
}

function choose(name: string): void {
  chosenName = name;
  const found = chosen();
  setValue("browse-after", String(found === null || found.first === null ? 0 : found.first - 1));
  clear("browse-list");
  settled("browse-status");
  draw(topics);
}

function drawChosen(): void {
  const found = chosen();
  at("topic-chosen").textContent = found?.name ?? "none chosen";
  for (const id of ["topic-facts", "topic-readers", "topic-groups"]) {
    clear(id);
  }
  disable("browse-read", found === null);
  disable("browse-next", found === null);
  if (found !== null) {
    facts("topic-facts", {
      held: held(found),
      first: found.first ?? "none held",
      last: found.last,
      keeps: found.retain ?? "everything",
      "largest message": found.maxBytes === null ? "any size" : `${found.maxBytes} bytes`,
    });
    const readers = headed(["reader", "position", "lag"]);
    const readerRows = readers.createTBody();
    for (const [name, each] of found.readers) {
      const row = readerRows.insertRow();
      row.insertCell().textContent = name;
      numberCell(row, each.position);
      numberCell(row, each.lag);
    }
    at("topic-readers").appendChild(found.readers.size > 0 ? readers : note("nobody reads under a name"));
    const groups = headed([
      "group", "handed out", "committed", "lag", "in flight", "width", "redelivered",
      "dead letters", "deadline",
    ]);
    const groupRows = groups.createTBody();
    for (const [name, each] of found.groups) {
      const row = groupRows.insertRow();
      row.insertCell().textContent = name;
      const figures = [
        each.position, each.committed, each.lag, each.in_flight, each.width, each.redelivered,
        each.dead_lettered,
      ];
      for (const figure of figures) {
        numberCell(row, figure);
      }
      row.insertCell().textContent = each.deadline;
    }
    at("topic-groups").appendChild(found.groups.size > 0 ? groups : note("no group settles this topic"));
  }
  for (const todo of listeners) {
    todo();
  }
}

function note(words: string): HTMLParagraphElement {
  const line = made("p", "empty");
  line.textContent = words;
  return line;
}

async function browse(): Promise<void> {
  const place = where();
  const found = chosen();
  const after = aWhole(trimmed("browse-after"));
  const count = aWhole(trimmed("browse-count"));
  if (place === null || found === null || after === null || count === null || count < 1 || count > 100) {
    say("browse-status", "a position from 0 and a count from 1 to 100", true);
    return;
  }
  state("browse-status", "waiting", "reading…");
  try {
    const answered = await valueOf(
      tenancy(place.namespace, place.database) + `READ FROM ${found.name} AFTER ${after} LIMIT ${count};`,
      SCREEN,
    );
    const records = answered?.records ?? [];
    clear("browse-list");
    const table = headed(["position", "message"]);
    const body = table.createTBody();
    let last = after;
    for (const record of records) {
      const inside = fieldsOf(record.value) ?? {};
      const position = typeof inside["position"] === "number" ? inside["position"] : last;
      last = Math.max(last, position);
      const row = body.insertRow();
      numberCell(row, position);
      row.insertCell().textContent = JSON.stringify(inside["value"] ?? null);
    }
    at("browse-list").appendChild(table);
    at("browse-next").dataset["after"] = String(last);
    const said = (answered?.notes ?? []).map((each) => each.message).join(" · ");
    const also = said === "" ? "" : ` — ${said}`;
    if (records.length === 0) {
      state("browse-status", "empty", `nothing after position ${after}${also}`);
    } else {
      say("browse-status", `${records.length} from position ${after + 1}${also}`);
    }
  } catch (failure) {
    state("browse-status", "wrong", words(failure));
  }
}

export function wire(): void {
  at("topics-namespace").addEventListener("change", () => void readDatabases());
  at("topics-database").addEventListener("change", () => void readTopics());
  at("topics-refresh").addEventListener("click", () => void readNamespaces());
  at("browse-read").addEventListener("click", () => void browse());
  at("browse-next").addEventListener("click", () => {
    setValue("browse-after", at("browse-next").dataset["after"] ?? trimmed("browse-after"));
    void browse();
  });
  onArrival(["topics"], () => void readNamespaces());
}
