//! Following a table as it changes.

import { WATCH_ROUTE } from "./api.js";
import { at, clear, disable, made, say, value } from "./dom.js";
import { token } from "./session.js";

/** The socket this page is holding open, or `null`. */
let following: WebSocket | null = null;

/**
 * Whether a follow is running right now.
 *
 * Asked on the way out, so a reload can say what was actually lost instead of
 * guessing from a field that happened to have text in it.
 */
export const isFollowing = (): boolean => following !== null;

/** What the node sends for one change, or instead of one. */
interface Change {
  readonly sequence?: number;
  readonly table?: string;
  readonly id?: string;
  readonly became?: string;
  readonly value?: unknown;
  readonly refused?: string;
  readonly error?: string;
}

/** What this page asks the node to follow. */
interface Asked {
  namespace: string;
  database: string;
  from: number;
  table?: string;
  token?: string;
  user?: string;
  password?: string;
}

function stop(words?: string): void {
  if (following !== null) {
    following.close();
    following = null;
  }
  disable("follow", false);
  disable("stop", true);
  if (words !== undefined) {
    say("watch-status", words);
  }
}

/** Add one change to the top of the list, as text and never as markup. */
function change(what: Change): void {
  const line = made("li");
  const became = typeof what.became === "string" ? what.became : "";
  line.classList.add(became === "removed" ? "removed" : "written");
  line.textContent =
    "#" +
    String(what.sequence) +
    "  " +
    String(what.table) +
    ":" +
    String(what.id) +
    "  " +
    became +
    (what.value === undefined ? "" : "  " + JSON.stringify(what.value));
  const list = at("changes");
  list.insertBefore(line, list.firstChild);
}

/** The socket's address: this page's own, with the scheme it was served over. */
function where(): URL {
  // Resolved against this page's own address rather than assembled from pieces:
  // same origin by construction, and it leaves the route a plain readable
  // literal instead of a fragment glued to a scheme. The scheme follows the
  // page's own, because a console served over TLS must not open a plaintext
  // socket.
  const address = new URL(WATCH_ROUTE, window.location.href);
  address.protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
  return address;
}

/** What authenticates this subscription, put into the message body. */
function asked(): Asked {
  const wanted: Asked = {
    namespace: value("namespace"),
    database: value("database"),
    from: Number(value("from")),
  };
  const table = value("table");
  if (table !== "") {
    wanted.table = table;
  }
  // A browser cannot set a header on a `WebSocket`, so whatever authenticates
  // this travels in the message. A token when there is one — it expires and can
  // be revoked, which a password does neither of, and it means following a
  // table does not put a password into a message body.
  const carried = token();
  if (carried !== null) {
    wanted.token = carried;
    return wanted;
  }
  // Both halves or neither: a name without a password would be asking to be
  // signed in without proof.
  const user = value("user");
  const password = value("password");
  if (user !== "" || password !== "") {
    wanted.user = user;
    wanted.password = password;
  }
  return wanted;
}

export function wire(): void {
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
      let what: Change;
      try {
        what = JSON.parse(String(event.data)) as Change;
      } catch {
        say("watch-status", "the node sent something this page cannot read", true);
        return;
      }
      // The node answers a request it will not serve in words rather than by
      // going quiet, so an operator can tell "not permitted" from "nothing has
      // happened yet".
      if (typeof what.refused === "string") {
        say("watch-status", what.refused, true);
        return;
      }
      if (typeof what.error === "string") {
        say("watch-status", what.error, true);
        return;
      }
      change(what);
    });

    socket.addEventListener("close", (event) => {
      // 1001 is this node stopping. The position is held by the client, so
      // following again from the last sequence seen resumes exactly there.
      stop(event.code === 1001 ? "the node is stopping" : "stopped");
    });

    socket.addEventListener("error", () => {
      say("watch-status", "the socket failed", true);
    });
  });

  at("stop").addEventListener("click", () => stop("stopped"));
}
