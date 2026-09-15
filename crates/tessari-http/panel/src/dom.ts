//! Runtime DOM. Everything here runs in the browser.
//!
//! The build-time twin is `html.ts`, which renders one string and is then gone.
//! This reaches for elements already on the page and builds nodes beside them —
//! a different problem with the same subject, which is why they are two files.
//!
//! Every reach for an id goes through `at`, which throws when the id is not
//! there. The page and this code are emitted by the same build from the same
//! source, so a missing id is a build defect rather than a runtime condition to
//! handle politely, and failing loudly is what makes it findable.

/** The element with this id. Throws when the page carries none. */
export function at(id: string): HTMLElement {
  const found = document.getElementById(id);
  if (found === null) {
    throw new Error(`the page has no element #${id}`);
  }
  return found;
}

/** Every element matching a selector, as an array rather than a live list. */
export const all = (selector: string): HTMLElement[] =>
  Array.from(document.querySelectorAll<HTMLElement>(selector));

/** Anything the reader types into or chooses from. */
type Control = HTMLInputElement | HTMLTextAreaElement | HTMLSelectElement;

function control(id: string): Control {
  const found = at(id);
  if (
    found instanceof HTMLInputElement ||
    found instanceof HTMLTextAreaElement ||
    found instanceof HTMLSelectElement
  ) {
    return found;
  }
  throw new Error(`#${id} is not a control`);
}

/** What is in a control right now. */
export const value = (id: string): string => control(id).value;

/** What is in a control right now, with the ends trimmed. */
export const trimmed = (id: string): string => control(id).value.trim();

export function setValue(id: string, text: string): void {
  control(id).value = text;
}

/** Put text into an element. Text and never markup: see `made` below. */
export function write(id: string, words: string): void {
  at(id).textContent = words;
}

/** Empty an element out. */
export function clear(id: string): void {
  at(id).textContent = "";
}

export function hide(id: string, hidden: boolean): void {
  at(id).hidden = hidden;
}

export function disable(id: string, disabled: boolean): void {
  const found = at(id);
  if (found instanceof HTMLButtonElement || found instanceof HTMLInputElement) {
    found.disabled = disabled;
    return;
  }
  throw new Error(`#${id} cannot be disabled`);
}

/** Say something in a status line, marking a failure as one. */
export function say(id: string, words: string, failed?: boolean): void {
  const line = at(id);
  line.textContent = words;
  line.classList.toggle("failed", failed === true);
}

/** A fresh element, optionally classed. */
export function made<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className?: string,
): HTMLElementTagNameMap[K] {
  const element = document.createElement(tag);
  if (className !== undefined) {
    element.className = className;
  }
  return element;
}

/**
 * A one-line paragraph in the trailer voice, as text.
 *
 * `textContent` throughout this file and everywhere that draws: these values
 * come out of the store, out of a config file, or off the wire, and a record
 * that happens to hold a `<script>` tag is data rather than markup.
 */
export function trailer(words: string): HTMLParagraphElement {
  const line = made("p", "trailer");
  line.textContent = words;
  return line;
}

/** A `<pre>` holding a value as the JSON it is. */
export function shown(value: unknown): HTMLPreElement {
  const block = made("pre");
  block.textContent = typeof value === "string" ? value : JSON.stringify(value, null, 2);
  return block;
}
