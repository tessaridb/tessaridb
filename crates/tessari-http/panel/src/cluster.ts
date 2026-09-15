//! Cluster — what this node knows about the cluster it belongs to.
//!
//! Extracted from `page.ts` for the reason `access.ts` was. It is the screen S4
//! rebuilds into a map, so it is the one most worth having on its own already.

import { el, type Node } from "./html.js";
import { to } from "./destinations.js";
import {
  answer, behindDisclosure, button, field, note, pane, paneHead, panel, row,
  says, status, text, warning,
} from "./ui.js";

export const cluster = (): Node =>
  panel(
    to("cluster"),
    false,
    pane(
      paneHead("The cluster, as this node sees it", status("cluster-status")),
      // The map, and the raw answer under it. Both, because the map is a
      // reading of the answer and an operator who doubts the reading needs the
      // thing it was read from — that is the same rule the statement log
      // follows for statements.
      el("div", { id: "cluster-map", class: "map" }),
      // The drawer sits inside the pane and over the map, so opening it never
      // costs the view the decision is being made against.
      el(
        "div",
        { id: "drawer", class: "sheet drawer", hidden: true },
        row(
          "spread",
          el("h3", { id: "drawer-title" }, "a node"),
          button("drawer-close", "Close", "quiet"),
        ),
        el("div", { id: "drawer-missing" }),
        row(
          "default",
          ...["serving", "writable", "coordinating"].map((bit) =>
            field(bit, el("input", { id: `drawer-${bit}`, type: "checkbox" }), { class: "tick" }),
          ),
        ),
        says("drawer-says"),
        row("default", button("drawer-apply", "Declare it", "primary"), status("drawer-status")),
        note(
          "Draining this node and handing leadership over are the other two things " +
            "you would come here for, and neither has a statement behind it yet — so " +
            "this drawer does not offer a control that would compose nothing.",
        ),
      ),
      behindDisclosure("The answer this was drawn from", answer("cluster-facts", "small")),
    ),
    pane(
      paneHead("Declare the membership", status("form-status")),
      note(
        "Every peer at once, in one transaction. Declaring them one at a time " +
          "strands you: the first declaration makes this node clustered, which " +
          "costs it the authority to accept the second. All of them, or none.",
      ),
      ...Array.from({ length: 5 }, (_, at) =>
        row(
          "default",
          field("Name", text(`peer-${at}-name`, { placeholder: "warsaw" })),
          field("Address", text(`peer-${at}-endpoint`, { placeholder: "10.0.0.2:9000" })),
          field("Node id", text(`peer-${at}-node`, { placeholder: "9f2c4e1a-…" })),
          ...["serving", "writable", "coordinating"].map((bit) =>
            field(
              bit,
              el("input", { id: `peer-${at}-${bit}`, type: "checkbox" }),
              { class: "tick" },
            ),
          ),
        ),
      ),
      says("form-says"),
      behindDisclosure(
        "The transaction this will send",
        el("div", { id: "form-statement", class: "answer small" }),
      ),
      row("default", button("form-cluster", "Declare it", "primary")),
    ),
    pane(
      paneHead("What is here, and what is not"),
      note(
        "What the pane above shows is what this node itself knows: its own " +
          "roles, the peers it has been told about, and the addresses it answers " +
          "on. It is read from the node, not from anything standing beside it.",
      ),
      // 141 words of explanation, moved behind a disclosure rather than deleted.
      // The brief's rule is that prose paragraphs are not a UI element and that
      // explanation lives behind a disclosure, in the docs, or nowhere — and an
      // operator on their fiftieth visit is not reading this, while somebody on
      // their first may want it.
      behindDisclosure(
        "How membership works",
        note(
          "Membership is a record and not a control plane: a node learns its peers " +
            "through the same log it replicates data with, so there is nothing to stand " +
            "up beside the database. A namespace declares how many copies the cluster " +
            "keeps, and a peer's subscription decides which of them it receives. A " +
            "leader holds a renewable lease granted by a majority and stops writing " +
            "before that lease expires, measured on elapsed time rather than on the " +
            "clock. A read may name the staleness it will accept, and a bound tighter " +
            "than the cluster can know about itself is refused with the floor named. A " +
            "write sent to the wrong node comes back as a redirect carrying the " +
            "endpoint, the node and the epoch it was decided under. A divergence is " +
            "refused at the first divergent record and counted, never silently ranked.",
        ),
      ),
      warning(
        el("strong", {}, "There is no sharding."),
        " Every node that holds a namespace holds all of it; one dataset is not " +
          "split across machines by key. A namespace larger than one machine is the " +
          "case this engine does not serve.",
      ),
      note(el("strong", {}, "No lag figure, and no leadership for other nodes.")),
      behindDisclosure(
        "Why neither is drawn",
        note(
          "Nothing pulls a replica forward on a timer, so a node that is not writing " +
            "has no last collection its copy could be measured from; and this node " +
            "knows which lease it holds, never which lease somebody else holds. A " +
            "number invented for either would be the dashboard drawn ahead of the " +
            "engine, which is what makes the rest of a console untrustworthy.",
        ),
      ),
    ),
  );
