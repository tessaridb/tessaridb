//! Arriving with an identifier instead of a destination.

//!
//! An operator almost never opens a console to browse. They open it holding a
//! name — an account, a table, a record somebody escalated — and every step
//! between the door and that thing is a step they did not come for. So the
//! field is in the bar rather than behind a destination: search is the entry
//! point, and an entry point you have to navigate to first is not one.
//!
//! # It asks the node rather than guessing
//!
//! A bare word could be an account, a namespace or a table, and the panel has no
//! way to know which — so it does not decide. It builds an ORDERED list of
//! candidate questions and puts them to the node one at a time, landing on the
//! first that is answered. The store is the authority on what exists, which is
//! the same reason the listing draws the role the node reported rather than one
//! the panel inferred.
//!
//! The order is fixed and stated, because "whichever answers first" is only
//! unambiguous if the sequence is: a record key, then a database, then an
//! account, then a namespace, then a table. The two punctuated forms come first
//! because punctuation makes them unambiguous, and `ada` resolving to the
//! account before the table of the same name is the right guess for a console
//! whose destructive screens are all about accounts.

import { valueOf } from "./api.js";
import { at, setValue, trimmed, write } from "./dom.js";
import { show } from "./tabs.js";

/** One thing the field might mean, and what to ask the node about it. */
interface Candidate {
  readonly kind: string;
  readonly statement: string;
  /** Where the answer belongs, once the node has confirmed the thing exists. */
  readonly land: () => void;
}

/** Put the statement in Run and show its answer there. */
function inRun(statement: string): void {
  setValue("script", statement);
  show("run");
  at("run").click();
}

/**
 * What `text` might be, in the order the questions are put.
 *
 * Deliberately a closed list rather than an open query surface: the field
 * accepts five shapes and composes nothing else, so it cannot become a way to
 * run arbitrary TessariQL from the top bar of every screen.
 */
function candidates(text: string): readonly Candidate[] {
  const record = text.includes(":");
  const qualified = text.includes(".") && !record;
  const [namespace, database] = qualified ? text.split(".", 2) : ["", ""];
  const out: Candidate[] = [];

  if (record) {
    const statement = "SELECT * FROM " + text + ";";
    out.push({ kind: "record", statement, land: () => inRun(statement) });
  }
  if (qualified) {
    const statement =
      "USE NAMESPACE " + namespace + "; USE DATABASE " + database + "; INFO FOR DATABASE;";
    out.push({ kind: "database", statement, land: () => inRun(statement) });
  }
  if (!record && !qualified) {
    out.push({
      kind: "account",
      statement: "INFO FOR USER " + text + ";",
      // The account's own screen, which already draws a user as facts — landing
      // in Run would answer the question and lose the four things you reached
      // for the account in order to do.
      land: () => {
        show("access");
        setValue("lookup-name", text);
        at("lookup").click();
      },
    });
    const namespaceStatement = "USE NAMESPACE " + text + "; INFO FOR NAMESPACE;";
    out.push({
      kind: "namespace",
      statement: namespaceStatement,
      land: () => inRun(namespaceStatement),
    });
    const tableStatement = "INFO FOR TABLE " + text + ";";
    out.push({ kind: "table", statement: tableStatement, land: () => inRun(tableStatement) });
  }
  return out;
}

/** Put each question to the node in turn, and land on the first it answers. */
async function look(): Promise<void> {
  const text = trimmed("search");
  if (text === "") {
    return;
  }
  write("search-says", "looking…");
  for (const candidate of candidates(text)) {
    try {
      await valueOf(candidate.statement, "Search · " + candidate.kind);
    } catch {
      // A refusal here is an answer: this is not that kind of thing. The next
      // question is asked, and only the last failure is reported.
      continue;
    }
    write("search-says", "");
    candidate.land();
    return;
  }
  // Never a guess about WHY. The panel knows the node said no to every shape it
  // knows; it does not know whether the thing is absent or simply not the
  // caller's to see, and saying either would be inventing one.
  write("search-says", "nothing here answers to that name");
}

export function wire(): void {
  at("search").addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      void look();
    }
  });

  // `/` and ⌘K from anywhere. The first is the convention every text-heavy tool
  // shares; the second is what people who live in an editor reach for. Both
  // exist because the claim is *reachable from every destination*, and a
  // shortcut that only works on one screen would not be that.
  document.addEventListener("keydown", (event) => {
    const focused = document.activeElement;
    const typing =
      focused instanceof HTMLInputElement ||
      focused instanceof HTMLTextAreaElement ||
      focused instanceof HTMLSelectElement;
    const shortcut = event.key === "k" && (event.metaKey || event.ctrlKey);
    if (shortcut || (event.key === "/" && !typing)) {
      event.preventDefault();
      at("search").focus();
      (at("search") as HTMLInputElement).select();
    }
  });
}
