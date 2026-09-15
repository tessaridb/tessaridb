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
 * Do this every time any of `names` is reached — however it is reached.
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
 *
 * EVERY time, and not once. W319 introduced this as one-shot and W320 measured
 * what that costs: arrive at a destination before signing in, its read is
 * refused, the registration is spent, and the screen stays on that refusal for
 * the life of the tab — signing in does not recover it, and neither does
 * leaving and coming back. Three screens were reachable that way, one of them
 * by following a shared `#cluster` link. A read that happens on arrival costs
 * nothing while nobody is arriving, which was the whole reason it is not on
 * load; it does not also have to be the only one.
 */
export function onArrival(names: readonly string[], todo: () => void): void {
  arrivals.push({ names, todo });
  if (names.includes(here())) {
    todo();
  }
}

/**
 * Everything registered for the destination ON SCREEN reads again.
 *
 * For the news that arrives while you are already standing there, which an
 * arrival by definition cannot carry: the identity changed, or a declaration
 * landed. Both make what is drawn an answer to a question nobody is asking any
 * more — the account list of whoever was signed in a moment ago, or a cluster
 * map drawn before the membership that is now declared.
 *
 * Only the destination on screen, because the others will read on arrival.
 */
export function hereAgain(): void {
  for (const arrival of arrivals) {
    if (arrival.names.includes(here())) {
      arrival.todo();
    }
  }
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
  for (const arrival of arrivals) {
    if (arrival.names.includes(wanted)) {
      arrival.todo();
    }
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
