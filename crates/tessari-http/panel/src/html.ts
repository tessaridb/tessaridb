//! Build-time HTML. Nothing here reaches the browser.
//!
//! The page is a value the build renders to a string, so the committed
//! `index.html` is still real markup with every element present — a reader with
//! JavaScript off sees the structure, and the tests that assert an id resolves
//! are asserting something about the file rather than about a runtime.
//!
//! This is deliberately not a virtual DOM. Nothing here diffs, reconciles or
//! re-renders; it builds a string once, at build time, and then the module is
//! gone. Runtime construction is `dom.ts`, which is a different problem.

export type Attrs = Record<string, string | number | boolean | null | undefined>;

/** Markup that is already markup — trusted, and never escaped again. */
export class Raw {
  constructor(readonly html: string) {}
}

export const raw = (html: string): Raw => new Raw(html);

export interface Node {
  readonly tag: string;
  readonly attrs: Attrs;
  readonly children: readonly Child[];
}

export type Child = Node | Raw | string | number | null | undefined | false;

const ESCAPES: Readonly<Record<string, string>> = {
  "&": "&amp;",
  "<": "&lt;",
  ">": "&gt;",
  '"': "&quot;",
};

/** Text into markup. Every string that is not `Raw` goes through this. */
export const escape = (text: string): string =>
  text.replace(/[&<>"]/g, (character) => ESCAPES[character] ?? character);

export function el(tag: string, attrs: Attrs = {}, ...children: Child[]): Node {
  return { tag, attrs, children };
}

/** Elements that close themselves and can hold nothing. */
const EMPTY = new Set(["meta", "link", "input", "br", "hr", "img", "source"]);

/**
 * Elements where every space is content. Their children are written with no
 * indentation at all, because a prettier file would be a different value.
 */
const LITERAL = new Set(["textarea", "pre", "script", "style"]);

/** Elements that sit inside a line of text rather than owning one. */
const INLINE = new Set([
  "span", "a", "code", "strong", "em", "b", "i", "small", "abbr", "kbd", "input", "br",
]);

const attributes = (attrs: Attrs): string => {
  const written: string[] = [];
  for (const [name, value] of Object.entries(attrs)) {
    if (value === undefined || value === null || value === false) continue;
    // A bare attribute — `hidden`, `reversed`, `disabled` — is present or it is
    // not. `hidden="false"` is hidden, which is the trap this avoids.
    if (value === true) {
      written.push(name);
      continue;
    }
    written.push(`${name}="${escape(String(value))}"`);
  }
  return written.length === 0 ? "" : ` ${written.join(" ")}`;
};

const present = (children: readonly Child[]): Child[] =>
  children.filter((child) => child !== null && child !== undefined && child !== false);

const isNode = (child: Child): child is Node =>
  typeof child === "object" && child !== null && "tag" in child;

/** Does this subtree belong on one line? */
const inline = (child: Child): boolean => {
  if (!isNode(child)) return true;
  if (!INLINE.has(child.tag)) return false;
  return present(child.children).every(inline);
};

const flat = (children: readonly Child[]): string =>
  present(children)
    .map((child) => {
      if (isNode(child)) return one(child, 0, true);
      if (child instanceof Raw) return child.html;
      return escape(String(child));
    })
    .join("");

function one(node: Node, depth: number, packed: boolean): string {
  const pad = packed ? "" : "  ".repeat(depth);
  const open = `${pad}<${node.tag}${attributes(node.attrs)}>`;
  if (EMPTY.has(node.tag)) return `${pad}<${node.tag}${attributes(node.attrs)} />`;

  const children = present(node.children);
  if (LITERAL.has(node.tag) || children.every(inline)) {
    return `${open}${flat(children)}</${node.tag}>`;
  }

  const inner = children
    .map((child) =>
      isNode(child)
        ? one(child, depth + 1, false)
        : `${"  ".repeat(depth + 1)}${child instanceof Raw ? child.html : escape(String(child))}`,
    )
    .join("\n");
  return `${open}\n${inner}\n${pad}</${node.tag}>`;
}

/** One page, as the bytes that get committed. */
export const document = (root: Node): string => `<!doctype html>\n${one(root, 0, false)}\n`;
