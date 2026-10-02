//! One node, over the map.
//!
//! A drawer rather than a screen, because the map is the context the decision is
//! being made in: an operator looking at a node during a failover is looking at
//! it *relative to the others*, and a navigation that replaces the view takes
//! away the reason they opened it.
//!
//! # It carries two actions, and still says why it is not three
//!
//! The band asks for three — a role change, a drain, and a hand-over. Two have
//! a statement behind them, and each was searched rather than assumed:
//!
//! - **drain** — `DEFINE NODE ROLES NONE` clears the roles of the node the
//!   statement runs on. It did not exist when this drawer was built: `ROLES
//!   NONE` was a parse error, there was no `DRAIN`, and omitting `ROLES` means
//!   *leave them alone*, so the empty role set was a state the store could hold
//!   and no statement could ask for. It exists now.
//! - **hand-over** — still no `HANDOVER`, `STEP DOWN` or `YIELD` in the
//!   grammar, so the drawer names it and offers no control for it.
//!
//! A button that composes no statement is a button that lies, and on this screen
//! it would lie about the one thing an operator opens the screen to do. The
//! drain stopped being one the day the statement landed; the hand-over has not.
//!
//! # The drain says what it costs, and what may undo it
//!
//! Draining is the destructive action S2.1 names on this screen: the node keeps
//! its data and its place in the membership and stops answering clients, so the
//! radius line names that before the statement rather than after it.
//!
//! It also names the one way the statement does not stick. `DEFINE NODE` is
//! local and immediate, and on a node a membership row declares a role for, a
//! local drain is an override the next open discards — the desired role is the
//! shared truth. An operator who drains a bound node and walks away has done
//! nothing that survives, which is the slow kind of lie, so the drawer says so
//! while the decision is still being made.
//!
//! # A peer is amended one clause at a time, and removed for good
//!
//! The drawer first offered the role change on this node only, because a
//! running node refused every way of changing a peer's row — a second `DEFINE
//! REPLICA` for the name in use, an `ALTER` that took no `REPLICA`. `ALTER
//! REPLICA <name> ROLES …` exists now (Q-892) and changes that one clause,
//! leaving the row's node, subscription and pinned certificate as they are. It
//! is a write to the membership, so it is taken by the node that leads the store
//! and refused, in the node's words, anywhere else.
//!
//! Removing a peer is `DROP REPLICA`, and it is not a role change with a bigger
//! button: the dropped node is recorded as removed and never admitted again
//! (ADR-0108 D9), so a machine coming back must be wiped and join under a new
//! identity. The drawer therefore asks for the name typed again and says so
//! before the button is live.

import { valueOf } from "./api.js";
import { at, clear, disable, hide, made, say, setValue, trimmed } from "./dom.js";
import { told } from "./session.js";
import { hereAgain } from "./tabs.js";
import { aName } from "./topic-names.js";

/** What the drawer is open on. `null` when it is closed. */
export interface Subject {
  readonly name: string;
  readonly self: boolean;
  readonly endpoint: string | null;
  readonly node: string | null;
  readonly roles: readonly string[];
  /**
   * The roles a membership row declares for this node, or `null` when none
   * does. Read by the drain, which a declaration outlives.
   */
  readonly declared: readonly string[] | null;
}

let open: Subject | null = null;

const BITS = ["serving", "writable", "coordinating"] as const;

/** The roles the drawer's ticks currently describe. */
function ticked(): string[] {
  return BITS.filter((bit) => (at(`drawer-${bit}`) as HTMLInputElement).checked);
}

/**
 * The statement the drawer would send.
 *
 * Exported so the preview and the button read one value. `null` when there is
 * nothing to send — no subject, or a peer whose row this node cannot rewrite
 * because it never recorded the address and id the declaration needs.
 */
export function change(subject: Subject | null, roles: readonly string[]): string | null {
  // A peer first: its roles are a clause of its membership row. This node's
  // own statement below reaches the local `META` keyspace and works on a
  // clustered node, and must never be what a peer's drawer sends.
  if (subject === null) {
    return null;
  }
  if (!subject.self) {
    // A peer's row, one clause. A name is grammar, so it is checked rather
    // than quoted; a row whose name is not one cannot be amended from here.
    return aName(subject.name) === null || roles.length === 0
      ? null
      : `ALTER REPLICA ${subject.name} ROLES ${roles.join(", ")};`;
  }
  // `NONE` is a whole answer and not a member of the list, so an empty tick set
  // composes it rather than composing `ROLES ;`, which the grammar refuses.
  return roles.length === 0
    ? `DEFINE NODE ROLES NONE;`
    : `DEFINE NODE ROLES ${roles.join(", ")};`;
}

/** What pressing the button will do, in words. */
function preview(): void {
  if (open === null) {
    return;
  }
  if (!open.self) {
    const roles = ticked();
    say(
      "drawer-says",
      roles.length === 0
        ? `A peer keeps at least one role; to take it out of the cluster, remove it below.`
        : `Sets ${open.name}'s declared roles to ${roles.join(", ")}, leaving its node, ` +
            `subscription and certificate as they are. Taken by the node that leads the store.`,
    );
    return;
  }
  const roles = ticked();
  if (roles.length === 0) {
    const kept =
      `Drains this node: it keeps its data, its identity and its place in the ` +
      `membership, and stops answering clients and accepting writes until a role ` +
      `is declared here again.`;
    const declared = open.declared;
    say(
      "drawer-says",
      declared === null
        ? kept
        : `${kept} The membership declares ${declared.join(", ")} for this node, so ` +
            `this is a local override the next open discards — to drain it for good, ` +
            `write the membership row instead.`,
    );
    return;
  }
  say("drawer-says", `Sets this node's roles to ${roles.join(", ")}.`);
}

export function show(subject: Subject): void {
  open = subject;
  at("drawer-title").textContent = subject.self ? "This node" : subject.name;
  for (const bit of BITS) {
    (at(`drawer-${bit}`) as HTMLInputElement).checked = subject.roles.includes(bit);
  }
  clear("drawer-missing");
  hide("drawer-remove-part", subject.self);
  setValue("drawer-remove-confirm", "");
  shapeRemove();
  if (!subject.self) {
    const note = made("p", "faint");
    note.textContent =
      `${subject.name} answers on ${subject.endpoint ?? "an address this node did not record"}.`;
    at("drawer-missing").appendChild(note);
  }
  say("drawer-status", "");
  preview();
  hide("drawer", false);
  at("drawer-close").focus();
}

/** The removal, once its name has been typed again; `null` until then. */
function removal(): string | null {
  if (open === null || open.self || aName(open.name) === null) {
    return null;
  }
  return trimmed("drawer-remove-confirm") === open.name ? `DROP REPLICA ${open.name};` : null;
}

function shapeRemove(): void {
  disable("drawer-remove", removal() === null);
  say(
    "drawer-remove-says",
    open === null || open.self
      ? ""
      : `Removes ${open.name} from the membership. Its node is never admitted again — ` +
          `a machine coming back is wiped and joins under a new identity. Type ${open.name} to confirm.`,
  );
}

function closeIt(): void {
  hide("drawer", true);
  open = null;
}

export function wire(): void {
  for (const bit of BITS) {
    at(`drawer-${bit}`).addEventListener("change", preview);
  }
  at("drawer-close").addEventListener("click", closeIt);
  at("drawer-remove-confirm").addEventListener("input", shapeRemove);
  at("drawer-remove").addEventListener("click", async () => {
    const statement = removal();
    if (statement === null) {
      return;
    }
    say("drawer-remove-status", "running…");
    try {
      await valueOf(statement, "Cluster · remove", trimmed("drawer-remove-why") || undefined);
      say("drawer-remove-status", "removed");
      closeIt();
      hereAgain();
    } catch (failure) {
      say("drawer-remove-status", told(failure), true);
    }
  });
  document.addEventListener("keydown", (pressed) => {
    if (pressed.key === "Escape" && !at("drawer").hidden) {
      closeIt();
    }
  });

  at("drawer-apply").addEventListener("click", async () => {
    const statement = change(open, ticked());
    if (statement === null) {
      say("drawer-status", "there is nothing this drawer can send for that", true);
      return;
    }
    say("drawer-status", "running…");
    try {
      const answered = await valueOf(statement, "Cluster · roles");
      const done = answered !== null && answered.kind === "done";
      say("drawer-status", done ? "declared" : "");
      if (done) {
        // The drawer must not read the map back itself — the map is what draws
        // the drawer, so importing it here would be a cycle. The destination on
        // screen reads again instead, which is the same news without the
        // dependency.
        //
        // W319 declared `coordinating` from here against a running node, the
        // node accepted it, this line said `declared`, and the lamp two inches
        // away went on showing the role as not held. A console that reports a
        // change and then displays the state the change replaced is worse than
        // one that reports nothing.
        hereAgain();
      }
    } catch (failure) {
      say("drawer-status", told(failure), true);
    }
  });
}
