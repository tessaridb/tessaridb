//! Which section is on screen.
//!
//! Hash routing, so a section is a link somebody can send and a refresh keeps
//! you where you were. A panel nobody can link to is a panel people describe to
//! each other in words.

import { all, at } from "./dom.js";

const tabs = (): HTMLElement[] => all('[role="tab"]');

/** The section this tab controls. Its absence is a build defect, not a state. */
function pane(tab: HTMLElement): HTMLElement {
  const named = tab.getAttribute("aria-controls");
  if (named === null) {
    throw new Error(`the tab #${tab.id} controls nothing`);
  }
  return at(named);
}

export function show(name: string): void {
  const wanted = tabs().some((tab) => tab.id === "tab-" + name) ? name : "query";
  for (const tab of tabs()) {
    const chosen = tab.id === "tab-" + wanted;
    tab.setAttribute("aria-selected", String(chosen));
    tab.tabIndex = chosen ? 0 : -1;
    pane(tab).hidden = !chosen;
  }
  if (window.location.hash !== "#" + wanted) {
    window.location.hash = wanted;
  }
}

export function wire(): void {
  for (const tab of tabs()) {
    tab.addEventListener("click", () => show(tab.id.replace("tab-", "")));
    // Arrow keys move between tabs, which is what a tablist owes anybody not
    // using a mouse — the roles alone promise it and do not provide it.
    tab.addEventListener("keydown", (event) => {
      const step = event.key === "ArrowRight" ? 1 : event.key === "ArrowLeft" ? -1 : 0;
      if (step === 0) {
        return;
      }
      event.preventDefault();
      const here = tabs();
      const next = here[(here.indexOf(tab) + step + here.length) % here.length];
      if (next === undefined) {
        return;
      }
      next.focus();
      show(next.id.replace("tab-", ""));
    });
  }

  window.addEventListener("hashchange", () =>
    show(window.location.hash.replace("#", "")),
  );

  show(window.location.hash.replace("#", ""));
}
