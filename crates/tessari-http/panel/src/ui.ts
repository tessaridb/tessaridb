//! The console's vocabulary.
//!
//! Every name here was read off the markup that already existed rather than
//! invented for it: the panel had been writing `pane`, `pane-head`, `split`,
//! `row`, `note` and `answer` by hand in five hundred places, which is a
//! component model that nobody had written down. This writes it down.
//!
//! The point of the band is that a new screen composes these instead of
//! starting again at `document.createElement`. If adding a screen is not
//! cheaper after this than before it, the model is wrong and the fix is here,
//! not in the screen.

import { el, type Child, type Node } from "./html.js";

/** A heading with whatever sits opposite it — a status, a refresh, a choice. */
export const paneHead = (heading: string, ...opposite: Child[]): Node =>
  el("div", { class: "pane-head" }, el("h2", {}, heading), ...opposite);

/** A bordered region with one subject. */
export const pane = (...children: Child[]): Node => el("div", { class: "pane" }, ...children);

/** Two regions side by side, stacking when there is no room. */
export const split = (...children: Child[]): Node => el("div", { class: "split" }, ...children);

export type RowSpacing = "default" | "spread" | "tight";

export const row = (spacing: RowSpacing, ...children: Child[]): Node =>
  el("div", { class: spacing === "default" ? "row" : `row ${spacing}` }, ...children);

/**
 * A live region the node writes into. `role="status"` rather than an alert:
 * an operator reading a table should not have a screen reader interrupt them
 * because a refresh finished.
 */
export const status = (id: string): Node =>
  el("span", { id, class: "status", role: "status" });

/** Prose. `warn` is for a consequence, not for emphasis. */
export const note = (...children: Child[]): Node => el("p", { class: "note" }, ...children);

export const warning = (...children: Child[]): Node =>
  el("p", { class: "note warn" }, ...children);

/** Where an answer from the node lands. Never written to by the page itself. */
export const answer = (id: string, size: "full" | "small" = "full"): Node =>
  el("div", {
    id,
    class: size === "small" ? "answer small" : "answer",
    "aria-live": "polite",
  });

/** The statement a form has composed, shown before it is sent. */
export const preview = (id: string): Node => el("pre", { id, class: "answer small" });

/**
 * What pressing the button will do, in the reader's language.
 *
 * Distinct from `preview` on purpose: that one is a `pre` because a statement is
 * code and its whitespace is load-bearing, and this one is a paragraph because a
 * consequence is prose. Drawing a sentence in a monospaced box makes it look
 * like something to be parsed rather than read.
 */
export const says = (id: string): Node => el("p", { id, class: "note says" });

/** A statement kept reachable without being in the way. */
export const behindDisclosure = (summary: string, ...children: Child[]): Node =>
  el("details", { class: "statement" }, el("summary", {}, summary), ...children);

export type Weight = "primary" | "quiet" | "default";

export const button = (
  id: string | undefined,
  label: string,
  weight: Weight = "default",
  extra: Record<string, string | boolean> = {},
): Node =>
  el(
    "button",
    {
      ...(id === undefined ? {} : { id }),
      type: "button",
      ...(weight === "default" ? {} : { class: weight }),
      ...extra,
    },
    label,
  );

/**
 * A control and the words for it, in one label, so the words are the hit target
 * too. `for`/`id` pairing is the alternative and it is one more thing to keep
 * in step.
 */
export const field = (
  label: string,
  control: Node,
  attrs: Record<string, string | boolean> = {},
): Node =>
  // The space is the component's job, not the caller's. In hand-written markup
  // it came free from the newline between the words and the control; composed,
  // it has to be asked for, and asking twenty callers to remember it is asking
  // for the one that forgets. `User` flush against its own input box is not a
  // rendering detail, it is the label looking like part of the value.
  el("label", attrs, `${label.trimEnd()} `, control);

export const text = (id: string, attrs: Record<string, string | number | boolean> = {}): Node =>
  el("input", { id, type: "text", ...attrs });

export const secret = (id: string, autocomplete: "current-password" | "new-password"): Node =>
  el("input", { id, type: "password", autocomplete });

export const number = (id: string, attrs: Record<string, string | number> = {}): Node =>
  el("input", { id, type: "number", ...attrs });

export interface Choice {
  readonly value: string;
  readonly label: string;
  readonly chosen?: boolean;
}

export const choose = (id: string, choices: readonly Choice[]): Node =>
  el(
    "select",
    { id },
    ...choices.map((choice) =>
      el("option", { value: choice.value, selected: choice.chosen === true }, choice.label),
    ),
  );

/** The four roles a build knows, plus the escape hatch for one it does not. */
export const ROLES: readonly Choice[] = [
  { value: "viewer", label: "viewer" },
  { value: "editor", label: "editor" },
  { value: "owner", label: "owner" },
  { value: "other", label: "something else…" },
];

export interface Destination {
  /** The id of the tab, without its `tab-` prefix, and of the pane it reaches. */
  readonly name: string;
  readonly label: string;
}

/**
 * The tab strip and the panes are built from ONE list, so a tab that reaches
 * nothing is not expressible. Two of the panel's tests exist because it used to
 * be two lists.
 */
export const tabs = (destinations: readonly Destination[]): Node =>
  el(
    "nav",
    { class: "tabs" },
    el(
      "div",
      { role: "tablist", "aria-label": "Sections" },
      ...destinations.map((destination, at) =>
        el(
          "button",
          {
            type: "button",
            role: "tab",
            id: `tab-${destination.name}`,
            "aria-controls": `panel-${destination.name}`,
            "aria-selected": at === 0 ? "true" : "false",
            ...(at === 0 ? {} : { tabindex: -1 }),
          },
          destination.label,
        ),
      ),
    ),
  );

export const panel = (destination: Destination, first: boolean, ...children: Child[]): Node =>
  el(
    "section",
    {
      id: `panel-${destination.name}`,
      role: "tabpanel",
      "aria-labelledby": `tab-${destination.name}`,
      tabindex: 0,
      hidden: !first,
    },
    ...children,
  );
