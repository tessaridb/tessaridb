//! Running the three account dialogs: create, change, remove.
//!
//! The statements are composed in `user-forms.ts`; this is the button that sends
//! one, refuses the click that would send it without a reason, and redraws the
//! list after. Each closes its dialog only on success, so a refusal stays on
//! screen beside the form that caused it.

import { valueOf } from "./api.js";
import { closeDialog } from "./dialog.js";
import { at, say, setValue } from "./dom.js";
import { decide, storeIsOpen } from "./gate.js";
import { told } from "./session.js";
import {
  alteration,
  changeMissing,
  changeWhy,
  definition,
  missing,
  removal,
  removeWhy,
  shapeTheForm,
  shapeTheRemoval,
} from "./user-forms.js";
import { listUsers } from "./users.js";

export function wire(): void {
  at("define").addEventListener("click", async () => {
    const statement = definition();
    if (statement === null) {
      say("define-status", missing(), true);
      return;
    }
    say("define-status", "running…");
    try {
      const answered = await valueOf(statement, "Users · define");
      if (answered === null || answered.kind !== "done") {
        say("define-status", "the node answered with something other than done — the log in the tray has it", true);
        return;
      }
      say("define-status", "");
      setValue("new-name", "");
      setValue("new-password", "");
      shapeTheForm();
      closeDialog("define-dialog");
      // The first user closes an open store, so the page asks again what it is:
      // the console the reader was using is now behind a sign-in.
      if (storeIsOpen()) {
        await decide();
        say("identity-status", "the store is closed now — sign in as the user you defined");
        return;
      }
      await listUsers();
    } catch (failure) {
      say("define-status", told(failure), true);
    }
  });

  at("change").addEventListener("click", async () => {
    const statement = alteration();
    if (statement === null) {
      say("change-status", changeMissing(), true);
      return;
    }
    // Refused here rather than by a dead button: a control that will not press
    // and says nothing leaves the operator looking for the field that is wrong,
    // and the whole point of the reason is that somebody articulates one.
    const why = changeWhy();
    if (why === "") {
      say("change-status", "say why — this ends their session and they sign in again", true);
      return;
    }
    say("change-status", "running…");
    try {
      await valueOf(statement, "Users · change", why);
      say("change-status", "");
      setValue("change-password", "");
      closeDialog("change-dialog");
      // The listing carries the role, so a role change that is not redrawn
      // leaves the old one on screen looking current.
      await listUsers();
    } catch (failure) {
      say("change-status", told(failure), true);
    }
  });

  at("remove").addEventListener("click", async () => {
    const statement = removal();
    if (statement === null) {
      say("remove-status", "the two names do not match", true);
      return;
    }
    const why = removeWhy();
    if (why === "") {
      say("remove-status", "say why — this does not come back", true);
      return;
    }
    say("remove-status", "running…");
    try {
      await valueOf(statement, "Users · remove", why);
      say("remove-status", "");
      // Cleared only on success, so a refusal leaves the name on screen to be
      // read — and never leaves a confirmed form one click from firing again.
      setValue("remove-name", "");
      setValue("remove-confirm", "");
      setValue("remove-why", "");
      shapeTheRemoval();
      closeDialog("remove-dialog");
      await listUsers();
    } catch (failure) {
      // The node's own words. A refusal here is the permission system working,
      // and paraphrasing it would hide which of the several reasons it was.
      say("remove-status", told(failure), true);
    }
  });
}
