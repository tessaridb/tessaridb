"use strict";
(() => {
  // src/dom.ts
  //! Runtime DOM. Everything here runs in the browser.
  //!
  //! The build-time twin is `html.ts`, which renders one string and is then gone.
  //! This reaches for elements already on the page and builds nodes beside them —
  //! a different problem with the same subject, which is why they are two files.
  //!
  //! Every reach for an id goes through `at`, which throws when the id is not
  //! there. The page and this code are emitted by the same build from the same
  //! source, so a missing id is a build defect rather than a runtime condition to
  //! handle politely, and failing loudly is what makes it findable.
  function at(id) {
    const found = document.getElementById(id);
    if (found === null) {
      throw new Error(`the page has no element #${id}`);
    }
    return found;
  }
  var all = (selector) => Array.from(document.querySelectorAll(selector));
  function control(id) {
    const found = at(id);
    if (found instanceof HTMLInputElement || found instanceof HTMLTextAreaElement || found instanceof HTMLSelectElement) {
      return found;
    }
    throw new Error(`#${id} is not a control`);
  }
  var value = (id) => control(id).value;
  var trimmed = (id) => control(id).value.trim();
  function setValue(id, text) {
    control(id).value = text;
  }
  function write(id, words) {
    at(id).textContent = words;
  }
  function clear(id) {
    at(id).textContent = "";
  }
  function hide(id, hidden) {
    at(id).hidden = hidden;
  }
  function disable(id, disabled) {
    const found = at(id);
    if (found instanceof HTMLButtonElement || found instanceof HTMLInputElement) {
      found.disabled = disabled;
      return;
    }
    throw new Error(`#${id} cannot be disabled`);
  }
  function say(id, words, failed) {
    const line = at(id);
    line.textContent = words;
    line.classList.toggle("failed", failed === true);
  }
  function made(tag, className) {
    const element = document.createElement(tag);
    if (className !== void 0) {
      element.className = className;
    }
    return element;
  }
  function trailer(words) {
    const line = made("p", "trailer");
    line.textContent = words;
    return line;
  }
  function shown(value2) {
    const block = made("pre");
    block.textContent = typeof value2 === "string" ? value2 : JSON.stringify(value2, null, 2);
    return block;
  }

  // src/tabs.ts
  //! Which section is on screen.
  //!
  //! Hash routing, so a section is a link somebody can send and a refresh keeps
  //! you where you were. A panel nobody can link to is a panel people describe to
  //! each other in words.
  var tabs = () => all('[role="tab"]');
  function pane(tab) {
    const named = tab.getAttribute("aria-controls");
    if (named === null) {
      throw new Error(`the tab #${tab.id} controls nothing`);
    }
    return at(named);
  }
  function show(name) {
    const wanted2 = tabs().some((tab) => tab.id === "tab-" + name) ? name : "run";
    for (const tab of tabs()) {
      const chosen = tab.id === "tab-" + wanted2;
      tab.setAttribute("aria-selected", String(chosen));
      tab.tabIndex = chosen ? 0 : -1;
      pane(tab).hidden = !chosen;
    }
    if (window.location.hash !== "#" + wanted2) {
      window.location.hash = wanted2;
    }
  }
  function wire() {
    for (const tab of tabs()) {
      tab.addEventListener("click", () => show(tab.id.replace("tab-", "")));
      tab.addEventListener("keydown", (event) => {
        const step = event.key === "ArrowRight" ? 1 : event.key === "ArrowLeft" ? -1 : 0;
        if (step === 0) {
          return;
        }
        event.preventDefault();
        const here = tabs();
        const next = here[(here.indexOf(tab) + step + here.length) % here.length];
        if (next === void 0) {
          return;
        }
        next.focus();
        show(next.id.replace("tab-", ""));
      });
    }
    window.addEventListener(
      "hashchange",
      () => show(window.location.hash.replace("#", ""))
    );
    show(window.location.hash.replace("#", ""));
  }

  // src/log.ts
  //! What the panel did, on the operator's behalf.
  //!
  //! Every other screen composes TessariQL and sends it without showing it. That
  //! is only honest because it is still recoverable, and this is where it is
  //! recovered from: the statement as sent, the node's own words back, how long it
  //! took, and which screen issued it.
  //!
  //! Two things it is deliberately not. It is not the store's audit trail —
  //! `INFO FOR AUDIT` answers a different question, for a different reader, with a
  //! durability this makes no claim to. And it is not persisted: a record of who
  //! was administered and when, left behind on a shared operator's machine, is a
  //! disclosure nobody asked for. It lives as long as the tab does.
  var redacted = (statement) => statement.replace(/PASSWORD\s+'(?:[^'\\]|\\.)*'/gi, "PASSWORD '…'");
  var CAP = 200;
  var kept = [];
  function record(entry2) {
    kept.unshift({ ...entry2, what: redacted(entry2.what) });
    if (kept.length > CAP) {
      kept.length = CAP;
    }
    write("log-count", String(kept.length));
    if (!at("log-sheet").hidden) {
      draw();
    }
  }
  function reopen(what) {
    setValue("script", what);
    hide("log-sheet", true);
    show("run");
    at("script").focus();
  }
  function copy(what, where2, said3) {
    const clipboard = navigator.clipboard;
    if (clipboard === void 0) {
      const range = document.createRange();
      range.selectNodeContents(where2);
      const selection = window.getSelection();
      selection?.removeAllRanges();
      selection?.addRange(range);
      said3.textContent = "selected — ⌘C or Ctrl-C";
      return;
    }
    void clipboard.writeText(what).then(
      () => {
        said3.textContent = "copied";
      },
      () => {
        said3.textContent = "the browser would not copy it";
      }
    );
  }
  function entry(one2) {
    const row = made("div", "logged");
    const head = made("div", "logged-head");
    const screen = made("span", "faint");
    screen.textContent = one2.screen;
    const took = made("span", "faint");
    took.textContent = one2.ms + " ms";
    head.append(screen, took);
    const what = made("pre", "logged-what");
    what.textContent = one2.what;
    const said3 = made("p", one2.failed ? "note warn" : "note");
    said3.textContent = one2.said;
    const why = made("p", "note faint");
    why.textContent = one2.why === void 0 ? "" : "Why: " + one2.why;
    why.hidden = one2.why === void 0;
    const actions = made("div", "row tight");
    const told3 = made("span", "status");
    const copied = made("button", "quiet");
    copied.type = "button";
    copied.textContent = "Copy";
    copied.addEventListener("click", () => copy(one2.what, what, told3));
    const opened = made("button", "quiet");
    opened.type = "button";
    opened.textContent = "Open in Run";
    opened.addEventListener("click", () => reopen(one2.what));
    actions.append(copied, opened, told3);
    row.append(head, what, said3, why, actions);
    return row;
  }
  function draw() {
    clear("log-list");
    if (kept.length === 0) {
      const empty = made("p", "note");
      empty.textContent = "Nothing yet. Everything this panel sends on your behalf lands here.";
      at("log-list").appendChild(empty);
      return;
    }
    const list = made("div");
    for (const one2 of kept) {
      list.appendChild(entry(one2));
    }
    at("log-list").appendChild(list);
  }
  function closeIt() {
    hide("log-sheet", true);
    at("log-open").setAttribute("aria-expanded", "false");
  }
  function wire2() {
    write("log-count", "0");
    at("log-open").addEventListener("click", () => {
      const opening = at("log-sheet").hidden;
      hide("log-sheet", !opening);
      at("log-open").setAttribute("aria-expanded", String(opening));
      if (opening) {
        draw();
      }
    });
    at("log-close").addEventListener("click", closeIt);
    document.addEventListener("keydown", (pressed) => {
      if (pressed.key === "Escape" && !at("log-sheet").hidden) {
        closeIt();
      }
    });
  }

  // src/session.ts
  //! Who this page is, and the token it is holding.
  //!
  //! The token lives in this module's scope. That is the whole reason the panel
  //! is ONE bundle: two bundles would each inline a copy of this module, so there
  //! would be two tokens, and signing in on one section would silently not sign
  //! in the other.
  var held = null;
  var token = () => held;
  function typed() {
    const user = value("user");
    const password = value("password");
    if (user === "" && password === "") {
      return null;
    }
    return "Basic " + basic(user, password);
  }
  function basic(user, password) {
    const bytes = new TextEncoder().encode(user + ":" + password);
    return btoa(String.fromCharCode(...bytes));
  }
  function credential() {
    if (held !== null) {
      return "Bearer " + held;
    }
    return typed();
  }
  function signedIn() {
    const user = value("user");
    if (held !== null && user !== "") {
      write("signed-in", user);
      return;
    }
    write("signed-in", user === "" ? "not signed in" : user + " — not yet");
  }
  function ended() {
    held = null;
    signedIn();
    say("identity-status", "this session ended — sign in again", true);
  }
  function reason(text) {
    try {
      const body = JSON.parse(text);
      if (typeof body === "object" && body !== null && "error" in body) {
        const held3 = body.error;
        if (typeof held3 === "string") {
          return held3;
        }
      }
      return text;
    } catch {
      return text;
    }
  }
  function sheet() {
    const found = document.querySelector("details.identity");
    if (!(found instanceof HTMLDetailsElement)) {
      throw new Error("the page has no identity disclosure");
    }
    return found;
  }
  function wire3() {
    const identity = sheet();
    at("user").addEventListener("input", signedIn);
    at("sign-in").addEventListener("click", async () => {
      const offered = typed();
      if (offered === null) {
        say("identity-status", "a name and a password, or nothing at all", true);
        return;
      }
      say("identity-status", "signing in…");
      try {
        const reply = await fetch("/session", {
          method: "POST",
          headers: { Authorization: offered },
          // The same reason `ask` omits them: left to itself the browser answers
          // the node's `401` challenge with its own credential dialog, which this
          // console did not ask for and cannot clear.
          credentials: "omit"
        });
        const text = await reply.text();
        if (reply.status >= 400) {
          say("identity-status", reason(text), true);
          return;
        }
        held = JSON.parse(text).token;
        setValue("password", "");
        say("identity-status", "");
        signedIn();
        identity.open = false;
      } catch (failure) {
        say("identity-status", "the node did not answer: " + told(failure), true);
      }
    });
    at("sign-out").addEventListener("click", async () => {
      if (held !== null) {
        try {
          await fetch("/session", {
            method: "DELETE",
            headers: { Authorization: "Bearer " + held },
            credentials: "omit"
          });
        } catch {
        }
      }
      held = null;
      setValue("user", "");
      setValue("password", "");
      say("identity-status", "");
      signedIn();
      identity.open = false;
    });
    identity.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        identity.open = false;
      }
    });
    document.addEventListener("click", (event) => {
      const target = event.target;
      if (identity.open && target instanceof globalThis.Node && !identity.contains(target)) {
        identity.open = false;
      }
    });
    signedIn();
  }
  function told(failure) {
    return failure instanceof Error ? failure.message : String(failure);
  }

  // src/api.ts
  //! Talking to the node.
  //!
  //! The console is a client of the public API and has no private path to it.
  //! Every request below is one a `curl` could make against the same node, which
  //! is the constraint that keeps the API the product surface rather than
  //! something this page sits on top of.
  var SCRIPT_ROUTE = "/script";
  var WATCH_ROUTE = "/watch";
  var outcome = (result) => {
    if (Array.isArray(result.records)) {
      return result.records.length === 1 ? "1 record" : `${result.records.length} records`;
    }
    return result.kind ?? "answered";
  };
  function said(text, status) {
    let body;
    try {
      body = JSON.parse(text);
    } catch {
      const words = text.trim();
      return { said: words === "" ? `${status}` : words, failed: status >= 400 };
    }
    if (typeof body.error === "string") {
      return { said: body.error, failed: true };
    }
    if (!Array.isArray(body.results)) {
      return { said: text, failed: status >= 400 };
    }
    return { said: body.results.map(outcome).join(", "), failed: status >= 400 };
  }
  async function ask(source, screen, why) {
    const started = performance.now();
    const headers = {};
    const offered = credential();
    if (offered !== null) {
      headers["Authorization"] = offered;
    }
    const reply = await fetch(SCRIPT_ROUTE, {
      method: "POST",
      headers,
      body: source,
      // Without this the browser handles the node's `401` challenge itself and
      // opens its own credential dialog on top of the page — a second sign-in
      // this console did not ask for, cannot read and cannot clear, and which
      // leaves the page's own request hanging behind it. The credential is in the
      // header above; nothing here wants the browser to manage one.
      credentials: "omit"
    });
    if (reply.status === 401 && token() !== null) {
      ended();
    }
    const text = await reply.text();
    record({
      what: source,
      ...said(text, reply.status),
      ms: Math.round(performance.now() - started),
      screen,
      why
    });
    return { reply, text };
  }
  async function valueOf(source, screen, why) {
    const { reply, text } = await ask(source, screen, why);
    let body;
    try {
      body = JSON.parse(text);
    } catch {
      throw new Error(text.trim() === "" ? reply.status + " " + reply.statusText : text);
    }
    if (!Array.isArray(body.results)) {
      throw new Error(typeof body.error === "string" ? body.error : text);
    }
    const answered2 = body.results[body.results.length - 1];
    return answered2 === void 0 ? null : answered2;
  }
  function held2(answered2) {
    if (answered2 === null || answered2.kind !== "value" || typeof answered2.value !== "object" || answered2.value === null) {
      return null;
    }
    return answered2.value;
  }
  async function scrape(route) {
    const reply = await fetch(route, { credentials: "omit" });
    const text = await reply.text();
    try {
      return { status: reply.status, body: JSON.parse(text) };
    } catch {
      return { status: reply.status, body: text };
    }
  }

  // src/drawer.ts
  //! One node, over the map.
  //!
  //! A drawer rather than a screen, because the map is the context the decision is
  //! being made in: an operator looking at a node during a failover is looking at
  //! it *relative to the others*, and a navigation that replaces the view takes
  //! away the reason they opened it.
  //!
  //! # It carries one action, and says why it is one
  //!
  //! The band asks for three — a role change, a drain, and a hand-over. Only the
  //! first has a statement behind it, and that was searched rather than assumed:
  //!
  //! - **drain** — `Roles::NONE` is a real state and the engine's source calls it
  //!   the operator's own drain, but `ROLES NONE` and `ROLES ;` are parse errors,
  //!   there is no `DRAIN`, and omitting `ROLES` means *leave them alone*.
  //! - **hand-over** — no `HANDOVER`, `STEP DOWN` or `YIELD` in the grammar.
  //!
  //! So the drawer names them and offers no control for them. A button that
  //! composes no statement is a button that lies, and on this screen it would lie
  //! about the one thing an operator opens the screen to do.
  //!
  //! # A peer cannot be changed from here at all, and that was measured
  //!
  //! The drawer first offered the role change on any node. A running node refused
  //! it, which is the right way to learn it:
  //!
  //! - `DEFINE REPLICA warsaw …` again → *"the name rp:warsaw is already in use"*
  //! - `ALTER REPLICA warsaw SET ROLES …` → `ALTER` takes only `NAMESPACE`,
  //!   `USER` or `TABLE`
  //! - `DROP REPLICA warsaw` → *"this node is in a cluster and holds no
  //!   leadership: it does not accept writes until a majority grants it one"*
  //!
  //! So a clustered node can declare its membership once and then cannot amend it
  //! from the query surface. **This node's own roles are the exception**, because
  //! `DEFINE NODE` writes to the local `META` keyspace rather than through the
  //! log, so the fence does not apply — verified on the same clustered node.
  //!
  //! The drawer therefore offers the control on this node and, on a peer, says
  //! what it would take. Offering it everywhere would have composed a statement
  //! that always fails, which is a button that lies in the slower way: it looks
  //! right until the one moment somebody needs it.
  var open = null;
  var BITS = ["serving", "writable", "coordinating"];
  function ticked() {
    return BITS.filter((bit) => at(`drawer-${bit}`).checked);
  }
  function change(subject, roles) {
    if (subject === null || roles.length === 0) {
      return null;
    }
    return subject.self ? `DEFINE NODE ROLES ${roles.join(", ")};` : null;
  }
  function preview() {
    if (open === null) {
      return;
    }
    if (!open.self) {
      say(
        "drawer-says",
        `A peer's row cannot be amended from here: there is no ALTER REPLICA, a second DEFINE REPLICA is refused for the name already in use, and DROP REPLICA is refused while this node holds no leadership.`
      );
      return;
    }
    const roles = ticked();
    if (roles.length === 0) {
      say("drawer-says", "at least one role — there is no statement that clears them all", true);
      return;
    }
    say("drawer-says", `Sets this node's roles to ${roles.join(", ")}.`);
  }
  function show2(subject) {
    open = subject;
    at("drawer-title").textContent = subject.self ? "This node" : subject.name;
    for (const bit of BITS) {
      at(`drawer-${bit}`).checked = subject.roles.includes(bit);
    }
    clear("drawer-missing");
    for (const bit of BITS) {
      at(`drawer-${bit}`).disabled = !subject.self;
    }
    hide("drawer-apply", !subject.self);
    if (!subject.self) {
      const note = made("p", "faint");
      note.textContent = `${subject.name} answers on ${subject.endpoint ?? "an address this node did not record"}.`;
      at("drawer-missing").appendChild(note);
    }
    say("drawer-status", "");
    preview();
    hide("drawer", false);
    at("drawer-close").focus();
  }
  function closeIt2() {
    hide("drawer", true);
    open = null;
  }
  function wire4() {
    for (const bit of BITS) {
      at(`drawer-${bit}`).addEventListener("change", preview);
    }
    at("drawer-close").addEventListener("click", closeIt2);
    document.addEventListener("keydown", (pressed) => {
      if (pressed.key === "Escape" && !at("drawer").hidden) {
        closeIt2();
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
        const answered2 = await valueOf(statement, "Cluster · roles");
        say("drawer-status", answered2 !== null && answered2.kind === "done" ? "declared" : "");
      } catch (failure) {
        say("drawer-status", told(failure), true);
      }
    });
  }

  // src/roster.ts
  //! What the panel has been told about who exists.
  //!
  //! The blast radius a destructive form shows — *who loses what* — has to come
  //! from somewhere, and the only honest source is the listing the node already
  //! answered. This holds it, so that the forms can read it without importing the
  //! listing that draws it: `users.ts` writes here and `user-forms.ts` reads, which
  //! keeps the two modules pointing one way instead of at each other.
  //!
  //! It is deliberately not a cache. Nothing here is asked for on a miss, nothing
  //! expires, and a name this module has never heard of returns `null` so the form
  //! can say it does not know rather than invent a reach. A radius drawn from a
  //! guess is worse than no radius at all — it is the panel narrating an answer
  //! the node never gave.
  var known = /* @__PURE__ */ new Map();
  function forget() {
    known.clear();
  }
  function remember(name, one2) {
    known.set(name, one2);
  }
  function lookup(name) {
    return known.get(name) ?? null;
  }

  // src/user-says.ts
  //! What a button will do, said in the reader's language.
  //!
  //! Three sentences and nothing else. They take what they describe as ARGUMENTS
  //! rather than reading the form, which is what lets them be read — and one day
  //! tested — without a form existing at all. `user-forms.ts` reads the controls
  //! and calls these; the split is along that line and not an arbitrary one.
  //!
  //! The rule every sentence here obeys: **say what the statement changes and
  //! stop.** A consequence the engine has not been asked about is the panel
  //! inventing an answer, which is the same defect as drawing a metric it does not
  //! have. Where a sentence does claim a consequence — that a session ends — the
  //! claim is backed by a test in the suite and not by reasoning from the
  //! mechanism.
  function definitionSays(name, role2, space) {
    return "Creates " + name + " as " + role2 + (space === "" ? " of the whole node — an administrator" : " in " + space) + ", with the password typed above.";
  }
  function alterationSays(name, what, role2) {
    const today = lookup(name);
    const standing = today === null ? " This panel has not been told what " + name + " reaches — press List to find out." : " Today " + name + " is " + today.role + " in " + today.reach + ", and that is unchanged.";
    if (what === "password") {
      return "Sets a new password for " + name + ". The one they have stops working and any session they are holding ends, so they sign in again with the new one." + standing;
    }
    return "Makes " + name + " " + role2 + ". Any session they are holding ends, so they sign in again." + standing;
  }
  function removalSays(name) {
    const today = lookup(name);
    if (today === null) {
      return name + " loses access entirely, and their grants go with them. This panel has not been told what " + name + " reaches — press List above to find out before you do this.";
    }
    return name + " loses access entirely: " + today.role + " in " + today.reach + ", and the grants go with them. A new user of the same name inherits none of it.";
  }

  // src/user-forms.ts
  //! The three user forms: what they describe, and what they show while typing.
  //!
  //! Only the statements and the previews live here. The buttons that RUN them
  //! are in `users.ts`, beside the listing they have to redraw — which keeps the
  //! two modules pointing one way instead of at each other.
  //!
  //! What a pane shows before it runs is **what will happen**, in words, and not
  //! the statement that will do it. The statement is not hidden — it is in the
  //! log the moment it is sent, and on the one pane that cannot come back it sits
  //! behind a disclosure. The difference matters because reading TessariQL to find
  //! out what a button does makes the language the interface, and then everybody
  //! who cannot read it is guessing.
  function quoted(text) {
    let out = "'";
    for (const character of text) {
      if (character === "'") {
        out += "\\'";
      } else if (character === "\\") {
        out += "\\\\";
      } else if (character === "\n") {
        out += "\\n";
      } else if (character === "\r") {
        out += "\\r";
      } else if (character === "	") {
        out += "\\t";
      } else {
        out += character;
      }
    }
    return out + "'";
  }
  var MEANS = {
    viewer: "reads what the space holds, and nothing else.",
    editor: "reads and writes records, and declares structure.",
    owner: "everything in the space, users included.",
    other: "a role this build may not know. It will be sent as typed, and the node's refusal is what you will see if it does not exist."
  };
  function reach() {
    if (value("new-reach") === "node") {
      return "";
    }
    const space = trimmed("new-scope");
    return space === "" ? null : space;
  }
  function role() {
    const chosen = value("new-role");
    return chosen === "other" ? trimmed("new-role-other") : chosen;
  }
  function definition() {
    const name = trimmed("new-name");
    const named = role();
    const space = reach();
    if (name === "" || named === "" || space === null) {
      return null;
    }
    return "DEFINE USER " + name + (space === "" ? "" : " ON " + space) + " ROLE " + named + " PASSWORD " + quoted(value("new-password")) + ";";
  }
  function missing() {
    if (trimmed("new-name") === "") {
      return "a name is needed";
    }
    if (role() === "") {
      return "a role is needed";
    }
    return "a space is needed — or choose the whole node, which is not the same thing";
  }
  function preview2() {
    write("role-says", MEANS[value("new-role")] ?? "");
    const space = reach();
    const statement = definition();
    write(
      "define-preview",
      statement === null || space === null ? missing() : definitionSays(trimmed("new-name"), role(), space)
    );
  }
  function shapeTheForm() {
    hide("scope-field", value("new-reach") === "node");
    hide("role-other-field", value("new-role") !== "other");
    preview2();
  }
  function changedRole() {
    const chosen = value("change-role");
    return chosen === "other" ? trimmed("change-role-other") : chosen;
  }
  function alteration() {
    const name = trimmed("change-name");
    if (name === "") {
      return null;
    }
    if (value("change-what") === "password") {
      return "ALTER USER " + name + " SET PASSWORD " + quoted(value("change-password")) + ";";
    }
    const named = changedRole();
    return named === "" ? null : "ALTER USER " + name + " SET ROLE " + named + ";";
  }
  function changeMissing() {
    if (trimmed("change-name") === "") {
      return "a name is needed";
    }
    return "a role is needed";
  }
  function changeWhy() {
    return trimmed("change-why");
  }
  function shapeTheChange() {
    const changing = value("change-what");
    hide("change-password-field", changing !== "password");
    hide("change-role-field", changing !== "role");
    hide("change-role-other-field", changing !== "role" || value("change-role") !== "other");
    write(
      "change-preview",
      alteration() === null ? changeMissing() : alterationSays(
        trimmed("change-name"),
        value("change-what") === "password" ? "password" : "role",
        changedRole()
      )
    );
  }
  function removal() {
    const name = trimmed("remove-name");
    const again = trimmed("remove-confirm");
    return name !== "" && name === again ? "DROP USER " + name + ";" : null;
  }
  function removeWhy() {
    return trimmed("remove-why");
  }
  function shapeTheRemoval() {
    const statement = removal();
    disable("remove", statement === null);
    const name = trimmed("remove-name");
    write(
      "remove-radius",
      statement !== null ? removalSays(name) : name === "" ? "a name is needed" : "type the same name again to confirm"
    );
    write("remove-preview", statement ?? "");
  }
  function wire5() {
    for (const field of [
      "new-name",
      "new-scope",
      "new-role",
      "new-role-other",
      "new-password",
      "new-reach"
    ]) {
      at(field).addEventListener("input", shapeTheForm);
      at(field).addEventListener("change", shapeTheForm);
    }
    for (const field of [
      "change-name",
      "change-what",
      "change-password",
      "change-role",
      "change-role-other",
      "change-why"
    ]) {
      at(field).addEventListener("input", shapeTheChange);
      at(field).addEventListener("change", shapeTheChange);
    }
    for (const field of ["remove-name", "remove-confirm", "remove-why"]) {
      at(field).addEventListener("input", shapeTheRemoval);
    }
    shapeTheForm();
    shapeTheChange();
    shapeTheRemoval();
  }

  // src/formation.ts
  //! Declaring the cluster's membership, in one transaction.
  //!
  //! # The cliff this form exists to not build
  //!
  //! Declaring peers one statement at a time STRANDS the operator, and it was
  //! reproduced twice against a running node:
  //!
  //! ```text
  //! DEFINE REPLICA warsaw …;  → ok
  //! DEFINE REPLICA lisbon …;  → error: this node is in a cluster and holds no
  //!                             leadership: it does not accept writes until a
  //!                             majority grants it one
  //! ```
  //!
  //! The first declaration makes the node clustered, which costs it `writable`,
  //! which is what the second declaration needs. Wrapping both in
  //! `BEGIN; … COMMIT;` succeeds.
  //!
  //! So a per-row Add button would build that cliff into the interface, and the
  //! operator would meet it halfway through a membership with no way forward and
  //! no way back. The whole intended membership is one form and one transaction.
  //!
  //! # Not a wizard either
  //!
  //! There are no ordered stages here — a membership is a set, declared at once.
  //! A wizard would impose an order the domain does not have and would make the
  //! last step the one that fails.
  var ROWS = 5;
  var rowFields = (at_) => [
    `peer-${at_}-name`,
    `peer-${at_}-endpoint`,
    `peer-${at_}-node`
  ];
  function intended() {
    const found = [];
    for (let index = 0; index < ROWS; index += 1) {
      const name = trimmed(`peer-${index}-name`);
      const endpoint = trimmed(`peer-${index}-endpoint`);
      const node = trimmed(`peer-${index}-node`);
      if (name === "" && endpoint === "" && node === "") {
        continue;
      }
      const roles = [];
      for (const bit of ["serving", "writable", "coordinating"]) {
        if (at(`peer-${index}-${bit}`).checked) {
          roles.push(bit);
        }
      }
      found.push({ name, endpoint, node, roles });
    }
    return found;
  }
  function incomplete(rows) {
    for (const [index, row] of rows.entries()) {
      const missing2 = row.name === "" ? "a name" : row.endpoint === "" ? "an address" : row.node === "" ? "a node id" : row.roles.length === 0 ? "at least one role" : null;
      if (missing2 !== null) {
        return `row ${index + 1} needs ${missing2}`;
      }
    }
    return null;
  }
  function formation(rows) {
    const declarations = rows.map(
      (row) => `DEFINE REPLICA ${row.name} AT ${quoted(row.endpoint)} NODE ${quoted(row.node)} ROLES ${row.roles.join(", ")};`
    );
    return ["BEGIN;", ...declarations, "COMMIT;"].join("\n");
  }
  function preview3() {
    const rows = intended();
    const missing2 = incomplete(rows);
    if (rows.length === 0) {
      say("form-says", "Nothing declared yet.");
      return;
    }
    if (missing2 !== null) {
      say("form-says", missing2, true);
      return;
    }
    const named = rows.map((row) => row.name).join(", ");
    say(
      "form-says",
      `Declares ${rows.length === 1 ? "one peer" : `${rows.length} peers`} — ${named} — in a single transaction. All of them or none.`
    );
  }
  function showStatement() {
    const rows = intended();
    clear("form-statement");
    const block = made("pre");
    block.textContent = rows.length === 0 || incomplete(rows) !== null ? "" : formation(rows);
    at("form-statement").appendChild(block);
  }
  function wire6() {
    for (let index = 0; index < ROWS; index += 1) {
      for (const field of [
        ...rowFields(index),
        `peer-${index}-serving`,
        `peer-${index}-writable`,
        `peer-${index}-coordinating`
      ]) {
        at(field).addEventListener("input", () => {
          preview3();
          showStatement();
        });
        at(field).addEventListener("change", () => {
          preview3();
          showStatement();
        });
      }
    }
    at("form-cluster").addEventListener("click", async () => {
      const rows = intended();
      if (rows.length === 0) {
        say("form-status", "nothing to declare", true);
        return;
      }
      const missing2 = incomplete(rows);
      if (missing2 !== null) {
        say("form-status", missing2, true);
        return;
      }
      say("form-status", "running…");
      try {
        const answered2 = await valueOf(formation(rows), "Cluster · form");
        say("form-status", answered2 !== null && answered2.kind === "done" ? "declared" : "");
      } catch (failure) {
        say("form-status", told(failure), true);
      }
    });
    preview3();
    showStatement();
  }

  // src/draw.ts
  //! Turning what the node said into something on screen.
  //!
  //! Every value drawn here arrives off the wire or out of the store, so every
  //! one of them is written with `textContent`. A record that happens to hold a
  //! `<script>` tag is data, not markup.
  var flat = (value2) => value2 === null || typeof value2 !== "object";
  function shape(records) {
    let agreed = null;
    for (const record2 of records) {
      const inside = record2.value;
      if (inside === null || typeof inside !== "object" || Array.isArray(inside)) {
        return null;
      }
      const fields = inside;
      const here = Object.keys(fields).sort();
      if (!here.every((field) => flat(fields[field]))) {
        return null;
      }
      if (agreed === null) {
        agreed = here;
      } else if (agreed.length !== here.length || !agreed.every((f, i) => f === here[i])) {
        return null;
      }
    }
    return agreed !== null && agreed.length > 0 ? agreed : null;
  }
  var cell = (value2) => typeof value2 === "string" ? value2 : JSON.stringify(value2);
  function drawn(records) {
    const fields = shape(records);
    if (fields === null) {
      return null;
    }
    const values = records.map((record2) => record2.value);
    const numeric = fields.map(
      (field) => values.every((value2) => typeof value2[field] === "number")
    );
    const table = made("table");
    const head = table.createTHead().insertRow();
    for (const name of ["id", ...fields]) {
      const column = made("th");
      column.textContent = name;
      head.appendChild(column);
    }
    const body = table.createTBody();
    records.forEach((record2, position) => {
      const row = body.insertRow();
      row.insertCell().textContent = record2.id;
      fields.forEach((field, index) => {
        const box = row.insertCell();
        box.textContent = cell(values[position]?.[field]);
        if (numeric[index] === true) {
          box.classList.add("number");
        }
      });
    });
    return table;
  }
  function put(where2, value2) {
    clear(where2);
    at(where2).appendChild(shown(value2));
  }
  function facts(where2, held3) {
    clear(where2);
    const table = made("table");
    const body = table.createTBody();
    for (const [name, value2] of Object.entries(held3)) {
      const row = body.insertRow();
      const label = made("th");
      label.textContent = name;
      row.appendChild(label);
      const box = row.insertCell();
      box.textContent = typeof value2 === "string" ? value2 : JSON.stringify(value2);
      if (typeof value2 === "number") {
        box.classList.add("number");
      }
    }
    at(where2).appendChild(table);
  }

  // src/map.ts
  //! The cluster, drawn.
  //!
  //! The one signature element of this console, and the thing the owner asked for
  //! by name. Everything here comes from `INFO FOR NODE` and nothing is inferred:
  //! the fields, and the reasons each may or may not be drawn, are settled in the
  //! data contract at `reports/2026-09-15-190000-g026-cluster-map-data-contract.md`,
  //! which was itself derived by running a node rather than by reading anything.
  //!
  //! # Three lamps, not one badge
  //!
  //! A role is three INDEPENDENT bits — serving, writable, coordinating — so there
  //! are eight combinations and no taxonomy of *leader / follower / standby* to
  //! draw. Three mutually exclusive badges would be inventing one, and would have
  //! no way at all to show the state the engine calls out as the operator's own
  //! drain mechanism: no roles at all, which is a node still holding its data and
  //! answering nothing.
  //!
  //! Each lamp carries its LETTER. Meaning never rests on colour alone — not for a
  //! reader who cannot separate two of them, and not on a projector at the back of
  //! an incident room.
  //!
  //! # Leadership is a lease, not a role
  //!
  //! `coordinating` means the node may STAND FOR leadership. Whether it holds it
  //! is `cluster.lease`, and the lease has an expiry. A leadership marker without
  //! a clock implies a permanence the engine never promised — and the engine's own
  //! source carries a note about an earlier version that read the role where it
  //! should have read the lease.
  //!
  //! # A peer's lamps are DECLARED and say so
  //!
  //! This node knows what it declared about a peer. It has not asked the peer what
  //! it reports, and there is no field that would answer. So a peer's lamps are
  //! drawn as declarations and labelled as declarations; drawing them filled, like
  //! this node's reported ones, would be the map claiming an observation nobody
  //! made.
  var BITS2 = [
    { name: "serving", letter: "S", means: "answers client requests" },
    { name: "writable", letter: "W", means: "accepts writes rather than forwarding them" },
    { name: "coordinating", letter: "C", means: "takes part in deciding, not only in storing" }
  ];
  function lamp(letter, state2, title) {
    const one2 = made("span", "lamp " + state2);
    one2.textContent = letter;
    one2.title = title;
    one2.setAttribute(
      "aria-label",
      `${title} — ${state2 === "held" ? "held" : state2 === "wanted" ? "declared, not yet held" : "not held"}`
    );
    return one2;
  }
  function lamps(has, wanted2) {
    const row = made("div", "lamps");
    for (const bit of BITS2) {
      const held3 = has.includes(bit.name);
      const asked2 = wanted2 !== null && wanted2.includes(bit.name);
      row.appendChild(lamp(bit.letter, held3 ? "held" : asked2 ? "wanted" : "off", bit.means));
    }
    return row;
  }
  function fact(label, said3) {
    if (said3 === null) {
      return null;
    }
    const line = made("div", "fact");
    const name = made("span", "faint");
    name.textContent = label;
    const value2 = made("span", "fact-value");
    value2.textContent = said3;
    line.append(name, value2);
    return line;
  }
  var told2 = (value2) => value2 === void 0 || value2 === null ? null : String(value2);
  function lease(held3) {
    if (held3 === void 0 || held3 === null) {
      return null;
    }
    const badge = made("div", "lease");
    const held_ = held3;
    const until = told2(held_.until ?? held_.expires ?? held3);
    badge.textContent = until === null ? "holds the lease" : `holds the lease until ${until}`;
    return badge;
  }
  var drained = (has) => has.length === 0;
  function figure(title, has, wanted2, facts2, kind, subject) {
    const box = made("article", "node " + kind);
    box.tabIndex = 0;
    box.setAttribute("role", "button");
    box.setAttribute("aria-label", `${title} — open its drawer`);
    box.addEventListener("click", () => show2(subject));
    box.addEventListener("keydown", (pressed) => {
      if (pressed.key === "Enter" || pressed.key === " ") {
        pressed.preventDefault();
        show2(subject);
      }
    });
    const head = made("div", "node-head");
    const name = made("h3");
    name.textContent = title;
    head.append(name, lamps(has, wanted2));
    box.appendChild(head);
    if (drained(has)) {
      const note = made("p", "note warn");
      note.textContent = kind === "self" ? "Drained — it holds its data and answers nothing." : "Declared with no roles — drained.";
      box.appendChild(note);
    }
    if (kind === "peer") {
      const note = made("p", "faint");
      note.textContent = "lamps as declared here; this node has not asked it";
      box.appendChild(note);
    }
    for (const one2 of facts2) {
      if (one2 !== null) {
        box.appendChild(one2);
      }
    }
    return box;
  }
  function draw2(into, seen) {
    const cluster = seen.cluster ?? {};
    const mine = seen.roles ?? [];
    const wanted2 = cluster.desired ?? null;
    const self = figure(
      "This node",
      mine,
      wanted2,
      [
        lease(cluster.lease),
        fact("id", told2(seen.id)),
        fact("answers on", (seen.endpoints ?? []).join(", ") || null),
        fact("epoch", told2(cluster.epoch)),
        fact("campaigns", told2(cluster.campaigns)),
        fact("collecting from here", String((cluster.followers ?? []).length)),
        wanted2 === null ? null : fact("declared for it", wanted2.join(", "))
      ],
      "self",
      { name: "This node", self: true, endpoint: null, node: null, roles: mine }
    );
    into.appendChild(self);
    for (const peer of cluster.peers ?? []) {
      into.appendChild(
        figure(
          peer.name ?? "(unnamed peer)",
          peer.roles ?? [],
          null,
          [
            fact("answers on", told2(peer.endpoint)),
            fact("id", told2(peer.node)),
            fact("replicates", told2(peer.replicates))
          ],
          "peer",
          {
            name: peer.name ?? "",
            self: false,
            endpoint: peer.endpoint ?? null,
            node: peer.node ?? null,
            roles: peer.roles ?? []
          }
        )
      );
    }
  }

  // src/node.ts
  //! This machine, and what the cluster tab can say about it today.
  //!
  //! The operational routes here are the ones any monitor already scrapes, so
  //! nothing on this tab is a capability only the console has.
  var HEALTH_ROUTE = "/health";
  var READY_ROUTE = "/ready";
  var METRICS_ROUTE = "/metrics";
  function readings(text) {
    const out = {};
    for (const line of text.split("\n")) {
      if (line.startsWith("#") || line.trim() === "") {
        continue;
      }
      const cut = line.lastIndexOf(" ");
      if (cut > 0) {
        out[line.slice(0, cut)] = Number(line.slice(cut + 1));
      }
    }
    return out;
  }
  function answer(scraped) {
    const body = scraped.body;
    const words = typeof body === "object" && body !== null ? Object.entries(body).map(([name, value2]) => name + " " + String(value2)).join(", ") : String(body).trim();
    return scraped.status + " " + words;
  }
  async function readNode() {
    say("node-status", "asking…");
    try {
      const answered2 = held2(await valueOf("INFO FOR NODE;", "Node"));
      const all2 = answered2 ?? {};
      const { cluster, ...mine } = all2;
      facts("node-facts", mine);
      const peers = typeof cluster === "object" && cluster !== null ? cluster.peers : void 0;
      clear("cluster-map");
      draw2(at("cluster-map"), all2);
      facts("cluster-facts", {
        roles: all2["roles"],
        peers: peers ?? [],
        endpoints: all2["endpoints"],
        id: all2["id"]
      });
      say("cluster-status", "");
    } catch (failure) {
      clear("node-facts");
      say("node-status", told(failure), true);
      say("cluster-status", told(failure), true);
      return;
    }
    const [health, ready, metrics] = await Promise.all([
      scrape(HEALTH_ROUTE),
      scrape(READY_ROUTE),
      scrape(METRICS_ROUTE)
    ]);
    facts("node-health", { health: answer(health), ready: answer(ready) });
    facts(
      "node-metrics",
      typeof metrics.body === "string" ? readings(metrics.body) : metrics.body
    );
    say("node-status", "");
  }
  function wire7() {
    at("node-refresh").addEventListener("click", readNode);
    let read = false;
    for (const tab of ["tab-this-node", "tab-cluster"]) {
      at(tab).addEventListener("click", () => {
        if (!read) {
          read = true;
          void readNode();
        }
      });
    }
  }

  // src/password.ts
  //! Changing your own password.
  function shapeMine() {
    const current = value("mine-current");
    const fresh = value("mine-new");
    const again = value("mine-again");
    disable("mine", current === "" || fresh === "" || fresh !== again);
    const differ = fresh !== "" && again !== "" && fresh !== again;
    say("mine-status", differ ? "the two new ones differ" : "", differ);
  }
  function wire8() {
    for (const field of ["mine-current", "mine-new", "mine-again"]) {
      at(field).addEventListener("input", shapeMine);
    }
    at("mine").addEventListener("click", async () => {
      const name = trimmed("user");
      if (name === "") {
        say("mine-status", "sign in first — this changes your own password", true);
        return;
      }
      say("mine-status", "changing…");
      const started = performance.now();
      try {
        const reply = await fetch("/password", {
          method: "POST",
          headers: { Authorization: "Basic " + basic(name, value("mine-current")) },
          body: value("mine-new"),
          credentials: "omit"
        });
        const text = await reply.text();
        record({
          what: "Change your own password — POST /password as " + name,
          said: reply.status >= 400 ? reason(text) : "changed; every session ended",
          failed: reply.status >= 400,
          ms: Math.round(performance.now() - started),
          screen: "Users · your own password"
        });
        if (reply.status >= 400) {
          say("mine-status", reason(text), true);
          return;
        }
        for (const field of ["mine-current", "mine-new", "mine-again"]) {
          setValue(field, "");
        }
        shapeMine();
        at("sign-out").click();
        say("identity-status", "password changed — sign in with the new one", false);
      } catch (failure) {
        say("mine-status", "the node did not answer: " + told(failure), true);
      }
    });
    shapeMine();
  }

  // src/query.ts
  //! Running a script, and showing what came back.
  var drawing = "auto";
  var answered = null;
  function one(pane2, result) {
    if (result.kind === "records" && Array.isArray(result.records)) {
      const records = result.records;
      const table = records.length === 0 ? null : drawn(records);
      if (table === null) {
        pane2.appendChild(shown(records.length === 0 ? "(no records)" : records));
      } else {
        pane2.appendChild(table);
      }
      pane2.appendChild(
        trailer("(" + records.length + " record(s), via " + String(result.path) + ")")
      );
      return;
    }
    if (result.kind === "done") {
      pane2.appendChild(trailer("ok"));
      return;
    }
    pane2.appendChild(shown(result));
  }
  function paint() {
    const pane2 = at("answer");
    pane2.textContent = "";
    if (answered === null) {
      return;
    }
    if (drawing === "json" || !Array.isArray(answered.results)) {
      pane2.appendChild(shown(answered));
      return;
    }
    for (const result of answered.results) {
      one(pane2, result);
    }
  }
  async function run() {
    say("script-status", "running…");
    answered = null;
    clear("answer");
    try {
      const { reply, text } = await ask(value("script"), "Query");
      try {
        answered = JSON.parse(text);
      } catch {
        answered = null;
        const block = made("pre");
        block.textContent = text;
        at("answer").appendChild(block);
      }
      paint();
      say("script-status", reply.status + " " + reply.statusText, reply.status >= 400);
    } catch (failure) {
      say("script-status", "the node did not answer: " + told(failure), true);
    }
  }
  function wire9() {
    for (const button of all("[data-shape]")) {
      button.addEventListener("click", () => {
        drawing = button.dataset["shape"] ?? "auto";
        for (const other of all("[data-shape]")) {
          other.classList.toggle("chosen", other === button);
        }
        paint();
      });
    }
    at("run").addEventListener("click", run);
    at("script").addEventListener("keydown", (event) => {
      if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
        event.preventDefault();
        void run();
      }
    });
  }

  // src/search.ts
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
  function inRun(statement) {
    setValue("script", statement);
    show("run");
    at("run").click();
  }
  function candidates(text) {
    const record2 = text.includes(":");
    const qualified = text.includes(".") && !record2;
    const [namespace, database] = qualified ? text.split(".", 2) : ["", ""];
    const out = [];
    if (record2) {
      const statement = "SELECT * FROM " + text + ";";
      out.push({ kind: "record", statement, land: () => inRun(statement) });
    }
    if (qualified) {
      const statement = "USE NAMESPACE " + namespace + "; USE DATABASE " + database + "; INFO FOR DATABASE;";
      out.push({ kind: "database", statement, land: () => inRun(statement) });
    }
    if (!record2 && !qualified) {
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
        }
      });
      const namespaceStatement = "USE NAMESPACE " + text + "; INFO FOR NAMESPACE;";
      out.push({
        kind: "namespace",
        statement: namespaceStatement,
        land: () => inRun(namespaceStatement)
      });
      const tableStatement = "INFO FOR TABLE " + text + ";";
      out.push({ kind: "table", statement: tableStatement, land: () => inRun(tableStatement) });
    }
    return out;
  }
  async function look() {
    const text = trimmed("search");
    if (text === "") {
      return;
    }
    write("search-says", "looking…");
    for (const candidate of candidates(text)) {
      try {
        await valueOf(candidate.statement, "Search · " + candidate.kind);
      } catch {
        continue;
      }
      write("search-says", "");
      candidate.land();
      return;
    }
    write("search-says", "nothing here answers to that name");
  }
  function wire10() {
    at("search").addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        void look();
      }
    });
    document.addEventListener("keydown", (event) => {
      const focused = document.activeElement;
      const typing = focused instanceof HTMLInputElement || focused instanceof HTMLTextAreaElement || focused instanceof HTMLSelectElement;
      const shortcut = event.key === "k" && (event.metaKey || event.ctrlKey);
      if (shortcut || event.key === "/" && !typing) {
        event.preventDefault();
        at("search").focus();
        at("search").select();
      }
    });
  }

  // src/states.ts
  //! The four things a screen can be, said as four things.
  //!
  //! `say(id, words, failed?)` carries one string and one boolean, so the five
  //! situations an operator actually meets render as two. The two that collapse
  //! are the expensive ones: **nothing here** and **some of it** look identical,
  //! and an operator who reads a bounded page as the whole list concludes an
  //! account does not exist when it is simply not on screen.
  //!
  //! # Each state names a NEXT ACTION, and that is the half worth guarding
  //!
  //! "No users" is a state. "No users — this store is open to anybody, and the
  //! first `DEFINE USER` closes it" is a state that tells the reader what to do
  //! about it. A screen full of correct nouns and no verbs is a screen that makes
  //! the operator go and ask somebody.
  //!
  //! # Why not simply widen `say`
  //!
  //! Because a wider `say` would let a screen render a partial state by passing
  //! the wrong argument, and nothing would say so. Four named calls make the wrong
  //! one a thing you have to type on purpose. `say` keeps its two-state job for
  //! the many places that genuinely have two — this is not a rewrite of all
  //! sixty-six of its call sites, and it must not become one.
  function state(id, kind, words) {
    const line = at(id);
    line.textContent = words;
    for (const other of ["waiting", "empty", "partial", "wrong"]) {
      line.classList.toggle(`is-${other}`, other === kind);
    }
    line.classList.toggle("failed", kind === "wrong");
    line.setAttribute("aria-live", kind === "wrong" ? "assertive" : "polite");
  }
  function settled(id) {
    const line = at(id);
    line.textContent = "";
    for (const other of ["waiting", "empty", "partial", "wrong"]) {
      line.classList.remove(`is-${other}`);
    }
    line.classList.remove("failed");
  }

  // src/users.ts
  //! Who exists, and the buttons that change that.
  //!
  //! Everything here goes through a statement over `POST /script`. There is no
  //! request on this page that a `curl` could not make, which is what keeps a
  //! console feature from becoming a capability only the console has.
  //!
  //! # The node answers in full and this file renders a page
  //!
  //! `INFO FOR USERS` refuses rather than filters: it is answered only to a caller
  //! who administers the tenancy, and then in full for that tenancy. So every row
  //! in the answer is a row the reader may see, and holding them all here crosses
  //! no boundary — which is what makes rendering a page, rather than paging the
  //! statement, an honest arrangement rather than a shortcut.
  //!
  //! It is an arrangement with a MEASUREMENT behind it and a trigger for undoing
  //! it. Measured on a skewed 5 000-account store: the listing costs about 4.9 µs
  //! and 74 bytes per account, beside a fixed ~30 ms of password verification that
  //! a signed-in panel pays once rather than per request. The node is not what
  //! costs; five thousand table rows in a browser are. Past **10 000 accounts in
  //! one tenancy** the paging belongs in the statement instead — that is engine
  //! work, and it is recorded as Q-678 rather than left to be noticed.
  var SHOWN = 200;
  var everybody = [];
  function pick(name) {
    setValue("lookup-name", name);
    setValue("change-name", name);
    setValue("remove-name", name);
    shapeTheChange();
    shapeTheRemoval();
    at("lookup").click();
  }
  var wanted = () => trimmed("user-filter").toLowerCase();
  function matching() {
    const needle = wanted();
    if (needle === "") {
      return everybody;
    }
    return everybody.filter((one2) => (one2.user ?? "").toLowerCase().includes(needle));
  }
  function tally(matched) {
    const held3 = everybody.length;
    const filtered = wanted() !== "";
    if (matched.length <= SHOWN) {
      return filtered ? `${matched.length} of ${held3}` : `${held3}`;
    }
    return `showing ${SHOWN} of ${matched.length}${filtered ? "" : ` — type a name to narrow`}`;
  }
  function listing(rows) {
    const table = made("table");
    const head = table.createTHead().insertRow();
    for (const column of ["user", "role", "reach"]) {
      const cell2 = made("th");
      cell2.textContent = column;
      head.appendChild(cell2);
    }
    const body = table.createTBody();
    for (const one2 of rows) {
      const row = body.insertRow();
      const name = one2.user ?? "";
      row.insertCell().textContent = name;
      row.insertCell().textContent = said2(one2);
      row.insertCell().textContent = reach2(one2);
      row.addEventListener("click", () => pick(name));
    }
    return table;
  }
  var said2 = (one2) => one2.role === "owner" && one2.namespace === void 0 ? "owner · admin" : one2.role ?? "";
  var reach2 = (one2) => one2.namespace === void 0 ? "the whole node" : one2.namespace + (one2.database === void 0 ? "" : "." + one2.database);
  async function listUsers() {
    state("user-status", "waiting", "asking the node who it knows about…");
    try {
      const answer2 = held2(await valueOf("INFO FOR USERS;", "Users · list"));
      const listed = answer2 === null ? [] : answer2["users"];
      everybody = Array.isArray(listed) ? listed : [];
      forget();
      for (const one2 of everybody) {
        remember(one2.user ?? "", { role: said2(one2), reach: reach2(one2) });
      }
      redraw();
    } catch (failure) {
      clear("user-list");
      state(
        "user-status",
        "wrong",
        `${told(failure)} — a listing is answered to whoever administers the tenancy.`
      );
    }
  }
  function redraw() {
    const matched = matching();
    clear("user-list");
    if (matched.length === 0) {
      state(
        "user-status",
        "empty",
        everybody.length === 0 ? "No accounts — this store is open to anybody, and the first DEFINE USER closes it." : `No account matches that — clear the filter to see all ${everybody.length} of them.`
      );
      at("user-list").appendChild(trailer("(nothing to show)"));
    } else if (matched.length > SHOWN) {
      state(
        "user-status",
        "partial",
        `Showing ${SHOWN} of ${matched.length} — type part of a name to narrow it.`
      );
      at("user-list").appendChild(listing(matched.slice(0, SHOWN)));
    } else {
      settled("user-status");
      at("user-list").appendChild(listing(matched));
    }
    write("user-count", tally(matched));
  }
  function wire11() {
    at("list").addEventListener("click", listUsers);
    at("user-filter").addEventListener("input", redraw);
    at("tab-access").addEventListener("click", () => {
      if (at("user-list").textContent === "") {
        void listUsers();
      }
    });
    at("lookup").addEventListener("click", async () => {
      const name = trimmed("lookup-name");
      if (name === "") {
        say("user-status", "a name is needed — there is no listing to pick from", true);
        return;
      }
      say("user-status", "asking…");
      try {
        const answered2 = await valueOf("INFO FOR USER " + name + ";", "Users · detail");
        const one2 = held2(answered2);
        if (one2 !== null) {
          facts("user-answer", one2);
        } else {
          put("user-answer", answered2);
        }
        say("user-status", "");
      } catch (failure) {
        clear("user-answer");
        say("user-status", told(failure), true);
      }
    });
    at("define").addEventListener("click", async () => {
      const statement = definition();
      if (statement === null) {
        say("define-status", missing(), true);
        return;
      }
      say("define-status", "running…");
      try {
        const answered2 = await valueOf(statement, "Users · define");
        const finished = answered2 !== null && answered2.kind === "done";
        say("define-status", finished ? "ok" : "");
        if (!finished) {
          put("user-answer", answered2);
        }
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
      const why = changeWhy();
      if (why === "") {
        say("change-status", "say why — this ends their session and they sign in again", true);
        return;
      }
      say("change-status", "running…");
      try {
        const answered2 = await valueOf(statement, "Users · change", why);
        say("change-status", answered2 !== null && answered2.kind === "done" ? "ok" : "");
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
        const answered2 = await valueOf(statement, "Users · remove", why);
        say("remove-status", answered2 !== null && answered2.kind === "done" ? "removed" : "");
        setValue("remove-name", "");
        setValue("remove-confirm", "");
        setValue("remove-why", "");
        shapeTheRemoval();
        await listUsers();
      } catch (failure) {
        say("remove-status", told(failure), true);
      }
    });
  }

  // src/watch.ts
  //! Following a table as it changes.
  var following = null;
  function stop(words) {
    if (following !== null) {
      following.close();
      following = null;
    }
    disable("follow", false);
    disable("stop", true);
    if (words !== void 0) {
      say("watch-status", words);
    }
  }
  function change2(what) {
    const line = made("li");
    const became = typeof what.became === "string" ? what.became : "";
    line.classList.add(became === "removed" ? "removed" : "written");
    line.textContent = "#" + String(what.sequence) + "  " + String(what.table) + ":" + String(what.id) + "  " + became + (what.value === void 0 ? "" : "  " + JSON.stringify(what.value));
    const list = at("changes");
    list.insertBefore(line, list.firstChild);
  }
  function where() {
    const address = new URL(WATCH_ROUTE, window.location.href);
    address.protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
    return address;
  }
  function asked() {
    const wanted2 = {
      namespace: value("namespace"),
      database: value("database"),
      from: Number(value("from"))
    };
    const table = value("table");
    if (table !== "") {
      wanted2.table = table;
    }
    const carried = token();
    if (carried !== null) {
      wanted2.token = carried;
      return wanted2;
    }
    const user = value("user");
    const password = value("password");
    if (user !== "" || password !== "") {
      wanted2.user = user;
      wanted2.password = password;
    }
    return wanted2;
  }
  function wire12() {
    at("follow").addEventListener("click", () => {
      stop();
      clear("changes");
      const socket = new WebSocket(where());
      following = socket;
      disable("follow", true);
      disable("stop", false);
      say("watch-status", "connecting…");
      socket.addEventListener("open", () => {
        socket.send(JSON.stringify(asked()));
        say("watch-status", "following");
      });
      socket.addEventListener("message", (event) => {
        let what;
        try {
          what = JSON.parse(String(event.data));
        } catch {
          say("watch-status", "the node sent something this page cannot read", true);
          return;
        }
        if (typeof what.refused === "string") {
          say("watch-status", what.refused, true);
          return;
        }
        if (typeof what.error === "string") {
          say("watch-status", what.error, true);
          return;
        }
        change2(what);
      });
      socket.addEventListener("close", (event) => {
        stop(event.code === 1001 ? "the node is stopping" : "stopped");
      });
      socket.addEventListener("error", () => {
        say("watch-status", "the socket failed", true);
      });
    });
    at("stop").addEventListener("click", () => stop("stopped"));
  }

  // src/console.ts
  //! The console's entry point: everything the page does, started in one place.
  //!
  //! Nothing here is fetched from anywhere else: no framework, no CDN, no web
  //! font. The page is meant to work on a machine with no route out at all, and a
  //! single remote reference would quietly take that away.
  //!
  //! This is ONE bundle on purpose. `sections.js` used to be a second file that
  //! read the first one's top-level names out of the global scope, and that
  //! coupling is what turned a single `SyntaxError` in one file into a dead page
  //! — every section of it, for nineteen days. Two bundles would be no better:
  //! each would inline its own copy of `session.ts`, so there would be two tokens
  //! and signing in on one section would silently not sign in the other.
  //!
  //! The start-up order is written out below rather than left to emerge from the
  //! import graph. In a bundle, a module's top-level statements run when the
  //! graph first reaches it, which makes the order of everything on this page an
  //! accident of who imports whom. One list is cheaper to read and cannot drift.
  write("where", "served by " + window.location.host);
  wire2();
  wire();
  wire3();
  wire9();
  wire10();
  wire6();
  wire4();
  wire12();
  wire5();
  wire11();
  wire7();
  wire8();
})();
