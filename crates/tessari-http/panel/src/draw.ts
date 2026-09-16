//! Turning what the node said into something on screen.
//!
//! Every value drawn here arrives off the wire or out of the store, so every
//! one of them is written with `textContent`. A record that happens to hold a
//! `<script>` tag is data, not markup.

import type { Row } from "./api.js";
import { at, clear, made, shown } from "./dom.js";

/** Whether a value belongs in a cell: not an object, not an array. */
const flat = (value: unknown): boolean => value === null || typeof value !== "object";

/**
 * The fields every record shares, or `null` when they do not share a shape.
 *
 * The same rule the terminal follows, and for the same reason. A union of the
 * field sets with blanks where a record has none would table more answers and
 * would make *absent* and *empty* look identical — in the rendering, which is
 * the last place a distinction should be lost.
 */
function shape(records: readonly Row[]): string[] | null {
  let agreed: string[] | null = null;
  for (const record of records) {
    // Named `inside` rather than `held`, which is the name the session token
    // uses. Two top-level `held` bindings in one file are what took the panel
    // off the air for nineteen days; the module boundary makes it impossible
    // now, and the name is still not worth reusing here.
    const inside = record.value;
    if (inside === null || typeof inside !== "object" || Array.isArray(inside)) {
      return null;
    }
    const fields = inside as Record<string, unknown>;
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

/** One cell's text. Numbers stay numbers; everything else is JSON. */
const cell = (value: unknown): string =>
  typeof value === "string" ? value : JSON.stringify(value);

/** These records as a table element, or `null` if they are not one. */
export function drawn(records: readonly Row[]): HTMLTableElement | null {
  const fields = shape(records);
  if (fields === null) {
    return null;
  }
  const values = records.map((record) => record.value as Record<string, unknown>);
  const numeric = fields.map((field) =>
    values.every((value) => typeof value[field] === "number"),
  );

  const table = made("table");
  const head = table.createTHead().insertRow();
  for (const name of ["id", ...fields]) {
    const column = made("th");
    column.textContent = name;
    head.appendChild(column);
  }
  const body = table.createTBody();
  records.forEach((record, position) => {
    const row = body.insertRow();
    row.insertCell().textContent = record.id;
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

/** Draw a value into a pane: the JSON it is, and nothing cleverer. */
export function put(where: string, value: unknown): void {
  clear(where);
  at(where).appendChild(shown(value));
}

/** Draw a flat object as a two-column table of name and value. */
export function facts(where: string, held: Record<string, unknown>): void {
  clear(where);
  const table = made("table");
  const body = table.createTBody();
  for (const [name, value] of Object.entries(held)) {
    const row = body.insertRow();
    const label = made("th");
    label.textContent = name;
    row.appendChild(label);
    const box = row.insertCell();
    box.textContent = typeof value === "string" ? value : JSON.stringify(value);
    if (typeof value === "number") {
      box.classList.add("number");
    }
  }
  at(where).appendChild(table);
}
