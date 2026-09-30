//! The Backup screen: `BACKUP … TO '<name>'`, and `RESTORE SCRIPT FROM '<name>'`.
//!
//! A file name is the operator's text, so it reaches a statement only through
//! `quoted()`, the one place the console escapes a string literal. A place a
//! part names is grammar and cannot be quoted, so each one passes `aName()`
//! first and a name that fails is refused here, before anything is sent. Whether
//! a name stays inside the backup folder, and whether a restore may land, is the
//! node's decision, and its refusal is shown in its own words.

import { Unreachable, held, valueOf } from "./api.js";
import { at, disable, say, setValue, trimmed, value, write } from "./dom.js";
import { told } from "./session.js";
import { aName } from "./topic-names.js";
import { quoted } from "./user-forms.js";

const SCREEN = "Backup";

type Form = "state" | "log" | "script";

const WHAT: Readonly<Record<Form, { readonly statement: string; readonly suffix: string; readonly says: string }>> = {
  state: {
    statement: "BACKUP STATE",
    suffix: "tessarisnap",
    says: "every live record at one moment; restores whole, and a pruned log does not stop it",
  },
  log: {
    statement: "BACKUP",
    suffix: "tessarilog",
    says: "every commit in order; a restore can stop at any point in it",
  },
  script: {
    statement: "BACKUP SCRIPT",
    suffix: "tessariql",
    says: "statements that rebuild the store; readable, and its header lists what it leaves out",
  },
};

function chosenForm(): Form {
  const picked = value("backup-form");
  return picked === "log" || picked === "script" ? picked : "state";
}

const partial = (): boolean => value("backup-part") === "places";

/**
 * `NAMESPACE crm, DATABASE prod.orders` from `crm, prod.orders`, or the sentence
 * that says which name is not one.
 */
function places(): { readonly of: string } | { readonly missing: string } {
  const written = trimmed("backup-places")
    .split(",")
    .map((place) => place.trim())
    .filter((place) => place !== "");
  if (written.length === 0) {
    return { missing: "name at least one namespace or database, such as crm or prod.orders" };
  }
  const named: string[] = [];
  for (const place of written) {
    const [namespace, database, extra] = place.split(".");
    const within = aName(namespace ?? "");
    if (within === null || extra !== undefined) {
      return { missing: `${place} is not a namespace or namespace.database` };
    }
    if (database === undefined) {
      named.push(`NAMESPACE ${within}`);
      continue;
    }
    const inner = aName(database);
    if (inner === null) {
      return { missing: `${place} is not a namespace or namespace.database` };
    }
    named.push(`DATABASE ${within}.${inner}`);
  }
  return { of: named.join(", ") };
}

/** `tessaridb-20260930-194500.tessarisnap`, in UTC, so two names sort as their times do. */
function suggested(form: Form): string {
  const stamp = new Date().toISOString().replace(/[-:]/g, "").replace("T", "-").slice(0, 15);
  return `tessaridb-${stamp}.${WHAT[form].suffix}`;
}

type Composed = { readonly statement: string; readonly says: string } | { readonly missing: string };

function composeBackup(): Composed {
  const name = trimmed("backup-name");
  if (name === "") return { missing: "name the file to write" };
  const form = chosenForm();
  if (!partial()) {
    return {
      statement: `${WHAT[form].statement} TO ${quoted(name)};`,
      says: `Writes ${name} into the node's backup folder: ${WHAT[form].says}.`,
    };
  }
  if (form !== "script") {
    return { missing: "a part of the store is written as TessariQL; choose that form" };
  }
  const part = places();
  if ("missing" in part) return part;
  return {
    statement: `BACKUP SCRIPT OF ${part.of} TO ${quoted(name)};`,
    says:
      `Writes ${name} into the node's backup folder: ${part.of} as statements, with the analyzers ` +
      "their fields use; users belong to the whole store and stay out of it.",
  };
}

function composeRestore(): Composed {
  const name = trimmed("restore-name");
  if (name === "") return { missing: "name a script in the backup folder" };
  return {
    statement: `RESTORE SCRIPT FROM ${quoted(name)};`,
    says: `Runs ${name} from the node's backup folder, creating the databases it carries.`,
  };
}

/** Whether the name is still one this screen suggested, and so may follow the form. */
let ours = true;

function shape(): void {
  const backup = composeBackup();
  disable("backup-run", !("statement" in backup));
  write("backup-says", "statement" in backup ? backup.says : backup.missing);
  disable("backup-places", !partial());
  const restore = composeRestore();
  disable("restore-run", !("statement" in restore));
  write("restore-says", "statement" in restore ? restore.says : restore.missing);
}

async function send(
  composed: Composed,
  button: string,
  status: string,
  answer: string,
  shown: (answered: Record<string, unknown> | null) => string,
): Promise<boolean> {
  if (!("statement" in composed)) {
    return false;
  }
  disable(button, true);
  say(status, "working…");
  write(answer, "");
  try {
    write(answer, shown(held(await valueOf(composed.statement, SCREEN))));
    say(status, "done");
    return true;
  } catch (failure) {
    const words = told(failure);
    say(status, failure instanceof Unreachable ? "the node did not answer — " + words : words, true);
    return false;
  } finally {
    shape();
  }
}

async function backUp(): Promise<void> {
  const written = await send(composeBackup(), "backup-run", "backup-status", "backup-answer", (answered) => {
    const path = typeof answered?.path === "string" ? answered.path : trimmed("backup-name");
    const bytes = typeof answered?.bytes === "number" ? answered.bytes : null;
    return bytes === null ? path : `${path}\n${bytes.toLocaleString("en")} bytes`;
  });
  if (written) {
    ours = true;
    setValue("backup-name", suggested(chosenForm()));
    shape();
  }
}

async function restore(): Promise<void> {
  await send(composeRestore(), "restore-run", "restore-status", "restore-answer", (answered) => {
    const databases = Array.isArray(answered?.databases)
      ? answered.databases.filter((place): place is string => typeof place === "string")
      : [];
    const statements = typeof answered?.statements === "number" ? answered.statements : 0;
    return `${databases.join(", ") || "no database"} created · ${statements.toLocaleString("en")} statements`;
  });
}

export function wire(): void {
  setValue("backup-name", suggested(chosenForm()));
  for (const id of ["backup-form", "backup-part"]) {
    at(id).addEventListener("change", () => {
      if (ours) {
        setValue("backup-name", suggested(chosenForm()));
      }
      shape();
    });
  }
  at("backup-name").addEventListener("input", () => {
    ours = false;
    shape();
  });
  at("backup-places").addEventListener("input", shape);
  at("restore-name").addEventListener("input", shape);
  at("backup-run").addEventListener("click", () => void backUp());
  at("restore-run").addEventListener("click", () => void restore());
  shape();
}
