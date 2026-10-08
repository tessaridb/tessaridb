//! Light or dark. Until the operator chooses, the page follows the system's
//! preference; a choice is remembered in this browser only. It is a viewer's
//! convenience and not console state — it names no account, no namespace and no
//! statement, which is the line `context.ts` holds for everything it keeps — and
//! a browser that refuses storage simply forgets it.

import { at } from "./dom.js";
import { icon } from "./icons.js";

type Theme = "light" | "dark";

const KEY = "tessaridb-console-theme";
const darkQuery = matchMedia("(prefers-color-scheme: dark)");

function remembered(): Theme | null {
  try {
    const value = localStorage.getItem(KEY);
    return value === "light" || value === "dark" ? value : null;
  } catch {
    return null;
  }
}

function remember(theme: Theme): void {
  try {
    localStorage.setItem(KEY, theme);
  } catch {
    // Storage refused: the choice lasts for this page only.
  }
}

const current = (): Theme => remembered() ?? (darkQuery.matches ? "dark" : "light");

/** Applies the remembered or system theme and makes the switch flip between the two. */
export function wire(): void {
  const button = at("theme");
  const show = (): void => {
    const theme = current();
    if (remembered() === null) {
      document.documentElement.removeAttribute("data-theme");
    } else {
      document.documentElement.setAttribute("data-theme", theme);
    }
    // The words in their own element, so a narrow screen can keep them for a
    // screen reader while showing only the icon.
    const label = document.createElement("span");
    label.className = "label";
    label.textContent = theme === "dark" ? "Light theme" : "Dark theme";
    button.replaceChildren(icon(theme === "dark" ? "sun" : "moon"), label);
  };
  button.addEventListener("click", () => {
    remember(current() === "dark" ? "light" : "dark");
    show();
  });
  darkQuery.addEventListener("change", show);
  show();
}
