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
  function write(id, words4) {
    at(id).textContent = words4;
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
  function say(id, words4, failed2) {
    const line = at(id);
    line.textContent = words4;
    line.classList.toggle("failed", failed2 === true);
    line.setAttribute("aria-live", failed2 === true ? "assertive" : "polite");
  }
  function made(tag, className) {
    const element = document.createElement(tag);
    if (className !== void 0) {
      element.className = className;
    }
    return element;
  }
  function trailer(words4) {
    const line = made("p", "trailer");
    line.textContent = words4;
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
  function here() {
    const chosen2 = tabs().find((tab) => tab.getAttribute("aria-selected") === "true");
    return chosen2 === void 0 ? "" : chosen2.id.replace("tab-", "");
  }
  var arrivals = [];
  function onArrival(names, todo) {
    arrivals.push({ names, todo });
    if (names.includes(here())) {
      todo();
    }
  }
  function hereAgain() {
    for (const arrival of arrivals) {
      if (arrival.names.includes(here())) {
        arrival.todo();
      }
    }
  }
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
      const chosen2 = tab.id === "tab-" + wanted2;
      tab.setAttribute("aria-selected", String(chosen2));
      tab.tabIndex = chosen2 ? 0 : -1;
      pane(tab).hidden = !chosen2;
    }
    if (window.location.hash !== "#" + wanted2) {
      window.location.hash = wanted2;
    }
    for (const arrival of arrivals) {
      if (arrival.names.includes(wanted2)) {
        arrival.todo();
      }
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
        const here2 = tabs();
        const next = here2[(here2.indexOf(tab) + step + here2.length) % here2.length];
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
  var redacted = (statement2) => statement2.replace(/PASSWORD\s+'(?:[^'\\]|\\.)*'/gi, "PASSWORD '…'");
  var CAP = 200;
  var kept = [];
  var answeredOnce = (entry2) => !entry2.failed && /CREATE\s+JOIN\s+TOKEN/i.test(entry2.what) ? "a join token, shown once where it was asked for" : entry2.said;
  function record(entry2) {
    kept.unshift({ ...entry2, what: redacted(entry2.what), said: answeredOnce(entry2) });
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
  function copy(what, where3, said3) {
    const clipboard = navigator.clipboard;
    if (clipboard === void 0) {
      const range = document.createRange();
      range.selectNodeContents(where3);
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
    const copied2 = made("button", "quiet");
    copied2.type = "button";
    copied2.textContent = "Copy";
    copied2.addEventListener("click", () => copy(one2.what, what, told3));
    const opened = made("button", "quiet");
    opened.type = "button";
    opened.textContent = "Open in Run";
    opened.addEventListener("click", () => reopen(one2.what));
    actions.append(copied2, opened, told3);
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
    hereAgain();
  }
  function reason(text) {
    try {
      const body = JSON.parse(text);
      if (typeof body === "object" && body !== null && "error" in body) {
        const held5 = body.error;
        if (typeof held5 === "string") {
          return held5;
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
  function fromThisMachine() {
    const host = location.hostname;
    return host === "localhost" || host === "[::1]" || host === "::1" || host.startsWith("127.");
  }
  function transport() {
    const secured = location.protocol === "https:";
    const local = !secured && fromThisMachine();
    write(
      "transport-says",
      secured ? "This page and everything it sends travel over TLS to this node." : local ? "This page arrived without TLS from this machine itself, so the password does not cross a network." : "This page arrived without TLS, so the password travels as typed. Give the node --tls-cert and --tls-key, or keep it on a network you protect."
    );
    at("transport-says").className = secured || local ? "note" : "note warn";
    at("clear-banner").hidden = secured || local;
  }
  function wire3() {
    const identity = sheet();
    transport();
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
        hereAgain();
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
      hereAgain();
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
  var Unreachable = class extends Error {
  };
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
      const words4 = text.trim();
      return { said: words4 === "" ? `${status}` : words4, failed: status >= 400 };
    }
    if (typeof body.error === "string") {
      return { said: body.error, failed: true };
    }
    if (!Array.isArray(body.results)) {
      return { said: text, failed: status >= 400 };
    }
    return { said: body.results.map(outcome).join(", "), failed: status >= 400 };
  }
  async function ask(source, screen, why, parameters) {
    const started = performance.now();
    const headers = {};
    let body = source;
    if (parameters !== void 0) {
      headers["Content-Type"] = "application/json";
      body = JSON.stringify({ script: source, parameters });
    }
    const offered = credential();
    if (offered !== null) {
      headers["Authorization"] = offered;
    }
    let reply;
    try {
      reply = await fetch(SCRIPT_ROUTE, {
        method: "POST",
        headers,
        body,
        // Without this the browser handles the node's `401` challenge itself and
        // opens its own credential dialog on top of the page — a second sign-in
        // this console did not ask for, cannot read and cannot clear, and which
        // leaves the page's own request hanging behind it. The credential is in
        // the header above; nothing here wants the browser to manage one.
        credentials: "omit"
      });
    } catch {
      record({
        what: source,
        said: "the node did not answer",
        failed: true,
        ms: Math.round(performance.now() - started),
        screen,
        why
      });
      throw new Unreachable("it may be stopped, or unreachable from this browser");
    }
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
  async function valueOf(source, screen, why, parameters) {
    const { reply, text } = await ask(source, screen, why, parameters);
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
  async function route(method, path, screen, body) {
    const started = performance.now();
    const headers = {};
    const offered = credential();
    if (offered !== null) {
      headers["Authorization"] = offered;
    }
    let reply;
    try {
      reply = await fetch(path, { method, headers, credentials: "omit", ...body === void 0 ? {} : { body } });
    } catch {
      record({
        what: `${method} ${path}`,
        said: "the node did not answer",
        failed: true,
        ms: Math.round(performance.now() - started),
        screen
      });
      throw new Unreachable("it may be stopped, or unreachable from this browser");
    }
    if (reply.status === 401 && token() !== null) {
      ended();
    }
    const text = await reply.text();
    record({
      what: `${method} ${path}`,
      ...said(text, reply.status),
      ms: Math.round(performance.now() - started),
      screen
    });
    try {
      return { status: reply.status, body: JSON.parse(text) };
    } catch {
      return { status: reply.status, body: text };
    }
  }
  async function scrape(route2) {
    const reply = await fetch(route2, { credentials: "omit" });
    const text = await reply.text();
    try {
      return { status: reply.status, body: JSON.parse(text) };
    } catch {
      return { status: reply.status, body: text };
    }
  }

  // src/watch.ts
  //! Following a table as it changes.
  var following = null;
  var toldWhy = false;
  var isFollowing = () => following !== null;
  function stop(words4) {
    if (following !== null) {
      following.close();
      following = null;
    }
    disable("follow", false);
    disable("stop", true);
    if (words4 !== void 0) {
      say("watch-status", words4);
    }
  }
  function change(what) {
    const line = made("li");
    const became = typeof what.became === "string" ? what.became : "";
    line.classList.add(became === "removed" ? "removed" : "written");
    line.textContent = "#" + String(what.sequence) + "  " + String(what.table) + ":" + String(what.id) + "  " + became + (what.value === void 0 ? "" : "  " + JSON.stringify(what.value)) + (what.cursor === void 0 ? "" : "  cursor " + what.cursor);
    if (typeof what.cursor === "string") {
      at("cursor").value = what.cursor;
    }
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
    const cursor = value("cursor");
    if (cursor !== "") {
      wanted2.cursor = cursor;
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
  function wire4() {
    at("follow").addEventListener("click", () => {
      stop();
      clear("changes");
      const socket = new WebSocket(where());
      following = socket;
      toldWhy = false;
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
          toldWhy = true;
          return;
        }
        if (typeof what.error === "string") {
          say("watch-status", what.error, true);
          toldWhy = true;
          return;
        }
        change(what);
      });
      socket.addEventListener("close", (event) => {
        stop(toldWhy ? void 0 : event.code === 1001 ? "the node is stopping" : "stopped");
      });
      socket.addEventListener("error", () => {
        say("watch-status", "the socket failed", true);
      });
    });
    at("stop").addEventListener("click", () => stop("stopped"));
  }

  // src/context.ts
  //! What you had typed, still there after the reload.
  //!
  //! An operator mid-incident loses a session to a refresh, a crashed tab, a
  //! laptop lid. What they lose with it is a script they had built up, a name they
  //! were looking at, a form half filled in — every one of which they will now
  //! reconstruct from memory, in the worst conditions for reconstructing anything.
  //!
  //! # Not a password, ever
  //!
  //! Nothing whose control is `type="password"` is written here, and that is
  //! enforced by reading the control rather than by keeping the list correct: a
  //! field added to the list later cannot become a stored credential by somebody
  //! forgetting which kind it was. The console's whole posture is that it holds a
  //! token in memory and nothing on disk, and a remembered password would quietly
  //! be the exception to that on a machine two people share.
  //!
  //! # `sessionStorage`, not `localStorage`
  //!
  //! Per tab, gone when the tab closes, never shared with another tab. A reload is
  //! the interruption this exists for; a colleague opening the console tomorrow on
  //! the same machine is not, and `localStorage` would hand them the namespace, the
  //! account name and the statement somebody was working on last night.
  //!
  //! # What does NOT survive, said plainly
  //!
  //! A running follow does not resume. The token that authorised it lives in
  //! memory and the reload took it, so re-establishing the socket would need a
  //! sign-in the operator has not given yet. The fields come back and the screen
  //! says the follow stopped — which is the honest half of the claim, where
  //! silently not resuming would leave somebody watching a feed that is not there.
  var KEY = "tessaridb.console.context";
  var FOLLOWING = "#following";
  var REMEMBERED = [
    "script",
    "lookup-name",
    "user-filter",
    "new-name",
    "new-scope",
    "change-name",
    "remove-name",
    "remove-why",
    "change-why",
    "namespace",
    "database",
    "table",
    "from",
    "search"
  ];
  function mayKeep(id) {
    const control2 = at(id);
    return !(control2 instanceof HTMLInputElement && control2.type === "password");
  }
  function held3() {
    try {
      const found = window.sessionStorage.getItem(KEY);
      return found === null ? {} : JSON.parse(found);
    } catch {
      return {};
    }
  }
  function keep() {
    const kept2 = {};
    for (const id of REMEMBERED) {
      if (!mayKeep(id)) {
        continue;
      }
      const value2 = at(id).value;
      if (value2 !== "") {
        kept2[id] = value2;
      }
    }
    if (isFollowing()) {
      kept2[FOLLOWING] = "yes";
    }
    try {
      window.sessionStorage.setItem(KEY, JSON.stringify(kept2));
    } catch {
    }
  }
  function restore() {
    const kept2 = held3();
    for (const id of REMEMBERED) {
      const value2 = kept2[id];
      if (value2 === void 0 || !mayKeep(id)) {
        continue;
      }
      at(id).value = value2;
    }
    if (kept2[FOLLOWING] === "yes") {
      at("watch-status").textContent = "the follow stopped at the reload — sign in and press Follow to resume it";
    }
  }
  function wire5() {
    restore();
    for (const id of REMEMBERED) {
      at(id).addEventListener("input", keep);
    }
    window.addEventListener("beforeunload", keep);
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
      const here2 = Object.keys(fields).sort();
      if (!here2.every((field) => flat(fields[field]))) {
        return null;
      }
      if (agreed === null) {
        agreed = here2;
      } else if (agreed.length !== here2.length || !agreed.every((f, i) => f === here2[i])) {
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
  function put(where3, value2) {
    clear(where3);
    at(where3).appendChild(shown(value2));
  }
  function facts(where3, held5) {
    clear(where3);
    const table = made("table");
    const body = table.createTBody();
    for (const [name, value2] of Object.entries(held5)) {
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
    at(where3).appendChild(table);
  }

  // src/detail.ts
  //! One thing, looked at.
  //!
  //! A sheet over whatever you were doing, not a destination. The cap is five
  //! destinations and a detail view is not a place you go — it is a thing you
  //! open, look at, and close, and it has to leave you where you were.
  //!
  //! It draws whatever the node answered, as facts, under a heading that names
  //! what kind of thing it is. The same sheet serves a namespace, a database, a
  //! table and a record, because the difference between them is the question that
  //! was asked and not the shape of the answer.
  //!
  //! # A record has a timeline; the others still do not
  //!
  //! T5.2 asked for a timeline on each of these, and W318 drew none because the
  //! measurement said there was no event source. That measurement was of three
  //! READ surfaces — `INFO FOR VERSIONS` (a conflict report: one version after
  //! three writes), `INFO FOR NAMESPACE` (a list of databases) and `INFO FOR
  //! AUDIT` (the vault's own trail) — and their silence was read as the store
  //! recording nothing. It records everything: every commit is a log record
  //! carrying what it changed (Q-739). What was missing was a way to ask, and
  //! `INFO FOR HISTORY OF` is now it.
  //!
  //! So a RECORD is drawn with its history. A namespace, a database and a table
  //! are not, and that is not an omission left for later: a catalog row is
  //! deliberately filtered out of the log projection, so there is genuinely
  //! nothing to draw for them, and the sheet says which case it is in rather than
  //! showing an empty list that reads like a quiet record.
  //!
  //! # Three states, three renderings
  //!
  //! A timeline nobody could fetch, a timeline that is empty, and a timeline cut
  //! short at the log's walk budget are three different facts. Rendering any two
  //! of them the same way is the failure this console has refused by name three
  //! times: an absence that looks like a measurement.
  function timeline(kind2, history) {
    clear("detail-history");
    const line = made("p", "note");
    if (kind2 !== "record") {
      line.textContent = "Only a record has a history: a catalog row is kept out of the log’s change projection, so there is nothing recorded to draw for this.";
      at("detail-history").appendChild(line);
      return;
    }
    const answer2 = history?.value;
    const events = Array.isArray(answer2?.events) ? answer2.events : null;
    if (events === null) {
      line.textContent = "The history could not be read — the node refused it, or this build does not answer INFO FOR HISTORY. That is not the same as nothing having happened.";
      at("detail-history").appendChild(line);
      return;
    }
    if (events.length === 0) {
      line.textContent = "Nothing is recorded against this record in the log read.";
      at("detail-history").appendChild(line);
      return;
    }
    const list = made("ol", "timeline");
    for (const event of events) {
      const entry2 = made("li");
      const what = event.change === "removed" ? "removed" : "written";
      const when = typeof event.at === "string" ? event.at : "?";
      entry2.textContent = `${what} at ${when}`;
      list.appendChild(entry2);
    }
    at("detail-history").appendChild(list);
    if (answer2?.complete === false) {
      const cut = made("p", "note");
      cut.textContent = "Older entries may exist: the read stopped at its record budget before reaching the start of the log.";
      at("detail-history").appendChild(cut);
    }
  }
  function show2(kind2, name, answered2, history = null) {
    at("detail-kind").textContent = kind2;
    at("detail-name").textContent = name;
    clear("detail-facts");
    if (Array.isArray(answered2?.records)) {
      for (const row of answered2.records) {
        const id = made("p", "faint");
        id.textContent = row.id;
        at("detail-facts").appendChild(id);
        if (typeof row.value === "object" && row.value !== null) {
          const into = made("div");
          into.id = `detail-row-${row.id.replace(/[^a-zA-Z0-9]/g, "-")}`;
          at("detail-facts").appendChild(into);
          facts(into.id, row.value);
        }
      }
      if (answered2.records.length === 0) {
        const nothing = made("p", "note");
        nothing.textContent = "The node answered, and there is no such record.";
        at("detail-facts").appendChild(nothing);
      }
    } else if (typeof answered2?.value === "object" && answered2.value !== null) {
      facts("detail-facts", answered2.value);
    } else {
      const nothing = made("p", "note");
      nothing.textContent = "The node answered, and the answer carries no fields.";
      at("detail-facts").appendChild(nothing);
    }
    timeline(kind2, history);
    at("detail-sheet").hidden = false;
    at("detail-close").focus();
  }
  function closeIt2() {
    at("detail-sheet").hidden = true;
  }
  var hide2 = closeIt2;
  function wire6() {
    at("detail-close").addEventListener("click", closeIt2);
    document.addEventListener("keydown", (pressed) => {
      if (pressed.key === "Escape" && !at("detail-sheet").hidden) {
        closeIt2();
      }
    });
  }

  // src/topic-names.ts
  //! What the Topics screen may write into a statement.
  //!
  //! A topic, a group, a namespace and a database are grammar: the node cannot take
  //! them as parameters, so this screen writes them into the text. That is safe
  //! only behind a check narrower than the node's own lexer, and these are the same
  //! two patterns every client applies (consumer contract 1.0, section 3). A name
  //! that fails is refused here, before anything is sent, rather than quoted.
  //!
  //! Numbers and durations are grammar too — `AFTER 40`, `ACK DEADLINE 30s` — and
  //! are checked for the same reason.
  var NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;
  var GROUP = /^[A-Za-z0-9_.:-]{1,128}$/;
  var DURATION = /^[0-9]{1,9}(ms|s|m|h|d|w)$/;
  var WHOLE = /^[0-9]{1,15}$/;
  var aName = (text) => NAME.test(text) ? text : null;
  var aGroup = (text) => GROUP.test(text) ? text : null;
  var aDuration = (text) => DURATION.test(text) ? text : null;
  function aWhole(text) {
    return WHOLE.test(text) ? Number(text) : null;
  }
  var tenancy = (namespace, database) => `USE NAMESPACE ${namespace}; USE DATABASE ${database}; `;

  // src/drawer.ts
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
  var open = null;
  var BITS = ["serving", "writable", "coordinating"];
  function ticked() {
    return BITS.filter((bit) => at(`drawer-${bit}`).checked);
  }
  function change2(subject, roles) {
    if (subject === null) {
      return null;
    }
    if (!subject.self) {
      return aName(subject.name) === null || roles.length === 0 ? null : `ALTER REPLICA ${subject.name} ROLES ${roles.join(", ")};`;
    }
    return roles.length === 0 ? `DEFINE NODE ROLES NONE;` : `DEFINE NODE ROLES ${roles.join(", ")};`;
  }
  function preview() {
    if (open === null) {
      return;
    }
    if (!open.self) {
      const roles2 = ticked();
      say(
        "drawer-says",
        roles2.length === 0 ? `A peer keeps at least one role; to take it out of the cluster, remove it below.` : `Sets ${open.name}'s declared roles to ${roles2.join(", ")}, leaving its node, subscription and certificate as they are. Taken by the node that leads the store.`
      );
      return;
    }
    const roles = ticked();
    if (roles.length === 0) {
      const kept2 = `Drains this node: it keeps its data, its identity and its place in the membership, and stops answering clients and accepting writes until a role is declared here again.`;
      const declared = open.declared;
      say(
        "drawer-says",
        declared === null ? kept2 : `${kept2} The membership declares ${declared.join(", ")} for this node, so this is a local override the next open discards — to drain it for good, write the membership row instead.`
      );
      return;
    }
    say("drawer-says", `Sets this node's roles to ${roles.join(", ")}.`);
  }
  function show3(subject) {
    open = subject;
    at("drawer-title").textContent = subject.self ? "This node" : subject.name;
    for (const bit of BITS) {
      at(`drawer-${bit}`).checked = subject.roles.includes(bit);
    }
    clear("drawer-missing");
    hide("drawer-remove-part", subject.self);
    setValue("drawer-remove-confirm", "");
    shapeRemove();
    if (!subject.self) {
      const note2 = made("p", "faint");
      note2.textContent = `${subject.name} answers on ${subject.endpoint ?? "an address this node did not record"}.`;
      at("drawer-missing").appendChild(note2);
    }
    say("drawer-status", "");
    preview();
    hide("drawer", false);
    at("drawer-close").focus();
  }
  function removal() {
    if (open === null || open.self || aName(open.name) === null) {
      return null;
    }
    return trimmed("drawer-remove-confirm") === open.name ? `DROP REPLICA ${open.name};` : null;
  }
  function shapeRemove() {
    disable("drawer-remove", removal() === null);
    say(
      "drawer-remove-says",
      open === null || open.self ? "" : `Removes ${open.name} from the membership. Its node is never admitted again — a machine coming back is wiped and joins under a new identity. Type ${open.name} to confirm.`
    );
  }
  function closeIt3() {
    hide("drawer", true);
    open = null;
  }
  function wire7() {
    for (const bit of BITS) {
      at(`drawer-${bit}`).addEventListener("change", preview);
    }
    at("drawer-close").addEventListener("click", closeIt3);
    at("drawer-remove-confirm").addEventListener("input", shapeRemove);
    at("drawer-remove").addEventListener("click", async () => {
      const statement2 = removal();
      if (statement2 === null) {
        return;
      }
      say("drawer-remove-status", "running…");
      try {
        await valueOf(statement2, "Cluster · remove", trimmed("drawer-remove-why") || void 0);
        say("drawer-remove-status", "removed");
        closeIt3();
        hereAgain();
      } catch (failure) {
        say("drawer-remove-status", told(failure), true);
      }
    });
    document.addEventListener("keydown", (pressed) => {
      if (pressed.key === "Escape" && !at("drawer").hidden) {
        closeIt3();
      }
    });
    at("drawer-apply").addEventListener("click", async () => {
      const statement2 = change2(open, ticked());
      if (statement2 === null) {
        say("drawer-status", "there is nothing this drawer can send for that", true);
        return;
      }
      say("drawer-status", "running…");
      try {
        const answered2 = await valueOf(statement2, "Cluster · roles");
        const done = answered2 !== null && answered2.kind === "done";
        say("drawer-status", done ? "declared" : "");
        if (done) {
          hereAgain();
        }
      } catch (failure) {
        say("drawer-status", told(failure), true);
      }
    });
  }

  // src/trust.ts
  //! Trust on the cluster tab: what this node presents, what the cluster refuses,
  //! who may join, and the failover periods (ADR-0108 D6, D9; DEFINE FAILOVER).
  //!
  //! Everything drawn comes from the `INFO FOR NODE` answer the map was drawn
  //! from, so the panes and the map can never describe two different moments.
  //!
  //! # The irreversible one asks for the value again
  //!
  //! A revocation reaches every node and cannot be taken back, so the button stays
  //! dead until the first eight digits are typed a second time, and the sentence
  //! above it says when the fingerprint is this node's own — revoking that cuts
  //! this node off from its own cluster, which is a thing to be told before, not
  //! after.
  var SCREEN = "Cluster · trust";
  var FINGERPRINT = /^[0-9a-f]{64}$/;
  var DAY_MS = 864e5;
  var PERIODS = ["awareness", "collection", "round", "campaign", "lease"];
  var mine = [];
  var rows = [];
  function aFingerprint(text) {
    const plain = text.replace(/:/g, "").toLowerCase();
    return FINGERPRINT.test(plain) ? plain : null;
  }
  function daysLeft(expires) {
    const at_ = typeof expires === "string" ? Date.parse(expires) : Number.NaN;
    return Number.isNaN(at_) ? null : Math.floor((at_ - Date.now()) / DAY_MS);
  }
  function listed(where3, values, empty) {
    clear(where3);
    if (values.length === 0) {
      const none = made("p", "faint");
      none.textContent = empty;
      at(where3).appendChild(none);
      return;
    }
    const list = made("ul");
    for (const one2 of values) {
      const item = made("li");
      const code = made("code");
      code.textContent = one2;
      item.appendChild(code);
      list.appendChild(item);
    }
    at(where3).appendChild(list);
  }
  function drawMine() {
    clear("trust-mine");
    if (mine.length === 0) {
      const none = made("p", "faint");
      none.textContent = "This node serves its clients in the clear: --tls-cert and --tls-key would encrypt them, and --require-client-tls refuses to start without them.";
      at("trust-mine").appendChild(none);
      return;
    }
    for (const shown3 of mine) {
      const line = made("p");
      const left = daysLeft(shown3.expires);
      const when = left === null ? "its expiry could not be read" : left < 0 ? `EXPIRED ${-left} day(s) ago — every handshake refuses it` : `expires in ${left} day(s) (${told(shown3.expires)})`;
      line.textContent = `${shown3.surface ?? "?"} — ${when}: `;
      const code = made("code");
      code.textContent = shown3.fingerprint ?? "";
      line.appendChild(code);
      if (left !== null && left < 14) {
        line.className = "note warn";
      }
      at("trust-mine").appendChild(line);
    }
  }
  function drawJoin() {
    const select = at("join-row");
    clear("join-row");
    const open2 = rows.filter((row) => (row.node ?? null) === null && typeof row.name === "string");
    if (open2.length === 0) {
      hide("join-token", true);
    }
    for (const row of open2) {
      const option = made("option");
      option.value = row.name ?? "";
      const until = row.join_expires_ms;
      option.textContent = (row.name ?? "") + (typeof until === "number" ? ` (a token waits until ${new Date(until).toLocaleTimeString()})` : "");
      select.appendChild(option);
    }
    shapeJoin();
  }
  function drawFailover(held5) {
    write(
      "failover-held",
      held5 === null || held5 === void 0 ? "Nobody has set a policy: every node runs the built-in periods shown as placeholders." : "Set: " + PERIODS.map((clause) => `${clause.toUpperCase()} ${told(held5[clause])}`).join(", ") + (held5["balance_leaderships"] === true ? ", BALANCE LEADERSHIPS" : "") + ` (epoch ${told(held5["epoch"])}, version ${told(held5["version"])}).`
    );
    at("failover-balance").checked = held5?.["balance_leaderships"] === true;
    shapeFailover();
  }
  function draw2(seen) {
    mine = seen.certificates ?? [];
    rows = seen.cluster?.peers ?? [];
    drawMine();
    listed("trust-revoked", seen.cluster?.revoked ?? [], "No certificate is refused.");
    listed("trust-removed", seen.cluster?.tombstoned ?? [], "No node has been removed.");
    drawJoin();
    drawFailover(seen.cluster?.failover);
    shapeRevoke();
    shapeFailover();
  }
  function revocation() {
    const fingerprint = aFingerprint(trimmed("revoke-fingerprint"));
    if (fingerprint === null) {
      return { missing: "a fingerprint: 64 hexadecimal digits, with or without colons" };
    }
    if (trimmed("revoke-confirm").toLowerCase() !== fingerprint.slice(0, 8)) {
      return { missing: `type ${fingerprint.slice(0, 8)} again to confirm` };
    }
    const own2 = mine.find((shown3) => shown3.fingerprint === fingerprint);
    const pinned = rows.find((row) => row.fingerprint === fingerprint);
    return {
      statement: `REVOKE CERTIFICATE '${fingerprint}';`,
      says: (own2 !== void 0 ? `This is the certificate THIS node presents on its ${own2.surface ?? ""} surface — every peer will refuse this node until it is given a new one. ` : pinned !== void 0 ? `This is the certificate pinned to ${pinned.name ?? "a row"}. ` : "") + "Every node refuses it, in both directions, for good."
    };
  }
  function shapeRevoke() {
    const composed = revocation();
    disable("revoke-apply", !("statement" in composed));
    say("revoke-says", "statement" in composed ? composed.says : composed.missing);
  }
  function shapeJoin() {
    const name = aName(trimmed("join-row"));
    disable("join-apply", name === null);
    say(
      "join-says",
      name === null ? "No row waits for a node: declare one without a node id or a fingerprint first." : `Issues a token that binds ${name} to the first node offering it, for ${trimmed("join-life")}.`
    );
  }
  function policy() {
    const said3 = [];
    for (const clause of PERIODS) {
      const period = aDuration(trimmed(`failover-${clause}`));
      if (period === null) {
        return { missing: `${clause.toUpperCase()} needs a duration such as 200ms or 1s — every clause is required` };
      }
      said3.push(`${clause.toUpperCase()} ${period}`);
    }
    if (at("failover-balance").checked) {
      said3.push("BALANCE LEADERSHIPS");
    }
    return { statement: `DEFINE FAILOVER ${said3.join(" ")};` };
  }
  function shapeFailover() {
    const composed = policy();
    disable("failover-apply", !("statement" in composed));
    say(
      "failover-says",
      "statement" in composed ? `Replaces the policy on every node: ${composed.statement} The node refuses a set whose periods do not hold together, and says which.` : composed.missing
    );
  }
  async function send(statement2, status, why) {
    say(status, "sending…");
    try {
      const answered2 = await valueOf(statement2, SCREEN, why);
      say(status, "done");
      return answered2 !== null;
    } catch (failure) {
      say(status, told(failure), true);
      return false;
    }
  }
  function wire8() {
    for (const id of ["revoke-fingerprint", "revoke-confirm"]) {
      at(id).addEventListener("input", shapeRevoke);
    }
    for (const id of ["join-row", "join-life"]) {
      at(id).addEventListener("change", shapeJoin);
    }
    for (const clause of PERIODS) {
      at(`failover-${clause}`).addEventListener("input", shapeFailover);
    }
    at("failover-balance").addEventListener("change", shapeFailover);
    at("revoke-apply").addEventListener("click", async () => {
      const composed = revocation();
      if (!("statement" in composed)) {
        return;
      }
      if (await send(composed.statement, "revoke-status", trimmed("revoke-why") || void 0)) {
        setValue("revoke-confirm", "");
        setValue("revoke-fingerprint", "");
        hereAgain();
      }
    });
    at("join-apply").addEventListener("click", async () => {
      const name = aName(trimmed("join-row"));
      const life = aDuration(trimmed("join-life"));
      if (name === null || life === null) {
        return;
      }
      say("join-status", "sending…");
      hide("join-token", true);
      try {
        const answered2 = await valueOf(`CREATE JOIN TOKEN FOR REPLICA ${name} EXPIRES ${life};`, SCREEN);
        const token2 = answered2 !== null && answered2.kind === "value" ? answered2.value : null;
        if (typeof token2 !== "string") {
          say("join-status", "the node answered without a token", true);
          return;
        }
        write("join-token", `--join-token ${token2}

Shown once. The node keeps only its digest.`);
        hide("join-token", false);
        say("join-status", "issued");
        hereAgain();
      } catch (failure) {
        say("join-status", told(failure), true);
      }
    });
    at("failover-apply").addEventListener("click", async () => {
      const composed = policy();
      if ("statement" in composed && await send(composed.statement, "failover-status")) {
        hereAgain();
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
    const chosen2 = value("new-role");
    return chosen2 === "other" ? trimmed("new-role-other") : chosen2;
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
    const statement2 = definition();
    write(
      "define-preview",
      statement2 === null || space === null ? missing() : definitionSays(trimmed("new-name"), role(), space)
    );
  }
  function shapeTheForm() {
    hide("scope-field", value("new-reach") === "node");
    hide("role-other-field", value("new-role") !== "other");
    preview2();
  }
  function changedRole() {
    const chosen2 = value("change-role");
    return chosen2 === "other" ? trimmed("change-role-other") : chosen2;
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
  function removal2() {
    const name = trimmed("remove-name");
    const again = trimmed("remove-confirm");
    return name !== "" && name === again ? "DROP USER " + name + ";" : null;
  }
  function removeWhy() {
    return trimmed("remove-why");
  }
  function shapeTheRemoval() {
    const statement2 = removal2();
    disable("remove", statement2 === null);
    const name = trimmed("remove-name");
    write(
      "remove-radius",
      statement2 !== null ? removalSays(name) : name === "" ? "a name is needed" : "type the same name again to confirm"
    );
    write("remove-preview", statement2 ?? "");
  }
  function wire9() {
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
  //!
  //! # What a row must say, and what the form adds
  //!
  //! A row with no `REPLICATES` is subscribed to nothing — every node up, every
  //! greeting landing, one copy that never changes — so the subscription is a
  //! field with `STORE` already in it rather than a clause left to memory. The
  //! identity is a node id or a pinned certificate fingerprint (ADR-0108 D9); a
  //! row with neither binds nobody until a join token is issued for it below.
  //!
  //! This node's own row is offered first, filled from the node. After the
  //! transaction the form sets this node's roles to what its row declares, with
  //! `DEFINE NODE ROLES` — local and never fenced — because a node left at the
  //! default `serving, writable` is clustered and a candidate for nothing: it
  //! stops accepting writes and never starts again.
  var ROWS = 5;
  var BITS2 = ["serving", "writable", "coordinating"];
  var IDENTIFYING = ["name", "endpoint", "clients", "node", "fingerprint"];
  var TEXTS = [...IDENTIFYING, "replicates"];
  var REACH = /^(STORE|NAMESPACE [A-Za-z_][A-Za-z0-9_]*|DATABASE [A-Za-z_][A-Za-z0-9_]*\.[A-Za-z_][A-Za-z0-9_]*)$/i;
  var thisNode = null;
  function intended() {
    const found = [];
    for (let index = 0; index < ROWS; index += 1) {
      const read = (part) => trimmed(`peer-${index}-${part}`);
      if (IDENTIFYING.every((part) => read(part) === "")) {
        continue;
      }
      const roles = BITS2.filter((bit) => at(`peer-${index}-${bit}`).checked);
      found.push({
        name: read("name"),
        endpoint: read("endpoint"),
        clients: read("clients"),
        node: read("node"),
        fingerprint: read("fingerprint"),
        replicates: read("replicates").replace(/\s+/g, " "),
        roles
      });
    }
    return found;
  }
  function incomplete(rows2) {
    for (const [index, row] of rows2.entries()) {
      const missing3 = aName(row.name) === null ? "a name: a letter or _, then letters, digits or _" : row.endpoint === "" ? "a peer address" : row.node !== "" && row.fingerprint !== "" ? "a node id or a fingerprint, not both" : row.fingerprint !== "" && aFingerprint(row.fingerprint) === null ? "a fingerprint of 64 hexadecimal digits" : !REACH.test(row.replicates) ? "what it replicates: STORE, NAMESPACE n or DATABASE n.d" : row.roles.length === 0 ? "at least one role" : null;
      if (missing3 !== null) {
        return `row ${index + 1} needs ${missing3}`;
      }
    }
    return null;
  }
  var own = (rows2) => thisNode === null ? void 0 : rows2.find((row) => row.node === thisNode?.id);
  function formation(rows2) {
    const declarations = rows2.map((row) => {
      const pinned = aFingerprint(row.fingerprint);
      return `DEFINE REPLICA ${row.name} AT ${quoted(row.endpoint)}` + (row.clients === "" ? "" : ` CLIENTS AT ${quoted(row.clients)}`) + (row.node === "" ? "" : ` NODE ${quoted(row.node)}`) + ` ROLES ${row.roles.join(", ")} REPLICATES ${row.replicates}` + (pinned === null ? "" : ` FINGERPRINT '${pinned}'`) + ";";
    });
    const mine2 = own(rows2);
    return [
      "BEGIN;",
      ...declarations,
      "COMMIT;",
      ...mine2 === void 0 ? [] : [`DEFINE NODE ROLES ${mine2.roles.join(", ")};`]
    ].join("\n");
  }
  function preview3() {
    const rows2 = intended();
    const missing3 = incomplete(rows2);
    if (rows2.length === 0) {
      say("form-says", "Nothing declared yet.");
      return;
    }
    if (missing3 !== null) {
      say("form-says", missing3, true);
      return;
    }
    const named = rows2.map((row) => row.name).join(", ");
    const waiting = rows2.filter((row) => row.node === "" && row.fingerprint === "").map((row) => row.name);
    const mine2 = own(rows2);
    say(
      "form-says",
      `Declares ${rows2.length === 1 ? "one member" : `${rows2.length} members`} — ${named} — in a single transaction. All of them or none. ` + (mine2 === void 0 ? "This node keeps the roles it has, as none of these rows names it — and once clustered, a node writes only while it holds coordinating. " : mine2.roles.includes("coordinating") ? `Then sets this node's roles to ${mine2.roles.join(", ")}. ` : "This node's row leaves out coordinating: once clustered, it stops accepting writes for good. ") + (waiting.length === 0 ? "" : `${waiting.join(", ")} will wait for a join token.`)
    );
  }
  function showStatement() {
    const rows2 = intended();
    clear("form-statement");
    const block = made("pre");
    block.textContent = rows2.length === 0 || incomplete(rows2) !== null ? "" : formation(rows2);
    at("form-statement").appendChild(block);
  }
  function changed() {
    preview3();
    showStatement();
  }
  function know(id, endpoints, peers) {
    if (typeof id !== "string") {
      return;
    }
    const endpoint = Array.isArray(endpoints) && typeof endpoints[0] === "string" ? endpoints[0] : "";
    thisNode = { id, endpoint };
    if (peers > 0 || IDENTIFYING.some((part) => trimmed(`peer-0-${part}`) !== "")) {
      changed();
      return;
    }
    setValue("peer-0-name", "this_node");
    setValue("peer-0-endpoint", endpoint);
    setValue("peer-0-node", id);
    for (const bit of BITS2) {
      at(`peer-0-${bit}`).checked = true;
    }
    changed();
  }
  function wire10() {
    for (let index = 0; index < ROWS; index += 1) {
      for (const part of [...TEXTS, ...BITS2]) {
        at(`peer-${index}-${part}`).addEventListener("input", changed);
        at(`peer-${index}-${part}`).addEventListener("change", changed);
      }
    }
    at("form-cluster").addEventListener("click", async () => {
      const rows2 = intended();
      if (rows2.length === 0) {
        say("form-status", "nothing to declare", true);
        return;
      }
      const missing3 = incomplete(rows2);
      if (missing3 !== null) {
        say("form-status", missing3, true);
        return;
      }
      say("form-status", "running…");
      try {
        const answered2 = await valueOf(formation(rows2), "Cluster · form");
        const done = answered2 !== null && answered2.kind === "done";
        say("form-status", done ? "declared" : "");
        if (done) {
          for (let index = 0; index < ROWS; index += 1) {
            for (const part of IDENTIFYING) {
              setValue(`peer-${index}-${part}`, "");
            }
            setValue(`peer-${index}-replicates`, "STORE");
            for (const bit of BITS2) {
              at(`peer-${index}-${bit}`).checked = false;
            }
          }
          changed();
          hereAgain();
        }
      } catch (failure) {
        say("form-status", told(failure), true);
      }
    });
    changed();
  }

  // src/grants.ts
  //! Giving and taking away what one account may reach.
  //!
  //! The third action S2.1 names, beside a role change and a removal, and the one
  //! the console had no surface for. `GRANT` and `REVOKE` were in the language the
  //! whole time; what was missing was a screen.
  //!
  //! # Two counterintuitive rules, both stated before the button
  //!
  //! The engine's own doc carries them and they are the reason this screen needs a
  //! blast radius rather than a confirmation:
  //!
  //! 1. **A user's first table grant NARROWS them.** A user with no grants is
  //!    governed by their role; a user with one reaches exactly what they were
  //!    granted. So giving `read` on one table takes away everything else the role
  //!    allowed — the opposite of what "grant" sounds like.
  //! 2. **Taking away the LAST table grant WIDENS them**, back to their whole
  //!    role, so the node refuses it. An operator running a revoke is thinking
  //!    about narrowing, and this is the one revoke that does the reverse.
  //!
  //! Neither is discoverable from the form. Both are in the radius line.
  //!
  //! # Authorities are a different question from table grants
  //!
  //! One asks *which of my tables*, the other *how much of this store*, and the
  //! language keeps them as separate statements for that reason. This screen keeps
  //! them as one control with a reach chooser, because to the operator they are
  //! one decision — who may do what, and how far.
  var reachOf = () => value("grant-reach");
  function statement() {
    const who = trimmed("grant-who");
    const what = trimmed("grant-what");
    const reach3 = reachOf();
    const name = trimmed("grant-name");
    const giving = value("grant-direction") === "give";
    if (who === "" || what === "" || reach3 !== "store" && name === "") {
      return null;
    }
    if (reach3 === "table") {
      const parts = name.split(".");
      if (parts.length !== 3) {
        return null;
      }
      const [namespaceOf, databaseOf, table] = parts;
      const act2 = giving ? `GRANT ${what} ON ${table} TO ${who};` : `REVOKE ${what} ON ${table} FROM ${who};`;
      return `USE NAMESPACE ${namespaceOf}; USE DATABASE ${databaseOf}; ${act2}`;
    }
    const target = reach3 === "store" ? "STORE" : `${reach3 === "namespace" ? "NAMESPACE" : "DATABASE"} ${name}`;
    return giving ? `GRANT ${what} ON ${target} TO ${who};` : `REVOKE ${what} ON ${target} FROM ${who};`;
  }
  function missing2() {
    if (trimmed("grant-who") === "") {
      return "a name is needed";
    }
    if (trimmed("grant-what") === "") {
      return "say what — read, write, manage, operate or replicate";
    }
    if (reachOf() === "table" && trimmed("grant-name").split(".").length !== 3) {
      return "name the table in full, as namespace.database.table";
    }
    return "name the table, namespace or database it is on";
  }
  function says() {
    const who = trimmed("grant-who");
    const what = trimmed("grant-what");
    const giving = value("grant-direction") === "give";
    const table = reachOf() === "table";
    const today = lookup(who);
    const standing = today === null ? ` This panel has not been told what ${who} reaches — press List to find out.` : ` Today ${who} is ${today.role} in ${today.reach}.`;
    if (table && giving) {
      return `Gives ${who} ${what} on ${trimmed("grant-name")}. If they hold no table grant yet this NARROWS them: a user with grants reaches exactly what they were granted, and nothing else their role would have allowed.` + standing;
    }
    if (table) {
      return `Takes ${what} on ${trimmed("grant-name")} away from ${who}. If it is their LAST table grant the node will refuse it — going from one grant to none widens them back to their whole role, which is the opposite of a revoke.` + standing;
    }
    const where3 = reachOf() === "store" ? "the whole store" : trimmed("grant-name");
    return giving ? `Gives ${who} ${what} over ${where3}.${standing}` : `Takes ${what} over ${where3} away from ${who}. An authority going to none leaves them holding nothing there, which the node allows.${standing}`;
  }
  function shape2() {
    hide("grant-name-field", reachOf() === "store");
    say("grant-says", statement() === null ? missing2() : says());
  }
  function wire11() {
    for (const field of [
      "grant-who",
      "grant-what",
      "grant-reach",
      "grant-name",
      "grant-direction",
      "grant-why"
    ]) {
      at(field).addEventListener("input", shape2);
      at(field).addEventListener("change", shape2);
    }
    at("grant-apply").addEventListener("click", async () => {
      const sending = statement();
      if (sending === null) {
        say("grant-status", missing2(), true);
        return;
      }
      const why = trimmed("grant-why");
      if (why === "") {
        say("grant-status", "say why — this changes what somebody may reach", true);
        return;
      }
      say("grant-status", "running…");
      try {
        const answered2 = await valueOf(sending, "Access · grant", why);
        say("grant-status", answered2 !== null && answered2.kind === "done" ? "done" : "");
      } catch (failure) {
        say("grant-status", told(failure), true);
      }
    });
    shape2();
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
  var BITS3 = [
    { name: "serving", letter: "S", means: "answers client requests" },
    {
      name: "writable",
      letter: "W",
      means: "accepts writes rather than forwarding them"
    },
    {
      name: "coordinating",
      letter: "C",
      means: "takes part in deciding, not only in storing"
    }
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
    for (const bit of BITS3) {
      const held5 = has.includes(bit.name);
      const asked2 = wanted2 !== null && wanted2.includes(bit.name);
      row.appendChild(
        lamp(
          bit.letter,
          held5 ? "held" : asked2 ? "wanted" : "off",
          bit.means
        )
      );
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
  function lease(held5) {
    if (held5 === void 0 || held5 === null) {
      return null;
    }
    const badge = made("div", "lease");
    const held_ = held5;
    const until = told2(held_.until ?? held_.expires ?? held5);
    badge.textContent = until === null ? "holds the lease" : `holds the lease until ${until}`;
    return badge;
  }
  var drained = (has) => has.length === 0;
  function figure(title, has, wanted2, facts2, kind2, subject) {
    const box = made("article", "node " + kind2);
    box.tabIndex = 0;
    box.setAttribute("role", "button");
    box.setAttribute("aria-label", `${title} — open its drawer`);
    box.addEventListener("click", () => show3(subject));
    box.addEventListener("keydown", (pressed) => {
      if (pressed.key === "Enter" || pressed.key === " ") {
        pressed.preventDefault();
        show3(subject);
      }
    });
    const head = made("div", "node-head");
    const name = made("h3");
    name.textContent = title;
    head.append(name, lamps(has, wanted2));
    box.appendChild(head);
    if (drained(has)) {
      const note2 = made("p", "note warn");
      note2.textContent = kind2 === "self" ? "Drained — it holds its data and answers nothing." : "Declared with no roles — drained.";
      box.appendChild(note2);
    }
    if (kind2 === "peer") {
      const note2 = made("p", "faint");
      note2.textContent = "lamps as declared here; this node has not asked it";
      box.appendChild(note2);
    }
    for (const one2 of facts2) {
      if (one2 !== null) {
        box.appendChild(one2);
      }
    }
    return box;
  }
  function furthest(followers) {
    let most = null;
    for (const one2 of followers) {
      if (typeof one2.behind === "number" && (most === null || one2.behind > most)) {
        most = one2.behind;
      }
    }
    return most === null ? null : `${most} record(s)`;
  }
  function bare(node) {
    return typeof node === "string" ? node.split("-").join("").toLowerCase() : null;
  }
  function leading(leaders, node) {
    const mine2 = bare(node);
    if (mine2 === null) {
      return null;
    }
    const ranges = leaders.filter((one2) => bare(one2.node) === mine2).map((one2) => `${told2(one2.range)} (epoch ${told2(one2.epoch)})`);
    return ranges.length === 0 ? null : ranges.join(", ");
  }
  function behindEach(followers) {
    const each = followers.filter((one2) => typeof one2.behind === "number").map((one2) => `${(bare(one2.node) ?? "?").slice(0, 8)}… ${told2(one2.behind)} behind`);
    return each.length === 0 ? null : each.join(", ");
  }
  function placement(peer) {
    const range = told2(peer.leads);
    if (range === null) {
      return null;
    }
    const said3 = [range];
    if (peer.preferred === true) {
      said3.push("preferred");
    }
    if (peer.releasing === true) {
      said3.push("being given back to the store line");
    }
    return said3.join(", ");
  }
  function balancedTables(balanced) {
    const each = (balanced ?? []).map((one2) => {
      const shards = (one2.shards ?? []).map((shard) => `${shard.complete === false ? "≥" : ""}${told2(shard.records) ?? "?"}`).join(" / ");
      const act2 = told2(one2.last_act);
      return `${told2(one2.table) ?? "?"}: ${shards}${act2 === null ? "" : ` (last: ${act2})`}`;
    });
    return each.length === 0 ? null : each.join("; ");
  }
  function ended2(across) {
    if (across === void 0) {
      return null;
    }
    return `${told2(across.committed) ?? "?"} committed, ${told2(across.aborted) ?? "?"} aborted, ${told2(across.in_doubt) ?? "?"} in doubt`;
  }
  function copied(upstream) {
    if (typeof upstream?.copies !== "number" || upstream.copies === 0) {
      return null;
    }
    return `${told2(upstream.copied_records)} record(s) in ${upstream.copies} cop${upstream.copies === 1 ? "y" : "ies"}`;
  }
  function draw3(into, seen) {
    const cluster = seen.cluster ?? {};
    const mine2 = seen.roles ?? [];
    const wanted2 = cluster.desired ?? null;
    const self = figure(
      "This node",
      mine2,
      wanted2,
      [
        lease(cluster.lease),
        fact("id", told2(seen.id)),
        fact("answers on", (seen.endpoints ?? []).join(", ") || null),
        fact("epoch", told2(cluster.epoch)),
        fact("campaigns", told2(cluster.campaigns)),
        fact(
          "collecting from here",
          String((cluster.followers ?? []).length)
        ),
        fact("furthest follower behind", furthest(cluster.followers ?? [])),
        fact("each follower", behindEach(cluster.followers ?? [])),
        fact("leads, as the log records", leading(cluster.leaders ?? [], seen.id)),
        // Absent on a node that follows nobody: `in sync` there would be a
        // state it has never been in.
        fact("sync with its upstream", cluster.upstream?.state ?? null),
        fact("copied from its upstream", copied(cluster.upstream)),
        fact("across leaders, coordinated here", ended2(cluster.across)),
        // `null` until the node's settling pass has looked, so the map says
        // nothing rather than a zero nobody measured.
        fact("records still pending here", told2(cluster.across?.pending)),
        fact("transactions holding intents here", told2(cluster.across?.with_intents)),
        fact(
          "balances leaderships",
          cluster.failover?.balance_leaderships === true ? "yes, when one node leads two lines more" : null
        ),
        fact("balanced tables, records per shard", balancedTables(cluster.balanced)),
        wanted2 === null ? null : fact("declared for it", wanted2.join(", "))
      ],
      "self",
      {
        name: "This node",
        self: true,
        endpoint: null,
        node: null,
        roles: mine2,
        declared: wanted2
      }
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
            // A redirect and a forwarded write go here rather than to the peer
            // door above, so a row that names it is worth showing (ADR-0101).
            fact(
              "clients reach it at",
              typeof peer.clients === "string" ? peer.clients : null
            ),
            fact("id", told2(peer.node)),
            fact("replicates", told2(peer.replicates)),
            fact("leads", placement(peer)),
            fact("leads, as the log records", leading(cluster.leaders ?? [], peer.node))
          ],
          "peer",
          {
            name: peer.name ?? "",
            self: false,
            endpoint: peer.endpoint ?? null,
            node: peer.node ?? null,
            roles: peer.roles ?? [],
            // A peer's declaration is not this node's to read, and the drawer
            // offers it no drain to qualify.
            declared: null
          }
        )
      );
    }
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
  function state(id, kind2, words4) {
    const line = at(id);
    line.textContent = words4;
    for (const other of ["waiting", "empty", "partial", "wrong"]) {
      line.classList.toggle(`is-${other}`, other === kind2);
    }
    line.classList.toggle("failed", kind2 === "wrong");
    line.setAttribute("aria-live", kind2 === "wrong" ? "assertive" : "polite");
  }
  function settled(id) {
    const line = at(id);
    line.textContent = "";
    for (const other of ["waiting", "empty", "partial", "wrong"]) {
      line.classList.remove(`is-${other}`);
    }
    line.classList.remove("failed");
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
    const words4 = typeof body === "object" && body !== null ? Object.entries(body).map(([name, value2]) => name + " " + String(value2)).join(", ") : String(body).trim();
    return scraped.status + " " + words4;
  }
  async function readNode() {
    say("node-status", "asking…");
    try {
      const answered2 = held2(await valueOf("INFO FOR NODE;", "Node"));
      const all2 = answered2 ?? {};
      const { cluster, ...mine2 } = all2;
      facts("node-facts", mine2);
      const peers = typeof cluster === "object" && cluster !== null ? cluster.peers : void 0;
      clear("cluster-map");
      draw3(at("cluster-map"), all2);
      draw2(all2);
      know(all2["id"], all2["endpoints"], Array.isArray(peers) ? peers.length : 0);
      facts("cluster-facts", {
        roles: all2["roles"],
        peers: peers ?? [],
        endpoints: all2["endpoints"],
        id: all2["id"]
      });
      const joining = Array.isArray(all2["certificates"]) && all2["certificates"].some(
        (shown3) => typeof shown3 === "object" && shown3 !== null && "surface" in shown3 && shown3.surface === "peers"
      );
      if (!Array.isArray(peers) || peers.length === 0) {
        state(
          "cluster-status",
          "empty",
          joining ? "This node holds a peer certificate and names nobody yet. To join through its seed, open This node above and set its roles to serving: a writable node stays its own authority and collects from nobody. To found a cluster here, declare the membership below." : "No peers — this node holds everything itself. Declare the membership below to add them, all at once."
        );
      } else {
        settled("cluster-status");
      }
    } catch (failure) {
      clear("node-facts");
      clear("cluster-map");
      clear("cluster-facts");
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
  function wire12() {
    at("node-refresh").addEventListener("click", readNode);
    onArrival(["cluster", "this-node"], () => void readNode());
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
  function wire13() {
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
      for (const note2 of result.notes ?? []) {
        pane2.appendChild(trailer("note " + note2.kind + ": " + note2.message));
      }
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
  function wire14() {
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
  //! The shapes are told apart by their PUNCTUATION rather than by which question
  //! happens to answer first: a colon is a record, three dotted parts are a table,
  //! two are a database, and one bare word is an account or a namespace. Only that
  //! last pair overlaps, and there `ada` resolving to the account first is the
  //! right guess for a console whose destructive screens are all about accounts.
  //!
  //! It was an ORDER until W319 drove the field against a running node. A bare
  //! `orders` asked `INFO FOR TABLE orders`, which can never resolve — every
  //! `/script` request is its own session, so there is no selected namespace for
  //! it to be a table IN — and `prod.library.orders` took the first two parts as a
  //! database, found one, and opened a sheet headed *database · prod.library.orders*
  //! listing that database's tables. A wrong answer, under a name nothing is
  //! called, reported as success. The record key had already been fixed this way
  //! four lines above; the table had not.
  var inDetail = (kind2, name) => (answered2) => show2(kind2, name, answered2);
  function splitAt(text, separator) {
    const at2 = text.indexOf(separator);
    return at2 < 0 ? [text, ""] : [text.slice(0, at2), text.slice(at2 + separator.length)];
  }
  function candidates(text) {
    const record2 = text.includes(":");
    const parts = record2 ? [] : text.split(".");
    const out = [];
    if (record2) {
      const [reach3, key] = splitAt(text, ":");
      const parts2 = reach3.split(".");
      if (parts2.length === 3) {
        const [namespaceOf, databaseOf, table] = parts2;
        out.push({
          kind: "record",
          statement: `USE NAMESPACE ${namespaceOf}; USE DATABASE ${databaseOf}; SELECT * FROM ${table}:${key};`,
          // A record is the one thing here with a history, so it is the one
          // `land` that asks a second question. The ask lives here and not in
          // the sheet because `detail.ts` reaches the node through nothing —
          // `api.ts`, `password.ts` and `session.ts` are the only modules that
          // may, and a test says so.
          land: async (answered2) => {
            let history = null;
            try {
              history = await valueOf(
                `USE NAMESPACE ${namespaceOf}; USE DATABASE ${databaseOf}; INFO FOR HISTORY OF ${table}:${key};`,
                "Record · history"
              );
            } catch {
              history = null;
            }
            show2("record", text, answered2, history);
          }
        });
      }
    }
    if (parts.length === 3) {
      const [namespace, database, table] = parts;
      out.push({
        kind: "table",
        statement: `USE NAMESPACE ${namespace}; USE DATABASE ${database}; INFO FOR TABLE ${table};`,
        land: inDetail("table", text)
      });
    }
    if (parts.length === 2) {
      const [namespace, database] = parts;
      out.push({
        kind: "database",
        statement: "USE NAMESPACE " + namespace + "; USE DATABASE " + database + "; INFO FOR DATABASE;",
        land: inDetail("database", text)
      });
    }
    if (parts.length === 1) {
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
      out.push({
        kind: "namespace",
        statement: "USE NAMESPACE " + text + "; INFO FOR NAMESPACE;",
        land: inDetail("namespace", text)
      });
    }
    return out;
  }
  async function look() {
    const text = trimmed("search");
    if (text === "") {
      return;
    }
    write("search-says", "looking…");
    hide2();
    for (const candidate of candidates(text)) {
      try {
        const answered2 = await valueOf(candidate.statement, "Search · " + candidate.kind);
        write("search-says", "");
        candidate.land(answered2);
        return;
      } catch {
        continue;
      }
    }
    write(
      "search-says",
      text.includes(":") ? "nothing here answers to that — name a record in full, as namespace.database.table:key" : text.includes(".") ? "nothing here answers to that name" : "nothing here answers to that name — a table is named in full, as namespace.database.table"
    );
  }
  function wire15() {
    at("search").addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        void look();
      }
    });
    document.addEventListener("keydown", (event) => {
      const focused = document.activeElement;
      const typing2 = focused instanceof HTMLInputElement || focused instanceof HTMLTextAreaElement || focused instanceof HTMLSelectElement;
      const shortcut = event.key === "k" && (event.metaKey || event.ctrlKey);
      if (shortcut || event.key === "/" && !typing2) {
        event.preventDefault();
        at("search").focus();
        at("search").select();
      }
    });
  }

  // src/shortcuts.ts
  //! Every key this console answers to, and one place that says so.
  //!
  //! A shortcut nobody can find is not reachable. Three handlers scattered through
  //! three modules is what the console had: `/` and `⌘K` for the search, `⌘↵` for
  //! the script, `Escape` for whatever was open — each real, each known only to
  //! whoever wrote it or read the source.
  //!
  //! So the list below is the ONE place they are written down, the sheet renders
  //! it, and `?` opens the sheet. The handlers still live with the screens they
  //! belong to, because a module that owned every key on the page would be a
  //! module that has to know about every screen.
  //!
  //! # Why the list is data and not prose
  //!
  //! It is rendered, so a shortcut added without a row here is a shortcut with no
  //! row in the sheet — visible immediately to anybody who opens it, rather than
  //! a documentation drift nobody notices for a year.
  var KEYS = [
    { press: "/", does: "find an account, a table, a namespace or a record" },
    { press: "⌘K  ·  Ctrl-K", does: "the same, from inside a field" },
    { press: "⌘1 … ⌘7", does: "Run, Topics, Cluster, Access, This node, Backup, Vault" },
    { press: "⌘↵  ·  Ctrl-↵", does: "run what is in the script box" },
    { press: "?", does: "this list" },
    { press: "Esc", does: "close the log, the drawer, or this list" }
  ];
  function typing() {
    const focused = document.activeElement;
    return focused instanceof HTMLInputElement || focused instanceof HTMLTextAreaElement || focused instanceof HTMLSelectElement;
  }
  function draw4() {
    clear("keys-list");
    const list = made("dl", "keys");
    for (const key of KEYS) {
      const press = made("dt");
      press.textContent = key.press;
      const does = made("dd");
      does.textContent = key.does;
      list.append(press, does);
    }
    at("keys-list").appendChild(list);
  }
  function closeIt4() {
    at("keys-sheet").hidden = true;
  }
  var DESTINATIONS = ["run", "topics", "cluster", "access", "this-node", "backup", "vault"];
  function wire16() {
    draw4();
    document.addEventListener("keydown", (event) => {
      if (event.key === "Escape" && !at("keys-sheet").hidden) {
        closeIt4();
        return;
      }
      if (event.key === "?" && !typing()) {
        event.preventDefault();
        at("keys-sheet").hidden = !at("keys-sheet").hidden;
        return;
      }
      if (!(event.metaKey || event.ctrlKey)) {
        return;
      }
      const at_ = Number.parseInt(event.key, 10) - 1;
      const wanted2 = DESTINATIONS[at_];
      if (wanted2 !== void 0) {
        event.preventDefault();
        show(wanted2);
      }
    });
    at("keys-close").addEventListener("click", closeIt4);
    at("keys-open").addEventListener("click", () => {
      at("keys-sheet").hidden = !at("keys-sheet").hidden;
    });
  }

  // src/topic-info.ts
  //! What the node says about a topic, checked before anything draws it.
  //!
  //! `INFO FOR TOPIC` is a body this page did not construct, so each field is
  //! narrowed here once and the screens after this point trust the shape.
  var whole = (value2) => typeof value2 === "number" && Number.isInteger(value2) && value2 >= 0 ? value2 : null;
  var fieldsOf = (value2) => typeof value2 === "object" && value2 !== null && !Array.isArray(value2) ? value2 : null;
  function reader(value2) {
    const fields = fieldsOf(value2);
    const position = whole(fields?.["position"]);
    const lag = whole(fields?.["lag"]);
    return position === null || lag === null ? null : { position, lag };
  }
  function group(value2) {
    const fields = fieldsOf(value2);
    const at2 = (name) => whole(fields?.[name]);
    const position = at2("position");
    const committed = at2("committed");
    const lag = at2("lag");
    const inFlight = at2("in_flight");
    const redelivered = at2("redelivered");
    const deadLettered = at2("dead_lettered");
    const width = at2("width");
    const deadline = fields?.["deadline"];
    if (position === null || committed === null || lag === null || inFlight === null || redelivered === null || deadLettered === null || width === null || typeof deadline !== "string") {
      return null;
    }
    return {
      position,
      committed,
      lag,
      in_flight: inFlight,
      redelivered,
      dead_lettered: deadLettered,
      deadline,
      width
    };
  }
  function ingest(value2) {
    const fields = fieldsOf(value2);
    const group2 = fields?.["group"];
    const into = fields?.["into"];
    const running = fields?.["running"];
    return typeof group2 === "string" && typeof into === "string" && typeof running === "boolean" ? { group: group2, into, running } : null;
  }
  function entries(value2, narrow) {
    const found = /* @__PURE__ */ new Map();
    for (const [name, each] of Object.entries(fieldsOf(value2) ?? {})) {
      const narrowed = narrow(each);
      if (narrowed !== null) {
        found.set(name, narrowed);
      }
    }
    return found;
  }
  function topic(value2) {
    const fields = fieldsOf(value2);
    const name = fields?.["name"];
    const last = whole(fields?.["last"]);
    if (fields === null || typeof name !== "string" || last === null) {
      return null;
    }
    const retain = fields["retain"];
    return {
      name,
      first: whole(fields["first"]),
      last,
      retain: typeof retain === "string" ? retain : null,
      retainBytes: whole(fields["retain_bytes"]),
      bytes: whole(fields["bytes"]),
      maxBytes: whole(fields["max_bytes"]),
      readers: entries(fields["consumers"], reader),
      groups: entries(fields["groups"], group),
      ingestedBy: entries(fields["ingested_by"], ingest)
    };
  }
  function keeps(topic2) {
    const limits = [
      ...topic2.retain === null ? [] : [topic2.retain],
      ...topic2.retainBytes === null ? [] : [`${topic2.retainBytes} bytes`]
    ];
    return limits.length === 0 ? "everything" : limits.join(", ");
  }
  var held4 = (topic2) => topic2.first === null || topic2.last < topic2.first ? 0 : topic2.last - topic2.first + 1;
  function behind(topic2) {
    const lags = [...topic2.readers.values(), ...topic2.groups.values()].map((each) => each.lag);
    return lags.length === 0 ? 0 : Math.max(...lags);
  }

  // src/topic-list.ts
  //! The Topics screen's reading: where, which topics, the chosen one, its messages.
  //!
  //! Everything here reads. Nothing moves a position — the message browser uses
  //! `READ FROM … AFTER n` with no consumer, which is a look and nothing more.
  var SCREEN2 = "Topics";
  var TOPICS_SHOWN = 200;
  function where2() {
    const namespace = aName(value("topics-namespace"));
    const database = aName(value("topics-database"));
    return namespace === null || database === null ? null : { namespace, database };
  }
  var topics = [];
  var chosenName = null;
  var listeners = [];
  var chosen = () => topics.find((each) => each.name === chosenName) ?? null;
  function whenChosen(todo) {
    listeners.push(todo);
  }
  var words = (failure) => failure instanceof Unreachable ? "the node did not answer — " + told(failure) : told(failure);
  async function results(source) {
    const { text } = await ask(source, SCREEN2);
    const body = JSON.parse(text);
    if (!Array.isArray(body.results)) {
      throw new Error(typeof body.error === "string" ? body.error : text);
    }
    return body.results;
  }
  function strings(answered2, field) {
    const list = held2(answered2)?.[field];
    return Array.isArray(list) ? list.filter((each) => typeof each === "string") : [];
  }
  function offer(id, names) {
    const select = at(id);
    const kept2 = value(id);
    clear(id);
    for (const name of names) {
      const option = made("option");
      option.value = name;
      option.textContent = name;
      select.appendChild(option);
    }
    if (names.includes(kept2)) {
      setValue(id, kept2);
    }
  }
  async function readNamespaces() {
    state("topics-status", "waiting", "asking…");
    try {
      offer("topics-namespace", strings(await valueOf("INFO FOR STORE;", SCREEN2), "namespaces"));
    } catch (failure) {
      state("topics-status", "wrong", words(failure));
      return;
    }
    await readDatabases();
  }
  async function readDatabases() {
    const namespace = aName(value("topics-namespace"));
    if (namespace === null) {
      offer("topics-database", []);
      draw5([]);
      state("topics-status", "empty", "No namespace here — declare one on Run with DEFINE NAMESPACE.");
      return;
    }
    try {
      const answered2 = await valueOf(`USE NAMESPACE ${namespace}; INFO FOR NAMESPACE;`, SCREEN2);
      offer("topics-database", strings(answered2, "databases"));
    } catch (failure) {
      state("topics-status", "wrong", words(failure));
      return;
    }
    await readTopics();
  }
  async function readTopics() {
    const place2 = where2();
    if (place2 === null) {
      draw5([]);
      state("topics-status", "empty", "Choose a namespace and a database that exist.");
      return;
    }
    const start = tenancy(place2.namespace, place2.database);
    state("topics-status", "waiting", "asking…");
    try {
      const listed2 = strings(await valueOf(start + "INFO FOR DATABASE;", SCREEN2), "topics");
      const names = listed2.filter((name) => aName(name) !== null);
      const shown3 = names.slice(0, TOPICS_SHOWN);
      const asked2 = shown3.map((name) => `INFO FOR TOPIC ${name};`).join(" ");
      const answered2 = shown3.length === 0 ? [] : await results(start + asked2);
      const read = answered2.map((each) => topic(each.value)).filter((each) => each !== null);
      draw5(read);
      if (names.length === 0) {
        const here2 = `${place2.namespace}.${place2.database}`;
        state("topics-status", "empty", `${here2} holds no topics — create one below.`);
      } else if (names.length > shown3.length) {
        state("topics-status", "partial", `showing ${shown3.length} of ${names.length} topics`);
      } else {
        settled("topics-status");
      }
    } catch (failure) {
      draw5([]);
      state("topics-status", "wrong", words(failure));
    }
  }
  function numberCell(row, figure2) {
    const box = row.insertCell();
    box.textContent = String(figure2);
    box.classList.add("number");
  }
  function headed(names) {
    const table = made("table");
    const head = table.createTHead().insertRow();
    for (const name of names) {
      const column = made("th");
      column.textContent = name;
      head.appendChild(column);
    }
    return table;
  }
  function draw5(read) {
    topics = read;
    if (chosen() === null) {
      chosenName = null;
    }
    clear("topics-list");
    if (read.length > 0) {
      const table = headed(["topic", "held", "last", "keeps", "readers", "groups", "most behind"]);
      const body = table.createTBody();
      for (const each of read) {
        const row = body.insertRow();
        const pick2 = made("button", each.name === chosenName ? "quiet chosen" : "quiet");
        pick2.type = "button";
        pick2.textContent = each.name;
        pick2.setAttribute("aria-pressed", String(each.name === chosenName));
        pick2.addEventListener("click", () => choose(each.name));
        row.insertCell().appendChild(pick2);
        numberCell(row, held4(each));
        numberCell(row, each.last);
        row.insertCell().textContent = keeps(each);
        numberCell(row, each.readers.size);
        numberCell(row, each.groups.size);
        numberCell(row, behind(each));
      }
      at("topics-list").appendChild(table);
    }
    drawChosen();
  }
  function choose(name) {
    chosenName = name;
    const found = chosen();
    setValue("browse-after", String(found === null || found.first === null ? 0 : found.first - 1));
    clear("browse-list");
    settled("browse-status");
    draw5(topics);
  }
  function drawChosen() {
    const found = chosen();
    at("topic-chosen").textContent = found?.name ?? "none chosen";
    for (const id of ["topic-facts", "topic-readers", "topic-groups", "topic-ingested"]) {
      clear(id);
    }
    disable("browse-read", found === null);
    disable("browse-next", found === null);
    if (found !== null) {
      facts("topic-facts", {
        held: held4(found),
        first: found.first ?? "none held",
        last: found.last,
        keeps: keeps(found),
        "bytes held": found.bytes === null ? "not counted" : `${found.bytes} bytes`,
        "largest message": found.maxBytes === null ? "any size" : `${found.maxBytes} bytes`
      });
      const readers = headed(["reader", "position", "lag"]);
      const readerRows = readers.createTBody();
      for (const [name, each] of found.readers) {
        const row = readerRows.insertRow();
        row.insertCell().textContent = name;
        numberCell(row, each.position);
        numberCell(row, each.lag);
      }
      at("topic-readers").appendChild(found.readers.size > 0 ? readers : note("nobody reads under a name"));
      const groups = headed([
        "group",
        "handed out",
        "committed",
        "lag",
        "in flight",
        "width",
        "redelivered",
        "dead letters",
        "deadline"
      ]);
      const groupRows = groups.createTBody();
      for (const [name, each] of found.groups) {
        const row = groupRows.insertRow();
        row.insertCell().textContent = name;
        const figures = [
          each.position,
          each.committed,
          each.lag,
          each.in_flight,
          each.width,
          each.redelivered,
          each.dead_lettered
        ];
        for (const figure2 of figures) {
          numberCell(row, figure2);
        }
        row.insertCell().textContent = each.deadline;
      }
      at("topic-groups").appendChild(found.groups.size > 0 ? groups : note("no group settles this topic"));
      const ingested = headed(["consumer", "group", "into", "here"]);
      const ingestedRows = ingested.createTBody();
      for (const [name, each] of found.ingestedBy) {
        const row = ingestedRows.insertRow();
        for (const cell2 of [name, each.group, each.into, each.running ? "running" : "not running"]) {
          row.insertCell().textContent = cell2;
        }
      }
      at("topic-ingested").appendChild(
        found.ingestedBy.size > 0 ? ingested : note("no topic consumer reads this topic")
      );
    }
    for (const todo of listeners) {
      todo();
    }
  }
  function note(words4) {
    const line = made("p", "empty");
    line.textContent = words4;
    return line;
  }
  async function browse() {
    const place2 = where2();
    const found = chosen();
    const after2 = aWhole(trimmed("browse-after"));
    const count = aWhole(trimmed("browse-count"));
    if (place2 === null || found === null || after2 === null || count === null || count < 1 || count > 100) {
      say("browse-status", "a position from 0 and a count from 1 to 100", true);
      return;
    }
    state("browse-status", "waiting", "reading…");
    try {
      const answered2 = await valueOf(
        tenancy(place2.namespace, place2.database) + `READ FROM ${found.name} AFTER ${after2} LIMIT ${count};`,
        SCREEN2
      );
      const records = answered2?.records ?? [];
      clear("browse-list");
      const table = headed(["position", "message"]);
      const body = table.createTBody();
      let last = after2;
      for (const record2 of records) {
        const inside = fieldsOf(record2.value) ?? {};
        const position = typeof inside["position"] === "number" ? inside["position"] : last;
        last = Math.max(last, position);
        const row = body.insertRow();
        numberCell(row, position);
        row.insertCell().textContent = JSON.stringify(inside["value"] ?? null);
      }
      at("browse-list").appendChild(table);
      at("browse-next").dataset["after"] = String(last);
      const said3 = (answered2?.notes ?? []).map((each) => each.message).join(" · ");
      const also = said3 === "" ? "" : ` — ${said3}`;
      if (records.length === 0) {
        state("browse-status", "empty", `nothing after position ${after2}${also}`);
      } else {
        say("browse-status", `${records.length} from position ${after2 + 1}${also}`);
      }
    } catch (failure) {
      state("browse-status", "wrong", words(failure));
    }
  }
  function wire17() {
    at("topics-namespace").addEventListener("change", () => void readDatabases());
    at("topics-database").addEventListener("change", () => void readTopics());
    at("topics-refresh").addEventListener("click", () => void readNamespaces());
    at("browse-read").addEventListener("click", () => void browse());
    at("browse-next").addEventListener("click", () => {
      setValue("browse-after", at("browse-next").dataset["after"] ?? trimmed("browse-after"));
      void browse();
    });
    onArrival(["topics"], () => void readNamespaces());
  }

  // src/topic-forms.ts
  //! The Topics screen's changes: a topic made or removed, a group made, moved or removed.
  //!
  //! Each form composes its statement from checked parts and says, in prose, what
  //! pressing the button will do — the numbers in that sentence come from the
  //! node's last answer about the chosen topic, so the reader sees the blast
  //! radius before the button is live. The three that lose something (removing a
  //! topic, removing a group, moving a group) ask for the name to be typed again.
  var SCREEN3 = "Topics";
  var MAX_IN_FLIGHT = 1e4;
  var PLACE = "choose a namespace and a database first";
  var TOPIC = "choose a topic in the list first";
  var DURATION2 = "a number and a unit: 500ms, 30s, 15m, 12h, 7d";
  var PLURAL = new Intl.PluralRules("en");
  var counted = (count, one2, many) => `${count} ${PLURAL.select(count) === "one" ? one2 : many}`;
  function optional(id, check) {
    const text = trimmed(id);
    return text === "" ? void 0 : check(text);
  }
  function newTopic() {
    const place2 = where2();
    const name = aName(trimmed("new-topic-name"));
    const retain = optional("new-topic-retain", aDuration);
    const kept2 = optional("new-topic-kept", aWhole);
    const bytes = optional("new-topic-bytes", aWhole);
    if (place2 === null) return { missing: PLACE };
    if (name === null) return { missing: "a name: a letter or _, then letters, digits or _" };
    if (retain === null) return { missing: "keep for is " + DURATION2 };
    if (kept2 === null || kept2 === 0) return { missing: "keep at most is a whole number of bytes above zero" };
    if (bytes === null || bytes === 0) return { missing: "the largest message is a whole number of bytes" };
    return {
      statement: `DEFINE TOPIC ${name}` + (retain === void 0 ? "" : ` RETAIN ${retain}`) + (kept2 === void 0 ? "" : ` RETAIN BYTES ${kept2}`) + (bytes === void 0 ? "" : ` MAX BYTES ${bytes}`) + ";",
      says: `Creates ${name} in ${place2.namespace}.${place2.database}, keeping ` + (retain === void 0 ? "every message" : `each message for ${retain}`) + (kept2 === void 0 ? "" : `, removing the oldest once it holds over ${kept2} bytes`) + (bytes === void 0 ? "." : ` and refusing a message over ${bytes} bytes.`)
    };
  }
  function dropTopic() {
    const topic2 = chosen();
    if (where2() === null) return { missing: PLACE };
    if (topic2 === null) return { missing: TOPIC };
    if (trimmed("drop-topic-confirm") !== topic2.name) return { missing: `type ${topic2.name} to confirm` };
    return {
      statement: `DROP TOPIC ${topic2.name};`,
      says: `Removes ${topic2.name} with ${counted(held4(topic2), "message", "messages")} it holds, ${counted(topic2.readers.size, "reader position", "reader positions")} and ${counted(topic2.groups.size, "group", "groups")}. Those messages are gone for every reader.`
    };
  }
  function newGroup() {
    const topic2 = chosen();
    const group2 = aGroup(trimmed("new-group-name"));
    const deadline = aDuration(trimmed("new-group-deadline"));
    const width = optional("new-group-width", aWhole);
    const deliveries = optional("new-group-deliveries", aWhole);
    const dead = optional("new-group-dead", aName);
    if (where2() === null) return { missing: PLACE };
    if (topic2 === null) return { missing: TOPIC };
    if (group2 === null) return { missing: "a group name: letters, digits and _ . : -" };
    if (deadline === null) return { missing: "acknowledge within is " + DURATION2 };
    if (width === null || width === 0 || (width ?? 1) > MAX_IN_FLIGHT) {
      return { missing: `in flight is a whole number from 1 to ${MAX_IN_FLIGHT}` };
    }
    if (deliveries === null || deliveries === 0) return { missing: "give up after is a whole number of deliveries" };
    if (dead === null || dead === topic2.name) return { missing: "dead letters go to another topic, by name" };
    if (dead !== void 0 && deliveries === void 0) {
      return { missing: "dead letters need give up after: how many deliveries before a message goes there" };
    }
    return {
      statement: `DEFINE GROUP '${group2}' ON TOPIC ${topic2.name} ACK DEADLINE ${deadline}` + (deliveries === void 0 ? "" : ` DELIVERIES ${deliveries}`) + (width === void 0 ? "" : ` IN FLIGHT ${width}`) + (dead === void 0 ? "" : ` DEAD LETTER TO ${dead}`) + ";",
      says: `Creates group '${group2}' on ${topic2.name}. Each message goes to one member and comes back if it is left unacknowledged for ${deadline}; ${width ?? 1} at a time` + (deliveries === void 0 ? "" : `, given up after ${counted(deliveries, "delivery", "deliveries")}`) + (dead === void 0 ? "" : ` and then appended to ${dead}`) + "."
    };
  }
  function changeGroup() {
    const topic2 = chosen();
    const name = value("group-which");
    const group2 = topic2?.groups.get(name);
    const start = aWhole(trimmed("group-start"));
    if (where2() === null) return { missing: PLACE };
    if (topic2 === null) return { missing: TOPIC };
    if (group2 === void 0 || aGroup(name) === null) return { missing: "the topic has no group to change" };
    if (trimmed("group-confirm") !== name) return { missing: `type ${name} to confirm` };
    if (value("group-action") === "drop") {
      return {
        statement: `DROP GROUP '${name}' ON TOPIC ${topic2.name};`,
        says: `Removes group '${name}' and forgets its position and ${counted(group2.in_flight, "message", "messages")} it holds in flight.`
      };
    }
    if (start === null) return { missing: "a position from 0" };
    const moved = start < group2.position ? `It hands out again ${counted(group2.position - start, "message", "messages")} it already handed out.` : start > group2.position ? `It skips ${counted(start - group2.position, "message", "messages")}.` : "Its position stays where it is.";
    return {
      statement: `ALTER GROUP '${name}' ON TOPIC ${topic2.name} START AT ${start};`,
      says: `Group '${name}' next hands out position ${start + 1} and forgets ${counted(group2.in_flight, "message", "messages")} it holds in flight. ${moved}`
    };
  }
  var FORMS = [
    {
      button: "new-topic",
      says: "new-topic-says",
      status: "new-topic-status",
      compose: newTopic,
      fields: ["new-topic-name", "new-topic-retain", "new-topic-kept", "new-topic-bytes"]
    },
    {
      button: "drop-topic",
      says: "drop-topic-says",
      status: "drop-topic-status",
      compose: dropTopic,
      fields: ["drop-topic-confirm", "drop-topic-why"],
      why: "drop-topic-why"
    },
    {
      button: "new-group",
      says: "new-group-says",
      status: "new-group-status",
      compose: newGroup,
      fields: ["new-group-name", "new-group-deadline", "new-group-width", "new-group-deliveries", "new-group-dead"]
    },
    {
      button: "group-apply",
      says: "group-says",
      status: "group-status",
      compose: changeGroup,
      fields: ["group-which", "group-action", "group-start", "group-confirm", "group-why"],
      why: "group-why"
    }
  ];
  function shape3(form) {
    const composed = form.compose();
    disable(form.button, !("statement" in composed));
    write(form.says, "statement" in composed ? composed.says : composed.missing);
  }
  async function send2(form) {
    const composed = form.compose();
    const place2 = where2();
    if (!("statement" in composed) || place2 === null) {
      return;
    }
    disable(form.button, true);
    say(form.status, "sending…");
    try {
      const why = form.why === void 0 ? void 0 : trimmed(form.why) || void 0;
      await valueOf(tenancy(place2.namespace, place2.database) + composed.statement, SCREEN3, why);
      say(form.status, "done");
      for (const field of form.fields) {
        if (at(field) instanceof HTMLInputElement) {
          setValue(field, field === "group-start" ? "0" : "");
        }
      }
      await readTopics();
    } catch (failure) {
      const words4 = told(failure);
      say(form.status, failure instanceof Unreachable ? "the node did not answer — " + words4 : words4, true);
    }
    shape3(form);
  }
  function wire18() {
    for (const form of FORMS) {
      for (const field of form.fields) {
        at(field).addEventListener("input", () => shape3(form));
        at(field).addEventListener("change", () => shape3(form));
      }
      at(form.button).addEventListener("click", () => void send2(form));
    }
    whenChosen(() => {
      offer("group-which", [...chosen()?.groups.keys() ?? []].filter((name) => aGroup(name) !== null));
      for (const form of FORMS) {
        shape3(form);
      }
    });
    at("topics-namespace").addEventListener("change", () => FORMS.forEach(shape3));
    at("topics-database").addEventListener("change", () => FORMS.forEach(shape3));
  }

  // src/backup.ts
  //! The Backup screen: `BACKUP … TO '<name>'`, and `RESTORE SCRIPT FROM '<name>'`.
  //!
  //! A file name is the operator's text, so it reaches a statement only through
  //! `quoted()`, the one place the console escapes a string literal. A place a
  //! part names is grammar and cannot be quoted, so each one passes `aName()`
  //! first and a name that fails is refused here, before anything is sent. Whether
  //! a name stays inside the backup folder, and whether a restore may land, is the
  //! node's decision, and its refusal is shown in its own words.
  var SCREEN4 = "Backup";
  var WHAT = {
    state: {
      statement: "BACKUP STATE",
      suffix: "tessarisnap",
      says: "every live record at one moment; restores whole, and a pruned log does not stop it"
    },
    log: {
      statement: "BACKUP LOG",
      suffix: "tessarilog",
      says: "every commit in order; a restore can stop at any point in it"
    },
    script: {
      statement: "BACKUP SCRIPT",
      suffix: "tessariql",
      says: "statements that rebuild the store; readable, and its header lists what it leaves out"
    }
  };
  function chosenForm() {
    const picked = value("backup-form");
    return picked === "log" || picked === "script" ? picked : "state";
  }
  var partial = () => value("backup-part") === "places";
  function places() {
    const written = trimmed("backup-places").split(",").map((place2) => place2.trim()).filter((place2) => place2 !== "");
    if (written.length === 0) {
      return { missing: "name at least one namespace or database, such as crm or prod.orders" };
    }
    const named = [];
    for (const place2 of written) {
      const [namespace, database, extra] = place2.split(".");
      const within = aName(namespace ?? "");
      if (within === null || extra !== void 0) {
        return { missing: `${place2} is not a namespace or namespace.database` };
      }
      if (database === void 0) {
        named.push(`NAMESPACE ${within}`);
        continue;
      }
      const inner = aName(database);
      if (inner === null) {
        return { missing: `${place2} is not a namespace or namespace.database` };
      }
      named.push(`DATABASE ${within}.${inner}`);
    }
    return { of: named.join(", ") };
  }
  function suggested(form) {
    const stamp = (/* @__PURE__ */ new Date()).toISOString().replace(/[-:]/g, "").replace("T", "-").slice(0, 15);
    return `tessaridb-${stamp}.${WHAT[form].suffix}`;
  }
  function composeBackup() {
    const name = trimmed("backup-name");
    if (name === "") return { missing: "name the file to write" };
    const form = chosenForm();
    if (!partial()) {
      return {
        statement: `${WHAT[form].statement} TO ${quoted(name)};`,
        says: `Writes ${name} into the node's backup folder: ${WHAT[form].says}.`
      };
    }
    if (form === "log") {
      return { missing: "a log of a part restores nowhere; choose a snapshot or TessariQL" };
    }
    const part = places();
    if ("missing" in part) return part;
    if (form === "state") {
      if (part.of.includes(",")) {
        return { missing: "a snapshot is of one place; name one, or choose TessariQL for several" };
      }
      return {
        statement: `BACKUP STATE OF ${part.of} TO ${quoted(name)};`,
        says: `Writes ${name} into the node's backup folder: ${part.of} at one moment, with the store's users; it restores into an empty store.`
      };
    }
    return {
      statement: `BACKUP SCRIPT OF ${part.of} TO ${quoted(name)};`,
      says: `Writes ${name} into the node's backup folder: ${part.of} as statements, with the analyzers their fields use; users belong to the whole store and stay out of it.`
    };
  }
  function composeRestore() {
    const name = trimmed("restore-name");
    if (name === "") return { missing: "name a script in the backup folder" };
    return {
      statement: `RESTORE SCRIPT FROM ${quoted(name)};`,
      says: `Runs ${name} from the node's backup folder, creating the databases it carries.`
    };
  }
  var ours = true;
  function shape4() {
    const backup = composeBackup();
    disable("backup-run", !("statement" in backup));
    write("backup-says", "statement" in backup ? backup.says : backup.missing);
    disable("backup-places", !partial());
    const restore3 = composeRestore();
    disable("restore-run", !("statement" in restore3));
    write("restore-says", "statement" in restore3 ? restore3.says : restore3.missing);
  }
  async function send3(composed, button, status, answer2, shown3) {
    if (!("statement" in composed)) {
      return false;
    }
    disable(button, true);
    say(status, "working…");
    write(answer2, "");
    try {
      write(answer2, shown3(held2(await valueOf(composed.statement, SCREEN4))));
      say(status, "done");
      return true;
    } catch (failure) {
      const words4 = told(failure);
      say(status, failure instanceof Unreachable ? "the node did not answer — " + words4 : words4, true);
      return false;
    } finally {
      shape4();
    }
  }
  async function backUp() {
    const written = await send3(composeBackup(), "backup-run", "backup-status", "backup-answer", (answered2) => {
      const path = typeof answered2?.path === "string" ? answered2.path : trimmed("backup-name");
      const bytes = typeof answered2?.bytes === "number" ? answered2.bytes : null;
      return bytes === null ? path : `${path}
${bytes.toLocaleString("en")} bytes`;
    });
    if (written) {
      ours = true;
      setValue("backup-name", suggested(chosenForm()));
      shape4();
    }
  }
  async function restore2() {
    await send3(composeRestore(), "restore-run", "restore-status", "restore-answer", (answered2) => {
      const databases = Array.isArray(answered2?.databases) ? answered2.databases.filter((place2) => typeof place2 === "string") : [];
      const statements = typeof answered2?.statements === "number" ? answered2.statements : 0;
      return `${databases.join(", ") || "no database"} created · ${statements.toLocaleString("en")} statements`;
    });
  }
  function wire19() {
    setValue("backup-name", suggested(chosenForm()));
    for (const id of ["backup-form", "backup-part"]) {
      at(id).addEventListener("change", () => {
        if (ours) {
          setValue("backup-name", suggested(chosenForm()));
        }
        shape4();
      });
    }
    at("backup-name").addEventListener("input", () => {
      ours = false;
      shape4();
    });
    at("backup-places").addEventListener("input", shape4);
    at("restore-name").addEventListener("input", shape4);
    at("backup-run").addEventListener("click", () => void backUp());
    at("restore-run").addEventListener("click", () => void restore2());
    shape4();
  }

  // src/vault-records.ts
  //! The Vault screen's records: which vaults a database holds, a vault's record
  //! ids a page at a time, a reveal on click, a write of one field, and the audit
  //! trail — all through `POST /script`, as any client would send them.
  //!
  //! A namespace, database, vault, field and actor are grammar, so each passes
  //! `aName()` and is written into the text. A record id and a written value are
  //! values, so they are BOUND: the statement log keeps the script and never its
  //! parameters, which is what keeps a written secret out of it. Revealed values
  //! live in one element on this page and nowhere else — no storage, no log line —
  //! and are removed by Hide, by the next reveal, and by listing again.
  var SCREEN5 = "Vault";
  var PAGE = 50;
  var after = null;
  function place(needsVault) {
    const namespace = aName(trimmed("vault-rec-namespace"));
    const database = aName(trimmed("vault-rec-database"));
    if (namespace === null) return { missing: "name the namespace" };
    if (database === null) return { missing: "name the database" };
    const typed2 = trimmed("vault-rec-name");
    const vault = typed2 === "" ? null : aName(typed2);
    if (needsVault && vault === null) return { missing: "name the vault" };
    return { tenancy: tenancy(namespace, database), vault };
  }
  function literal(id) {
    if (typeof id === "string") return quoted(id);
    if (typeof id === "number" && Number.isSafeInteger(id)) return String(id);
    return null;
  }
  function failed(failure) {
    const words4 = told(failure);
    say("vault-rec-status", failure instanceof Unreachable ? "the node did not answer — " + words4 : words4, true);
  }
  function hideRevealed() {
    clear("vault-rec-shown");
    disable("vault-rec-hide", true);
  }
  async function listVaults() {
    const where3 = place(false);
    if ("missing" in where3) return say("vault-rec-status", where3.missing, true);
    clear("vault-rec-vaults");
    try {
      const report = held2(await valueOf(`${where3.tenancy}INFO FOR DATABASE;`, SCREEN5));
      const names = Array.isArray(report?.vaults) ? report.vaults : [];
      const list = made("ul");
      for (const each of names) {
        const name = typeof each === "string" ? each : each?.name;
        if (typeof name !== "string") continue;
        const pick2 = made("button", "quiet");
        pick2.type = "button";
        pick2.textContent = name;
        pick2.addEventListener("click", () => {
          setValue("vault-rec-name", name);
          void listRecords(true);
        });
        const item = made("li");
        item.appendChild(pick2);
        list.appendChild(item);
      }
      at("vault-rec-vaults").appendChild(list);
      say("vault-rec-status", names.length === 0 ? "this database holds no vault" : `${names.length} vault(s)`);
    } catch (failure) {
      failed(failure);
    }
  }
  async function listRecords(first) {
    const where3 = place(true);
    if ("missing" in where3 || where3.vault === null) {
      return say("vault-rec-status", "missing" in where3 ? where3.missing : "name the vault", true);
    }
    if (first) after = null;
    hideRevealed();
    const vault = where3.vault;
    const from = after === null ? "" : ` AFTER ${vault}:$after`;
    const bound = after === null ? void 0 : { after };
    try {
      const report = held2(
        await valueOf(`${where3.tenancy}INFO FOR VAULT ${vault} RECORDS${from} LIMIT ${PAGE};`, SCREEN5, void 0, bound)
      );
      const ids = Array.isArray(report?.records) ? report.records : [];
      clear("vault-rec-ids");
      const list = made("ul");
      for (const id of ids) {
        const item = made("li");
        const bindable = literal(id);
        const open2 = made("button", "quiet");
        open2.type = "button";
        open2.textContent = typeof id === "string" ? id : JSON.stringify(id);
        open2.disabled = bindable === null;
        if (bindable !== null) open2.addEventListener("click", () => void reveal(where3.tenancy, vault, bindable));
        item.appendChild(open2);
        list.appendChild(item);
      }
      at("vault-rec-ids").appendChild(list);
      const next = report?.next;
      after = next === void 0 || next === null ? null : literal(next);
      disable("vault-rec-more", after === null);
      say("vault-rec-status", ids.length === 0 ? "no records on this page" : `${ids.length} id(s) — choose one to reveal`);
    } catch (failure) {
      failed(failure);
    }
  }
  async function reveal(where3, vault, id) {
    hideRevealed();
    try {
      const opened = held2(await valueOf(`${where3}REVEAL * FROM ${vault}:$id;`, SCREEN5, void 0, { id }));
      at("vault-rec-shown").appendChild(shown(opened ?? {}));
      disable("vault-rec-hide", false);
      say("vault-rec-status", "revealed — the store recorded this read");
    } catch (failure) {
      failed(failure);
    }
  }
  async function writeField() {
    const where3 = place(true);
    const field = aName(trimmed("vault-rec-field"));
    const id = trimmed("vault-rec-id");
    if ("missing" in where3 || where3.vault === null) {
      return say("vault-rec-status", "missing" in where3 ? where3.missing : "name the vault", true);
    }
    if (field === null) return say("vault-rec-status", "name the field", true);
    if (id === "") return say("vault-rec-status", "name the record id", true);
    const secret = value("vault-rec-value");
    setValue("vault-rec-value", "");
    shape5();
    try {
      await valueOf(
        `${where3.tenancy}UPSERT ${where3.vault}:$id MERGE { '${field}': $value };`,
        SCREEN5,
        void 0,
        { id: quoted(id), value: quoted(secret) }
      );
      say("vault-rec-status", `wrote ${field} on ${id}`);
    } catch (failure) {
      failed(failure);
    }
  }
  async function auditTrail() {
    const where3 = place(false);
    if ("missing" in where3) return say("vault-rec-status", where3.missing, true);
    const typed2 = trimmed("vault-rec-actor");
    const actor = typed2 === "" ? null : aName(typed2);
    if (typed2 !== "" && actor === null) return say("vault-rec-status", `${typed2} is not a user name`, true);
    clear("vault-rec-trail");
    try {
      const by = actor === null ? "" : ` BY ${actor}`;
      const report = held2(await valueOf(`${where3.tenancy}INFO FOR AUDIT${by};`, SCREEN5));
      at("vault-rec-trail").appendChild(shown(report?.audit ?? []));
      say("vault-rec-status", "the audit trail, oldest first");
    } catch (failure) {
      failed(failure);
    }
  }
  function shape5() {
    disable(
      "vault-rec-write",
      trimmed("vault-rec-id") === "" || trimmed("vault-rec-field") === "" || value("vault-rec-value") === ""
    );
  }
  function wireRecords() {
    at("vault-rec-list").addEventListener("click", () => void listVaults());
    at("vault-rec-records").addEventListener("click", () => void listRecords(true));
    at("vault-rec-more").addEventListener("click", () => void listRecords(false));
    at("vault-rec-hide").addEventListener("click", hideRevealed);
    at("vault-rec-write").addEventListener("click", () => void writeField());
    at("vault-rec-audit").addEventListener("click", () => void auditTrail());
    for (const id of ["vault-rec-id", "vault-rec-field", "vault-rec-value"]) {
      at(id).addEventListener("input", shape5);
    }
    shape5();
  }

  // src/vault.ts
  //! The Vault screen: the store's key over `/vault`, and one vault's over
  //! `/vault/{namespace}/{database}/{vault}` (ADR-0092, ADR-0093).
  //!
  //! The passphrase travels as the body of its own route and never as a statement,
  //! so it is in no script, no statement log line and no saved form: the log
  //! records `METHOD path`, a passphrase field is `type="password"`, and each one is
  //! emptied as soon as its request has gone. A vault's three names are grammar
  //! in the path, so each passes `aName()` first and a name that fails is refused
  //! here, before anything is sent.
  var SCREEN6 = "Vault";
  function shown2(body) {
    if (typeof body !== "object" || body === null) return "";
    const answered2 = body;
    const state2 = typeof answered2.state === "string" ? answered2.state : "unknown";
    const lines = [state2];
    if (typeof answered2.seals_at === "string") {
      lines.push(`seals at ${new Date(answered2.seals_at).toLocaleString()}`);
    }
    if (typeof answered2.unseal_for === "string") lines.push(`an unseal lasts ${answered2.unseal_for}`);
    if (typeof answered2.custody === "string") {
      lines.push(answered2.custody === "own" ? "opens with its own passphrase" : "opens with the store's passphrase");
    }
    if (answered2.initialised === true) lines.push("this unseal set the store's first passphrase");
    return lines.join("\n");
  }
  function refusal(body) {
    if (typeof body === "object" && body !== null && typeof body.error === "string") {
      return body.error;
    }
    return typeof body === "string" && body !== "" ? body : "refused";
  }
  async function act(method, path, statusId, answerId, body) {
    say(statusId, "working…");
    try {
      const answered2 = await route(method, path, SCREEN6, body);
      if (answered2.status === 200) {
        write(answerId, shown2(answered2.body));
        say(statusId, "done");
      } else {
        const words4 = answered2.status === 429 ? "too many wrong passphrases — wait and try again" : refusal(answered2.body);
        say(statusId, words4, true);
      }
    } catch (failure) {
      const words4 = told(failure);
      say(statusId, failure instanceof Unreachable ? "the node did not answer — " + words4 : words4, true);
    }
  }
  function spent(id) {
    const held5 = value(id);
    setValue(id, "");
    return held5;
  }
  var change3 = (current, next) => JSON.stringify({ current, new: next });
  function onePath() {
    const names = [
      ["namespace", trimmed("vault-one-namespace")],
      ["database", trimmed("vault-one-database")],
      ["vault", trimmed("vault-one-name")]
    ];
    const checked = [];
    for (const [what, name] of names) {
      if (name === "") return { missing: `name the ${what}` };
      const held5 = aName(name);
      if (held5 === null) return { missing: `${name} is not a ${what} name` };
      checked.push(held5);
    }
    return { path: `/vault/${checked.join("/")}` };
  }
  function shape6() {
    disable("vault-store-unseal", value("vault-store-passphrase") === "");
    disable("vault-store-change", value("vault-store-current") === "" || value("vault-store-new") === "");
    const one2 = onePath();
    write("vault-one-says", "path" in one2 ? `Acts on ${one2.path}.` : one2.missing);
    const named = !("path" in one2);
    disable("vault-one-show", named);
    disable("vault-one-seal", named);
    disable("vault-one-unseal", named || value("vault-one-passphrase") === "");
    disable("vault-one-change", named || value("vault-one-current") === "" || value("vault-one-new") === "");
  }
  async function onOne(act_) {
    const one2 = onePath();
    if ("path" in one2) await act_(one2.path);
    shape6();
  }
  function wire20() {
    const store = (method, path, body) => act(method, path, "vault-store-status", "vault-store-answer", body).finally(shape6);
    const one2 = (method, path, body) => act(method, path, "vault-one-status", "vault-one-answer", body);
    at("vault-store-refresh").addEventListener("click", () => void store("GET", "/vault"));
    at("vault-store-seal").addEventListener("click", () => void store("POST", "/vault/seal"));
    at("vault-store-unseal").addEventListener(
      "click",
      () => void store("POST", "/vault/unseal", spent("vault-store-passphrase"))
    );
    at("vault-store-change").addEventListener(
      "click",
      () => void store("POST", "/vault/passphrase", change3(spent("vault-store-current"), spent("vault-store-new")))
    );
    at("vault-one-show").addEventListener("click", () => void onOne((path) => one2("GET", path)));
    at("vault-one-seal").addEventListener("click", () => void onOne((path) => one2("POST", `${path}/seal`)));
    at("vault-one-unseal").addEventListener(
      "click",
      () => void onOne((path) => one2("POST", `${path}/unseal`, spent("vault-one-passphrase")))
    );
    at("vault-one-change").addEventListener(
      "click",
      () => void onOne(
        (path) => one2("POST", `${path}/passphrase`, change3(spent("vault-one-current"), spent("vault-one-new")))
      )
    );
    for (const id of [
      "vault-store-passphrase",
      "vault-store-current",
      "vault-store-new",
      "vault-one-namespace",
      "vault-one-database",
      "vault-one-name",
      "vault-one-passphrase",
      "vault-one-current",
      "vault-one-new"
    ]) {
      at(id).addEventListener("input", shape6);
    }
    shape6();
    wireRecords();
  }

  // src/spaces-list.ts
  //! The Spaces pane's reading: which tables of a database are spaces, their keys by
  //! prefix, and one key's value.
  //!
  //! The spaces are found as Series finds series — each table's `INFO FOR TABLE`
  //! writes back the statement that made it. The keys and the value come from the
  //! `/kv` routes, so what the pane shows is what any HTTP caller of those routes
  //! would be answered.
  var SCREEN7 = "Run";
  var TABLES_ASKED = 200;
  var KEYS_SHOWN = 100;
  var words2 = (failure) => failure instanceof Unreachable ? "the node did not answer — " + told(failure) : told(failure);
  function strings2(answered2, field) {
    const list = held2(answered2)?.[field];
    return Array.isArray(list) ? list.filter((each) => typeof each === "string") : [];
  }
  function isSpace(result) {
    const fields = fieldsOf(result.value);
    const name = fields?.["table"];
    const definition2 = fields?.["definition"];
    return typeof name === "string" && typeof definition2 === "string" && definition2.startsWith("DEFINE SPACE ") ? name : null;
  }
  async function readNamespaces2() {
    state("kv-status", "waiting", "asking…");
    try {
      offer("kv-namespace", strings2(await valueOf("INFO FOR STORE;", SCREEN7), "namespaces"));
    } catch (failure) {
      state("kv-status", "wrong", words2(failure));
      return;
    }
    await readDatabases2();
  }
  async function readDatabases2() {
    const namespace = aName(value("kv-namespace"));
    if (namespace === null) {
      offer("kv-database", []);
      offer("kv-space", []);
      state("kv-status", "empty", "No namespace here — declare one with DEFINE NAMESPACE.");
      return;
    }
    try {
      const answered2 = await valueOf(`USE NAMESPACE ${namespace}; INFO FOR NAMESPACE;`, SCREEN7);
      offer("kv-database", strings2(answered2, "databases"));
    } catch (failure) {
      state("kv-status", "wrong", words2(failure));
      return;
    }
    await readSpaces();
  }
  async function readSpaces() {
    const namespace = aName(value("kv-namespace"));
    const database = aName(value("kv-database"));
    clear("kv-list");
    clear("kv-value");
    if (namespace === null || database === null) {
      offer("kv-space", []);
      state("kv-status", "empty", "Choose a namespace and a database that exist.");
      return;
    }
    const start = tenancy(namespace, database);
    state("kv-status", "waiting", "asking…");
    try {
      const tables = strings2(await valueOf(start + "INFO FOR DATABASE;", SCREEN7), "tables").filter((name) => aName(name) !== null).slice(0, TABLES_ASKED);
      let answered2 = [];
      if (tables.length > 0) {
        const { text } = await ask(start + tables.map((name) => `INFO FOR TABLE ${name};`).join(" "), SCREEN7);
        const body = JSON.parse(text);
        if (!Array.isArray(body.results)) {
          throw new Error(typeof body.error === "string" ? body.error : text);
        }
        answered2 = body.results;
      }
      const spaces = answered2.map(isSpace).filter((name) => name !== null);
      offer("kv-space", spaces);
      if (spaces.length === 0) {
        state("kv-status", "empty", `${namespace}.${database} holds no space — DEFINE SPACE declares one.`);
        return;
      }
    } catch (failure) {
      state("kv-status", "wrong", words2(failure));
      return;
    }
    await listKeys();
  }
  function base() {
    const namespace = aName(value("kv-namespace"));
    const database = aName(value("kv-database"));
    const space = aName(value("kv-space"));
    return namespace === null || database === null || space === null ? null : `/kv/${namespace}/${database}/${space}`;
  }
  var listing = 0;
  async function listKeys() {
    const mine2 = ++listing;
    const where3 = base();
    clear("kv-list");
    clear("kv-value");
    if (where3 === null) {
      state("kv-status", "empty", "Choose a space.");
      return;
    }
    const prefix = value("kv-prefix");
    const query = `?limit=${KEYS_SHOWN}` + (prefix === "" ? "" : `&prefix=${encodeURIComponent(prefix)}`);
    state("kv-status", "waiting", "asking…");
    try {
      const { status, body } = await route("GET", where3 + query, SCREEN7);
      if (mine2 !== listing) {
        return;
      }
      clear("kv-list");
      const keys = body.keys;
      if (status !== 200 || !Array.isArray(keys)) {
        const refused = body.error;
        throw new Error(typeof refused === "string" ? refused : `the node answered ${status}`);
      }
      drawKeys(keys.filter((each) => typeof each === "string"));
      if (keys.length === 0) {
        state("kv-status", "empty", prefix === "" ? "The space holds no key." : `No key starts with ${prefix}.`);
      } else if (keys.length >= KEYS_SHOWN) {
        state("kv-status", "partial", `the first ${KEYS_SHOWN} keys`);
      } else {
        settled("kv-status");
      }
    } catch (failure) {
      state("kv-status", "wrong", words2(failure));
    }
  }
  function drawKeys(keys) {
    const list = made("ul");
    for (const key of keys) {
      const item = made("li");
      const open2 = made("button");
      open2.className = "quiet";
      open2.textContent = key;
      open2.addEventListener("click", () => void readKey(key));
      item.appendChild(open2);
      list.appendChild(item);
    }
    at("kv-list").appendChild(list);
  }
  async function readKey(key) {
    const where3 = base();
    clear("kv-value");
    if (where3 === null) {
      return;
    }
    try {
      const { status, body } = await route("GET", `${where3}/key/${encodeURIComponent(key)}`, SCREEN7);
      const shown3 = made("pre");
      if (status === 404) {
        shown3.textContent = `${key}: no such key — it may have expired`;
      } else {
        const { value: held5, ttl } = body;
        const left = typeof ttl === "string" ? `expires in ${ttl}` : "never expires";
        shown3.textContent = `${key} — ${left}
${JSON.stringify(held5, null, 2)}`;
      }
      at("kv-value").appendChild(shown3);
    } catch (failure) {
      state("kv-status", "wrong", words2(failure));
    }
  }
  function wire21() {
    at("kv-namespace").addEventListener("change", () => void readDatabases2());
    at("kv-database").addEventListener("change", () => void readSpaces());
    at("kv-space").addEventListener("change", () => void listKeys());
    at("kv-list-them").addEventListener("click", () => void listKeys());
    at("kv-refresh").addEventListener("click", () => void readNamespaces2());
    onArrival(["run"], () => void readNamespaces2());
  }

  // src/series-list.ts
  //! The Series pane's reading: where, and which tables there are series.
  //!
  //! Everything here reads. A table's kind is not a field of `INFO FOR DATABASE`,
  //! so each table is asked for `INFO FOR TABLE`: a series writes back the
  //! `DEFINE SERIES` statement that made it, and a rollup says it is one.
  var SCREEN8 = "Run";
  var TABLES_ASKED2 = 200;
  var DECLARED = /^DEFINE SERIES \S+ RETAIN (\S+?)(?: TIME (\S+?))?;/;
  function kind(value2) {
    const fields = fieldsOf(value2);
    const name = fields?.["table"];
    if (typeof name !== "string") {
      return null;
    }
    const undefinable = fields?.["undefinable"];
    if (typeof undefinable === "string" && undefinable.includes("is a rollup")) {
      return { name, rollup: true, time: "window", retain: "set by its DEFINE ROLLUP" };
    }
    const definition2 = fields?.["definition"];
    const declared = typeof definition2 === "string" ? DECLARED.exec(definition2) : null;
    if (declared === null) {
      return null;
    }
    return { name, rollup: false, time: declared[2] ?? null, retain: declared[1] ?? "" };
  }
  var words3 = (failure) => failure instanceof Unreachable ? "the node did not answer — " + told(failure) : told(failure);
  function strings3(answered2, field) {
    const list = held2(answered2)?.[field];
    return Array.isArray(list) ? list.filter((each) => typeof each === "string") : [];
  }
  async function readNamespaces3() {
    state("series-status", "waiting", "asking…");
    try {
      offer("series-namespace", strings3(await valueOf("INFO FOR STORE;", SCREEN8), "namespaces"));
    } catch (failure) {
      state("series-status", "wrong", words3(failure));
      return;
    }
    await readDatabases3();
  }
  async function readDatabases3() {
    const namespace = aName(value("series-namespace"));
    if (namespace === null) {
      offer("series-database", []);
      draw6([]);
      state("series-status", "empty", "No namespace here — declare one with DEFINE NAMESPACE.");
      return;
    }
    try {
      const answered2 = await valueOf(`USE NAMESPACE ${namespace}; INFO FOR NAMESPACE;`, SCREEN8);
      offer("series-database", strings3(answered2, "databases"));
    } catch (failure) {
      state("series-status", "wrong", words3(failure));
      return;
    }
    await readSeries();
  }
  async function readSeries() {
    const namespace = aName(value("series-namespace"));
    const database = aName(value("series-database"));
    if (namespace === null || database === null) {
      draw6([]);
      state("series-status", "empty", "Choose a namespace and a database that exist.");
      return;
    }
    const start = tenancy(namespace, database);
    state("series-status", "waiting", "asking…");
    try {
      const tables = strings3(await valueOf(start + "INFO FOR DATABASE;", SCREEN8), "tables").filter((name) => aName(name) !== null);
      const asked2 = tables.slice(0, TABLES_ASKED2);
      let answered2 = [];
      if (asked2.length > 0) {
        const { text } = await ask(start + asked2.map((name) => `INFO FOR TABLE ${name};`).join(" "), SCREEN8);
        const body = JSON.parse(text);
        if (!Array.isArray(body.results)) {
          throw new Error(typeof body.error === "string" ? body.error : text);
        }
        answered2 = body.results;
      }
      const found = answered2.map((each) => kind(each.value)).filter((each) => each !== null);
      draw6(found);
      if (found.length === 0) {
        state("series-status", "empty", `${namespace}.${database} holds no series — DEFINE SERIES declares one.`);
      } else if (tables.length > asked2.length) {
        state("series-status", "partial", `asked about ${asked2.length} of ${tables.length} tables`);
      } else {
        settled("series-status");
      }
    } catch (failure) {
      draw6([]);
      state("series-status", "wrong", words3(failure));
    }
  }
  function draw6(found) {
    clear("series-list");
    if (found.length === 0) {
      return;
    }
    const table = made("table");
    const head = table.createTHead().insertRow();
    for (const name of ["table", "kind", "ordered by", "answers for"]) {
      const column = made("th");
      column.textContent = name;
      head.appendChild(column);
    }
    const body = table.createTBody();
    for (const each of found) {
      const line = body.insertRow();
      line.insertCell().textContent = each.name;
      line.insertCell().textContent = each.rollup ? "rollup" : "series";
      line.insertCell().textContent = each.time ?? "arrival";
      line.insertCell().textContent = each.retain;
    }
    at("series-list").appendChild(table);
  }
  function wire22() {
    at("series-namespace").addEventListener("change", () => void readDatabases3());
    at("series-database").addEventListener("change", () => void readSeries());
    at("series-refresh").addEventListener("click", () => void readNamespaces3());
    onArrival(["run"], () => void readNamespaces3());
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
    const held5 = everybody.length;
    const filtered = wanted() !== "";
    if (matched.length <= SHOWN) {
      return filtered ? `${matched.length} of ${held5}` : `${held5}`;
    }
    return `showing ${SHOWN} of ${matched.length}${filtered ? "" : ` — type a name to narrow`}`;
  }
  function listing2(rows2) {
    const table = made("table");
    const head = table.createTHead().insertRow();
    for (const column of ["user", "role", "reach"]) {
      const cell2 = made("th");
      cell2.textContent = column;
      head.appendChild(cell2);
    }
    const body = table.createTBody();
    for (const one2 of rows2) {
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
      const listed2 = answer2 === null ? [] : answer2["users"];
      everybody = Array.isArray(listed2) ? listed2 : [];
      forget();
      for (const one2 of everybody) {
        remember(one2.user ?? "", { role: said2(one2), reach: reach2(one2) });
      }
      redraw();
    } catch (failure) {
      everybody = [];
      forget();
      clear("user-list");
      write("user-count", "");
      state(
        "user-status",
        "wrong",
        failure instanceof Unreachable ? `${told(failure)} — nothing about this listing is settled until it does.` : `${told(failure)} — a listing is answered to whoever administers the tenancy.`
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
      at("user-list").appendChild(listing2(matched.slice(0, SHOWN)));
    } else {
      settled("user-status");
      at("user-list").appendChild(listing2(matched));
    }
    write("user-count", tally(matched));
  }
  function wire23() {
    at("list").addEventListener("click", listUsers);
    at("user-filter").addEventListener("input", redraw);
    onArrival(["access"], () => void listUsers());
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
      const statement2 = definition();
      if (statement2 === null) {
        say("define-status", missing(), true);
        return;
      }
      say("define-status", "running…");
      try {
        const answered2 = await valueOf(statement2, "Users · define");
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
      const statement2 = alteration();
      if (statement2 === null) {
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
        const answered2 = await valueOf(statement2, "Users · change", why);
        say("change-status", answered2 !== null && answered2.kind === "done" ? "ok" : "");
        await listUsers();
      } catch (failure) {
        say("change-status", told(failure), true);
      }
    });
    at("remove").addEventListener("click", async () => {
      const statement2 = removal2();
      if (statement2 === null) {
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
        const answered2 = await valueOf(statement2, "Users · remove", why);
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
  wire14();
  wire15();
  wire16();
  wire10();
  wire8();
  wire11();
  wire7();
  wire6();
  wire5();
  wire4();
  wire9();
  wire23();
  wire12();
  wire13();
  wire17();
  wire18();
  wire19();
  wire20();
  wire22();
  wire21();
})();
