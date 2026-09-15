//! Which section is on screen.
//!
//! Hash routing, so a section is a link somebody can send and a refresh keeps
//! you where you were. A panel nobody can link to is a panel people describe to
//! each other in words.

import { all, at } from "./dom.js";

const tabs = (): HTMLElement[] => all('[role="tab"]');

/** The destination on screen right now. */
function here(): string {
  const chosen = tabs().find((tab) => tab.getAttribute("aria-selected") === "true");
  return chosen === undefined ? "" : chosen.id.replace("tab-", "");
}

/** Something to do the first time one of these destinations is reached. */
interface Arrival {
  readonly names: readonly string[];
  readonly todo: () => void;
}

const arrivals: Arrival[] = [];

/**
 * Do this once, the first time any of `names` is reached — however it is reached.
 *
 * The two screens that read on arrival used to listen for a CLICK on their own
 * tab, and every other way in left them blank AND silent: the console's own
 * ⌘1…⌘4, the arrow keys this tablist provides, a link somebody shared, a
 * reload. A cluster map that draws nothing when you follow a link to it is the
 * worst of the three, because an empty map is indistinguishable from a cluster
 * with nothing in it.
 *
 * `show` is the one place a destination becomes visible, so it is the one place
 * that can say so. Registering AFTER the destination is already on screen fires
 * immediately, so this does not depend on the start-up order staying right.
 */
export function onArrival(names: readonly string[], todo: () => void): void {
  if (names.includes(here())) {
    todo();
    return;
  }
  arrivals.push({ names, todo });
}

/** The section this tab controls. Its absence is a build defect, not a state. */
function pane(tab: HTMLElement): HTMLElement {
  const named = tab.getAttribute("aria-controls");
  if (named === null) {
    throw new Error(`the tab #${tab.id} controls nothing`);
  }
  return at(named);
}

export function show(name: string): void {
  const wanted = tabs().some((tab) => tab.id === "tab-" + name) ? name : "run";
  for (const tab of tabs()) {
    const chosen = tab.id === "tab-" + wanted;
    tab.setAttribute("aria-selected", String(chosen));
    tab.tabIndex = chosen ? 0 : -1;
    pane(tab).hidden = !chosen;
  }
  if (window.location.hash !== "#" + wanted) {
    window.location.hash = wanted;
  }
  for (const arrival of arrivals.filter((one) => one.names.includes(wanted))) {
    arrivals.splice(arrivals.indexOf(arrival), 1);
    arrival.todo();
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
