import type { KeyboardEvent } from "react";

/** Marks a header button or session row as a stop for the sidebar's arrow keys. */
export const NAV_ITEM = "data-sidebar-nav-item";
/** Marks the element that wraps one Group's rows; its header button points at it with `aria-controls`. */
export const ROWS = "data-sidebar-rows";

const SIDEBAR = '[role="navigation"][aria-label="Sessions sidebar"]';

/** A valid HTML id for the element wrapping `key`'s rows; group keys are paths and may hold spaces. */
export function rowsId(key: string): string {
  return `sidebar-rows-${encodeURIComponent(key)}`;
}

function navItems(sidebar: HTMLElement, within: ParentNode = sidebar): HTMLElement[] {
  return Array.from(within.querySelectorAll<HTMLElement>(`[${NAV_ITEM}]:not([aria-disabled="true"])`));
}

function headerOf(row: HTMLElement, sidebar: HTMLElement): HTMLElement | null {
  const rows = row.closest<HTMLElement>(`[${ROWS}]`);
  return rows ? sidebar.querySelector<HTMLElement>(`[aria-controls="${CSS.escape(rows.id)}"]`) : null;
}

/**
 * Up and Down move through visible header buttons and session rows in DOM order, stopping at the ends.
 * On a header, Left collapses, or moves to the parent header once collapsed; Right expands, or moves to the
 * first row once expanded. On a row, Left moves to its header. Modified keys, and keys from inside the item
 * (a rename input), are left alone.
 */
export function onSidebarArrowKey(e: KeyboardEvent<HTMLElement>): void {
  if (e.altKey || e.ctrlKey || e.metaKey || e.shiftKey || e.target !== e.currentTarget) return;
  if (!["ArrowDown", "ArrowUp", "ArrowLeft", "ArrowRight"].includes(e.key)) return;
  const item = e.currentTarget;
  const sidebar = item.closest<HTMLElement>(SIDEBAR);
  if (!sidebar) return;
  const expanded = item.getAttribute("aria-expanded");
  const isHeader = expanded !== null;
  let target: HTMLElement | null | undefined = null;

  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    const items = navItems(sidebar);
    const index = items.indexOf(item);
    if (index === -1) return;
    target = items[index + (e.key === "ArrowDown" ? 1 : -1)] ?? item;
  } else if (e.key === "ArrowLeft") {
    if (isHeader) {
      if (expanded === "true") item.click();
      else target = headerOf(item, sidebar);
    } else {
      target = headerOf(item, sidebar);
    }
  } else if (isHeader) {
    if (expanded === "false") {
      item.click();
    } else {
      const rows = document.getElementById(item.getAttribute("aria-controls") ?? "");
      target = rows ? navItems(sidebar, rows)[0] : null;
    }
  }
  e.preventDefault();
  target?.focus();
}

/**
 * Once `header` leaves the sidebar (its Group emptied, as after Archive all), move focus to the next
 * Group header, or the previous one at the end, unless focus has gone somewhere since.
 */
export function refocusNearbyWhenRemoved(header: HTMLElement, timeoutMs = 10_000): void {
  const sidebar = header.closest<HTMLElement>(SIDEBAR);
  if (!sidebar) return;
  const headers = navItems(sidebar).filter((el) => el.hasAttribute("aria-expanded"));
  const at = headers.indexOf(header);
  const rows = document.getElementById(header.getAttribute("aria-controls") ?? "");
  const next = headers.slice(at + 1).find((el) => !rows?.contains(el));
  const neighbours = [next, headers[at - 1]].filter((el): el is HTMLElement => !!el);
  const observer = new MutationObserver(() => {
    if (header.isConnected) return;
    observer.disconnect();
    clearTimeout(timer);
    const active = document.activeElement;
    if (active && active !== document.body) return;
    neighbours.find((el) => el.isConnected)?.focus();
  });
  observer.observe(sidebar, { childList: true, subtree: true });
  // Read only from the observer's callback, which can't run before this line.
  const timer = setTimeout(() => observer.disconnect(), timeoutMs);
}
