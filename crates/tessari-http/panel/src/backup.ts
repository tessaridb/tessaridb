//! The Backup screen: `BACKUP … TO '<name>'`, and where the file landed.
//!
//! The name is the operator's text, so it reaches the statement only through
//! `quoted()`, the one place the console escapes a string literal; whether the
//! name stays inside the backup folder is the node's decision, and its refusal
//! is shown in its own words.

import { Unreachable, held, valueOf } from "./api.js";
import { at, disable, say, setValue, trimmed, value, write } from "./dom.js";
import { told } from "./session.js";
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

/** `tessaridb-20260930-194500.tessarisnap`, in UTC, so two names sort as their times do. */
function suggested(form: Form): string {
  const stamp = new Date().toISOString().replace(/[-:]/g, "").replace("T", "-").slice(0, 15);
  return `tessaridb-${stamp}.${WHAT[form].suffix}`;
}

/** Whether the name is still one this screen suggested, and so may follow the form. */
let ours = true;

function shape(): void {
  const name = trimmed("backup-name");
  disable("backup-run", name === "");
  write(
    "backup-says",
    name === ""
      ? "name the file to write"
      : `Writes ${name} into the node's backup folder: ${WHAT[chosenForm()].says}.`,
  );
}

async function run(): Promise<void> {
  const name = trimmed("backup-name");
  if (name === "") {
    return;
  }
  disable("backup-run", true);
  say("backup-status", "writing…");
  write("backup-answer", "");
  try {
    const answered = held(await valueOf(`${WHAT[chosenForm()].statement} TO ${quoted(name)};`, SCREEN));
    const path = typeof answered?.path === "string" ? answered.path : name;
    const bytes = typeof answered?.bytes === "number" ? answered.bytes : null;
    say("backup-status", "written");
    write("backup-answer", bytes === null ? path : `${path}\n${bytes.toLocaleString("en")} bytes`);
    ours = true;
    setValue("backup-name", suggested(chosenForm()));
  } catch (failure) {
    const words = told(failure);
    say("backup-status", failure instanceof Unreachable ? "the node did not answer — " + words : words, true);
  }
  shape();
}

export function wire(): void {
  setValue("backup-name", suggested(chosenForm()));
  at("backup-form").addEventListener("change", () => {
    if (ours) {
      setValue("backup-name", suggested(chosenForm()));
    }
    shape();
  });
  at("backup-name").addEventListener("input", () => {
    ours = false;
    shape();
  });
  at("backup-run").addEventListener("click", () => void run());
  shape();
}
