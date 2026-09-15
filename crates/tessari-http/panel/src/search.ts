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

import { valueOf, type Result } from "./api.js";
import { hide as hideDetail, show as showDetail } from "./detail.js";
import { at, setValue, trimmed, write } from "./dom.js";
import { show } from "./tabs.js";

/** One thing the field might mean, and what to ask the node about it. */
interface Candidate {
  readonly kind: string;
  readonly statement: string;
  /** Where the answer belongs, once the node has confirmed the thing exists. */
  readonly land: (answered: Result | null) => void;
}

/**
 * Open the detail sheet on what was found.
 *
 * W316 routed these into Run with the statement pre-filled, and recorded that as
 * a compromise pending a detail surface. This is that surface: a namespace, a
 * database, a table and a record are all *things you look at*, and sending
 * somebody to the statement screen to look at one made the language the way in
 * again — the exact thing §5 of the brief corrected.
 */
const inDetail = (kind: string, name: string) => (answered: Result | null) =>
  showDetail(kind, name, answered);

/**
 * What `text` might be, in the order the questions are put.
 *
 * Deliberately a closed list rather than an open query surface: the field
 * accepts five shapes and composes nothing else, so it cannot become a way to
 * run arbitrary TessariQL from the top bar of every screen.
 */
/** Split once, at the first `separator`. */
function splitAt(text: string, separator: string): [string, string] {
  const at = text.indexOf(separator);
  return at < 0
    ? [text, ""]
    : [text.slice(0, at), text.slice(at + separator.length)];
}

function candidates(text: string): readonly Candidate[] {
  const record = text.includes(":");
  const qualified = text.includes(".") && !record;
  const [namespace, database] = qualified ? text.split(".", 2) : ["", ""];
  const out: Candidate[] = [];

  if (record) {
    // A record is read inside a selected namespace and database, and every
    // `/script` request is its own session — so a `USE` run on the Run screen
    // does not carry here, and a bare `orders:1` can never resolve. Measured,
    // not assumed: it was tried after selecting, and it still missed.
    //
    // So the qualified form is what the field accepts, and it carries its own
    // selection. `app.main.orders:1` is three parts and a key, which is also the
    // only shape that identifies a record without ambiguity when two databases
    // hold a table of the same name.
    const [reach, key] = splitAt(text, ":");
    const parts = reach.split(".");
    if (parts.length === 3) {
      const [namespaceOf, databaseOf, table] = parts as [string, string, string];
      out.push({
        kind: "record",
        statement:
          `USE NAMESPACE ${namespaceOf}; USE DATABASE ${databaseOf}; ` +
          `SELECT * FROM ${table}:${key};`,
        land: inDetail("record", text),
      });
    }
  }
  if (qualified) {
    out.push({
      kind: "database",
      statement:
        "USE NAMESPACE " + namespace + "; USE DATABASE " + database + "; INFO FOR DATABASE;",
      land: inDetail("database", text),
    });
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
    out.push({
      kind: "namespace",
      statement: "USE NAMESPACE " + text + "; INFO FOR NAMESPACE;",
      land: inDetail("namespace", text),
    });
    out.push({
      kind: "table",
      statement: "INFO FOR TABLE " + text + ";",
      land: inDetail("table", text),
    });
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
  // Closed before the first question. A miss used to leave the PREVIOUS thing's
  // sheet open, and a sheet showing something is read as the answer to what was
  // just asked — the console answering a question nobody asked with a fact about
  // something else.
  hideDetail();
  for (const candidate of candidates(text)) {
    try {
      const answered = await valueOf(candidate.statement, "Search · " + candidate.kind);
      write("search-says", "");
      candidate.land(answered);
      return;
    } catch {
      // A refusal here is an answer: this is not that kind of thing. The next
      // question is asked, and only the last failure is reported.
      continue;
    }
  }
  // Never a guess about WHY. The panel knows the node said no to every shape it
  // knows; it does not know whether the thing is absent or simply not the
  // caller's to see, and saying either would be inventing one.
  // A record key is answered against the SELECTED namespace and database, which
  // this field cannot guess — so the one shape with a likely innocent
  // explanation gets it, rather than the flat refusal the others deserve.
  write(
    "search-says",
    text.includes(":")
      ? "nothing here answers to that — name a record in full, as namespace.database.table:key"
      : "nothing here answers to that name",
  );
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
