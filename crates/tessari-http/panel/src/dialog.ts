//! Opening and closing the console's dialogs.
//!
//! The markup is `ui.dialog`; this is the behaviour the browser does not supply.
//! Escape, the inert page behind and the focus trap are the native `<dialog>`'s
//! own. What is added here is the rest of the modal contract: the control that
//! opened a dialog gets focus back when it closes, and a click on the backdrop
//! closes it as Escape would.

import { at } from "./dom.js";

/** Who opened each dialog that is open, so focus goes back to them. */
const openers = new Map<string, HTMLElement>();

function element(id: string): HTMLDialogElement {
  const found = at(id);
  if (!(found instanceof HTMLDialogElement)) {
    throw new Error(`#${id} is not a dialog`);
  }
  return found;
}

/**
 * Open a dialog over the page and put the reader in its first field.
 *
 * The first field and not the close button: the dialog was opened to do the
 * thing it asks for, and starting on Close makes the reader tab past it every
 * time.
 */
export function openDialog(id: string): void {
  const shown = element(id);
  const from = document.activeElement;
  if (from instanceof HTMLElement) {
    openers.set(id, from);
  }
  if (!shown.open) {
    shown.showModal();
  }
  // The first one that is actually drawn: a field hidden by its label (the
  // change dialog's "what", a role it is not asking for) is still in the DOM.
  const first = Array.from(
    shown.querySelectorAll<HTMLElement>(
      ".dialog-body input:not([readonly]), .dialog-body select, .dialog-body textarea, .dialog-body button",
    ),
  ).find((candidate) => candidate.offsetParent !== null);
  first?.focus();
}

export function closeDialog(id: string): void {
  const shown = element(id);
  if (shown.open) {
    shown.close();
  }
}

export function wire(): void {
  for (const shown of Array.from(document.querySelectorAll("dialog"))) {
    shown.addEventListener("close", () => {
      const back = openers.get(shown.id);
      openers.delete(shown.id);
      // Only while it is still on the page and visible: the row that opened a
      // removal is gone once the listing is redrawn without it.
      if (back !== undefined && back.isConnected && back.offsetParent !== null) {
        back.focus();
      } else {
        // The opener went with what the dialog did — a removed account's row —
        // so focus lands on the screen's heading rather than nowhere.
        at("view-title").focus();
      }
    });
    // A click whose target is the dialog itself landed on the backdrop — every
    // click inside the box lands on something the box contains.
    shown.addEventListener("click", (event) => {
      if (event.target === shown) {
        shown.close();
      }
    });
    for (const closer of Array.from(shown.querySelectorAll("[data-close]"))) {
      closer.addEventListener("click", () => shown.close());
    }
  }
}
