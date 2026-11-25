import type { KeyboardEvent } from "react";

/** Arrow, Home and End keys move focus among a menu's items, wrapping at either end. */
export function moveMenuFocus(e: KeyboardEvent<HTMLElement>) {
  const items = [...e.currentTarget.querySelectorAll<HTMLElement>('[role="menuitem"], [role="menuitemcheckbox"]')];
  const at = items.indexOf(document.activeElement as HTMLElement);
  const next =
    e.key === "ArrowDown"
      ? (at + 1) % items.length
      : e.key === "ArrowUp"
        ? (at - 1 + items.length) % items.length
        : e.key === "Home"
          ? 0
          : e.key === "End"
            ? items.length - 1
            : null;
  if (next === null || items.length === 0) return;
  e.preventDefault();
  items[next]!.focus();
}
