//! Every key this console answers to, and one place that says so.
//!
//! A shortcut nobody can find is not reachable. Three handlers scattered through
//! three modules is what the console had: `/` and `⌘K` for the search, `⌘↵` for
//! the script, `Escape` for whatever was open — each real, each known only to
//! whoever wrote it or read the source.
//!
//! So the list below is the ONE place they are written down, the sheet renders
//! it, and `?` opens the sheet. The handlers still live with the screens they
//! belong to, because a module that owned every key on the page would be a
//! module that has to know about every screen.
//!
//! # Why the list is data and not prose
//!
//! It is rendered, so a shortcut added without a row here is a shortcut with no
//! row in the sheet — visible immediately to anybody who opens it, rather than
//! a documentation drift nobody notices for a year.

import { at, clear, made } from "./dom.js";
import { show } from "./tabs.js";

/** One key, and what it does, in the words the reader would use for it. */
interface Key {
  readonly press: string;
  readonly does: string;
}

const KEYS: readonly Key[] = [
  { press: "/", does: "find an account, a table, a namespace or a record" },
  { press: "⌘K  ·  Ctrl-K", does: "the same, from inside a field" },
  { press: "⌘1 … ⌘4", does: "Run, Cluster, Access, This node" },
  { press: "⌘↵  ·  Ctrl-↵", does: "run what is in the script box" },
  { press: "?", does: "this list" },
  { press: "Esc", does: "close the log, the drawer, or this list" },
];

/** Whether the caret is somewhere a bare letter is a letter. */
function typing(): boolean {
  const focused = document.activeElement;
  return (
    focused instanceof HTMLInputElement ||
    focused instanceof HTMLTextAreaElement ||
    focused instanceof HTMLSelectElement
  );
}

function draw(): void {
  clear("keys-list");
  const list = made("dl", "keys");
  for (const key of KEYS) {
    const press = made("dt");
    press.textContent = key.press;
    const does = made("dd");
    does.textContent = key.does;
    list.append(press, does);
  }
  at("keys-list").appendChild(list);
}

function closeIt(): void {
  at("keys-sheet").hidden = true;
}

/** The destinations, in the order the strip draws them. */
const DESTINATIONS = ["run", "cluster", "access", "this-node"] as const;

export function wire(): void {
  draw();

  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && !at("keys-sheet").hidden) {
      closeIt();
      return;
    }
    // A bare `?` while typing is a question mark, and a console that ate one
    // inside a statement would be worse than a console with no shortcut.
    if (event.key === "?" && !typing()) {
      event.preventDefault();
      at("keys-sheet").hidden = !at("keys-sheet").hidden;
      return;
    }
    if (!(event.metaKey || event.ctrlKey)) {
      return;
    }
    const at_ = Number.parseInt(event.key, 10) - 1;
    const wanted = DESTINATIONS[at_];
    if (wanted !== undefined) {
      event.preventDefault();
      show(wanted);
    }
  });

  at("keys-close").addEventListener("click", closeIt);
  at("keys-open").addEventListener("click", () => {
    at("keys-sheet").hidden = !at("keys-sheet").hidden;
  });
}
