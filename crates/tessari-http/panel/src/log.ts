//! What the panel did, on the operator's behalf.
//!
//! Every other screen composes TessariQL and sends it without showing it. That
//! is only honest because it is still recoverable, and this is where it is
//! recovered from: the statement as sent, the node's own words back, how long it
//! took, and which screen issued it.
//!
//! Two things it is deliberately not. It is not the store's audit trail —
//! `INFO FOR AUDIT` answers a different question, for a different reader, with a
//! durability this makes no claim to. And it is not persisted: a record of who
//! was administered and when, left behind on a shared operator's machine, is a
//! disclosure nobody asked for. It lives as long as the tab does.

import { at, clear, hide, made, setValue, write } from "./dom.js";
import { show } from "./tabs.js";

/**
 * A password, drawn the way the store draws it.
 *
 * Every occurrence and not just the last, because the Run screen sends whatever
 * was typed into it and a script may carry several. The alternation matches the
 * escaping `quoted()` writes, so a password containing a quote does not end the
 * match early and leave its tail in the clear.
 */
export const redacted = (statement: string): string =>
  statement.replace(/PASSWORD\s+'(?:[^'\\]|\\.)*'/gi, "PASSWORD '…'");

/** One thing the panel did. */
export interface Entry {
  /** The statement as sent, or — for the one action that is not a statement —
   *  what it did, in the operator's words. */
  readonly what: string;
  /** The node's answer, or its refusal, in the node's own words. */
  readonly said: string;
  readonly failed: boolean;
  readonly ms: number;
  /** The screen that issued it. */
  readonly screen: string;
  /**
   * Why the operator said they were doing it, on the actions that cost somebody
   * else their access.
   *
   * Absent everywhere else, because a field that is always there is a field
   * nobody reads. What it buys is deliberation rather than forensics: the store
   * writes no audit row for a user change, so this sentence lives exactly as
   * long as the tab does and answers "did they think about it", never "who did
   * this, last March".
   */
  // `| undefined` explicitly, because `exactOptionalPropertyTypes` distinguishes
  // "absent" from "present and undefined", and `ask` passes an argument that is
  // one of the two without knowing which.
  readonly why?: string | undefined;
}

// Newest first, which is the order it is read in. A cap because a long-lived tab
// on a busy node is otherwise an unbounded array nobody asked for; 200 is far
// past what anyone scrolls and far short of what anyone notices.
const CAP = 200;
const kept: Entry[] = [];

export const entries = (): readonly Entry[] => kept;
export const count = (): number => kept.length;

/** Record one thing the panel did, and keep the count in the bar current. */
export function record(entry: Entry): void {
  kept.unshift({ ...entry, what: redacted(entry.what) });
  if (kept.length > CAP) {
    kept.length = CAP;
  }
  write("log-count", String(kept.length));
  if (!at("log-sheet").hidden) {
    draw();
  }
}

/** Put a statement back where it can be edited and run. */
function reopen(what: string): void {
  setValue("script", what);
  hide("log-sheet", true);
  show("query");
  at("script").focus();
}

/**
 * Hand the statement to the operator to keep.
 *
 * `navigator.clipboard` needs a secure context, and this console is served over
 * plain HTTP by design — so on most nodes it is simply absent. Selecting the
 * text is the honest fallback: it leaves the operator one keystroke away rather
 * than reporting a copy that did not happen.
 */
function copy(what: string, where: HTMLElement, said: HTMLElement): void {
  const clipboard = navigator.clipboard as Clipboard | undefined;
  if (clipboard === undefined) {
    const range = document.createRange();
    range.selectNodeContents(where);
    const selection = window.getSelection();
    selection?.removeAllRanges();
    selection?.addRange(range);
    said.textContent = "selected — ⌘C or Ctrl-C";
    return;
  }
  void clipboard.writeText(what).then(
    () => {
      said.textContent = "copied";
    },
    () => {
      said.textContent = "the browser would not copy it";
    },
  );
}

function entry(one: Entry): HTMLElement {
  const row = made("div", "logged");
  const head = made("div", "logged-head");
  const screen = made("span", "faint");
  screen.textContent = one.screen;
  const took = made("span", "faint");
  took.textContent = one.ms + " ms";
  head.append(screen, took);

  const what = made("pre", "logged-what");
  what.textContent = one.what;

  const said = made("p", one.failed ? "note warn" : "note");
  said.textContent = one.said;

  // Below the answer rather than above the statement: the reason is read when
  // somebody comes back to ask why, and it is noise on the way past.
  const why = made("p", "note faint");
  why.textContent = one.why === undefined ? "" : "Why: " + one.why;
  why.hidden = one.why === undefined;

  const actions = made("div", "row tight");
  const told = made("span", "status");
  const copied = made("button", "quiet");
  copied.type = "button";
  copied.textContent = "Copy";
  copied.addEventListener("click", () => copy(one.what, what, told));
  const opened = made("button", "quiet");
  opened.type = "button";
  opened.textContent = "Open in Run";
  opened.addEventListener("click", () => reopen(one.what));
  actions.append(copied, opened, told);

  row.append(head, what, said, why, actions);
  return row;
}

function draw(): void {
  clear("log-list");
  if (kept.length === 0) {
    const empty = made("p", "note");
    empty.textContent = "Nothing yet. Everything this panel sends on your behalf lands here.";
    at("log-list").appendChild(empty);
    return;
  }
  const list = made("div");
  for (const one of kept) {
    list.appendChild(entry(one));
  }
  at("log-list").appendChild(list);
}

function closeIt(): void {
  hide("log-sheet", true);
  at("log-open").setAttribute("aria-expanded", "false");
}

export function wire(): void {
  write("log-count", "0");

  at("log-open").addEventListener("click", () => {
    // It opens on being asked and never on its own: a panel that appears when
    // something happens is a panel that appears while you are typing.
    const opening = at("log-sheet").hidden;
    hide("log-sheet", !opening);
    at("log-open").setAttribute("aria-expanded", String(opening));
    if (opening) {
      draw();
    }
  });

  at("log-close").addEventListener("click", closeIt);

  document.addEventListener("keydown", (pressed) => {
    if (pressed.key === "Escape" && !at("log-sheet").hidden) {
      closeIt();
    }
  });
}
