//! The console's icons: one outline family on a 24px grid — stroke 2, round
//! caps and joins, `currentColor` so they follow the theme and the state they
//! sit in. The same family as the S3 console's, so the two consoles read as one
//! product. Every icon here sits beside a text label, so each is hidden from
//! screen readers; an icon never carries meaning on its own.
//!
//! Drawn at run time rather than written into the page: the page's controls keep
//! their words as their first text, which is what the tests that read the page
//! measure, and an icon is decoration on top of that.

import { all, at } from "./dom.js";

/** A rounded rectangle as a path. */
const box = (x: number, y: number, w: number, h: number, r: number): string =>
  `M${x + r} ${y}h${w - 2 * r}a${r} ${r} 0 0 1 ${r} ${r}v${h - 2 * r}a${r} ${r} 0 0 1 -${r} ${r}h-${w - 2 * r}a${r} ${r} 0 0 1 -${r} -${r}v-${h - 2 * r}a${r} ${r} 0 0 1 ${r} -${r}z`;

/** A circle as a path. */
const ring = (cx: number, cy: number, r: number): string =>
  `M${cx - r} ${cy}a${r} ${r} 0 1 0 ${2 * r} 0a${r} ${r} 0 1 0 -${2 * r} 0`;

const SHAPES = {
  run: [box(3, 4, 18, 16, 2), "M7 9l3 3-3 3", "M12.5 15H17"],
  topics: ["M4 6h16", "M4 12h10", "M4 18h13", "M19 10.5l2 1.5-2 1.5"],
  cluster: [ring(6, 7, 2.5), ring(18, 7, 2.5), ring(12, 18, 2.5), "M8.5 7h7", "M7.3 9.2l3.4 6.6", "M16.7 9.2l-3.4 6.6"],
  access: [ring(8, 15, 4), "M10.8 12.2L20 3", "M16 7l3 3", "M18.5 4.5l2 2"],
  "this-node": [box(3, 4, 18, 7, 2), box(3, 13, 18, 7, 2), "M7 7.5h.01", "M7 16.5h.01"],
  backup: ["M4 7.5l8-4 8 4-8 4z", "M4 12l8 4 8-4", "M4 16.5l8 4 8-4"],
  vault: [box(4, 11, 16, 10, 2), "M8 11V7.5a4 4 0 0 1 8 0V11", "M12 15v2"],
  sun: [ring(12, 12, 4), "M12 2.5v2", "M12 19.5v2", "M2.5 12h2", "M19.5 12h2", "M5.3 5.3l1.4 1.4", "M17.3 17.3l1.4 1.4", "M5.3 18.7l1.4-1.4", "M17.3 6.7l1.4-1.4"],
  moon: ["M20 14.5A8.5 8.5 0 1 1 9.5 4a6.5 6.5 0 0 0 10.5 10.5z"],
  user: [ring(12, 8, 4), "M4.5 20a7.5 7.5 0 0 1 15 0"],
} as const;

export type IconName = keyof typeof SHAPES;

const isIcon = (name: string): name is IconName => Object.hasOwn(SHAPES, name);

/** The empty frame the page carries: an `<svg>` the HTML parser already placed in the SVG namespace. */
function frame(): SVGSVGElement {
  const template = at("icon-frame");
  const svg = template instanceof HTMLTemplateElement ? template.content.firstElementChild : null;
  if (!(svg instanceof SVGSVGElement)) {
    throw new Error("the page's #icon-frame holds no <svg>");
  }
  return svg;
}

/** The icon `name`, decorative: hidden from assistive technology by its frame. */
export function icon(name: IconName): SVGSVGElement {
  const blank = frame();
  const svg = blank.cloneNode(false);
  if (!(svg instanceof SVGSVGElement)) {
    throw new Error("cloning the icon frame did not give an <svg>");
  }
  for (const d of SHAPES[name]) {
    const path = document.createElementNS(blank.namespaceURI, "path");
    path.setAttribute("d", d);
    svg.append(path);
  }
  return svg;
}

/** Puts each destination's icon in front of its label, and the reader's in front of the identity. */
export function wire(): void {
  for (const item of all("[data-destination]")) {
    const name = item.dataset["destination"] ?? "";
    if (isIcon(name)) {
      item.prepend(icon(name));
    }
  }
  at("signed-in").before(icon("user"));
}
