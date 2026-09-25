//! Running a script, and showing what came back.

import type { Answer, Result } from "./api.js";
import { ask } from "./api.js";
import { all, at, clear, made, say, shown, trailer, value } from "./dom.js";
import { drawn } from "./draw.js";
import { told } from "./session.js";

/** How the answer pane is drawing things: by shape, or as the JSON that came. */
let drawing = "auto";

/** The parsed body of the last answer, so the toggle can redraw without asking. */
let answered: Answer | null = null;

function one(pane: HTMLElement, result: Result): void {
  if (result.kind === "records" && Array.isArray(result.records)) {
    const records = result.records;
    const table = records.length === 0 ? null : drawn(records);
    if (table === null) {
      pane.appendChild(shown(records.length === 0 ? "(no records)" : records));
    } else {
      pane.appendChild(table);
    }
    // The trailer says how many and by which path. A scan should be visible
    // rather than folklore, which is why the store reports the path at all.
    pane.appendChild(
      trailer("(" + records.length + " record(s), via " + String(result.path) + ")"),
    );
    // A note is part of the answer, not decoration: dropping it discards the
    // only signal that, say, a read was gathered from several shards' leaders.
    for (const note of result.notes ?? []) {
      pane.appendChild(trailer("note " + note.kind + ": " + note.message));
    }
    return;
  }
  if (result.kind === "done") {
    // What the terminal prints for the same answer, and for the same reason: a
    // script's `USE` and `DEFINE` statements each answer, and three lines of
    // JSON apiece would bury the result somebody actually ran the script for.
    pane.appendChild(trailer("ok"));
    return;
  }
  pane.appendChild(shown(result));
}

export function paint(): void {
  const pane = at("answer");
  pane.textContent = "";
  if (answered === null) {
    return;
  }
  if (drawing === "json" || !Array.isArray(answered.results)) {
    pane.appendChild(shown(answered));
    return;
  }
  for (const result of answered.results) {
    one(pane, result);
  }
}

async function run(): Promise<void> {
  say("script-status", "running…");
  answered = null;
  clear("answer");
  try {
    const { reply, text } = await ask(value("script"), "Query");
    // Parsed when it is JSON and shown as it came when it is not: an error body
    // is plain text and reformatting it would only hide it.
    try {
      answered = JSON.parse(text) as Answer;
    } catch {
      answered = null;
      const block = made("pre");
      block.textContent = text;
      at("answer").appendChild(block);
    }
    paint();
    say("script-status", reply.status + " " + reply.statusText, reply.status >= 400);
  } catch (failure) {
    // A fetch rejects only when the request never got an answer, so this is a
    // connection problem and never a refusal from the node.
    say("script-status", "the node did not answer: " + told(failure), true);
  }
}

export function wire(): void {
  for (const button of all("[data-shape]")) {
    button.addEventListener("click", () => {
      drawing = button.dataset["shape"] ?? "auto";
      for (const other of all("[data-shape]")) {
        other.classList.toggle("chosen", other === button);
      }
      paint();
    });
  }

  at("run").addEventListener("click", run);

  at("script").addEventListener("keydown", (event) => {
    if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      void run();
    }
  });
}
