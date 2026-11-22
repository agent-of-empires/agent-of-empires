import { useEffect, useEffectEvent, useState } from "react";

const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]):not([type="hidden"]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

/** A radio group is one stop: its checked radio, or its first one while none is checked. */
function radioStop(radio: HTMLInputElement, root: HTMLElement): boolean {
  if (radio.checked || !radio.name) return true;
  const group = Array.from(root.querySelectorAll<HTMLInputElement>('input[type="radio"]')).filter(
    (other) => other.name === radio.name,
  );
  return group[0] === radio && !group.some((other) => other.checked);
}

/** The Tab stops inside `root` in order. */
export function tabStops(root: HTMLElement): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
    (el) =>
      el.tabIndex >= 0 &&
      !el.closest("[hidden], [inert]") &&
      (el.checkVisibility?.({ visibilityProperty: true }) ?? true) &&
      !(el instanceof HTMLInputElement && el.type === "radio" && !radioStop(el, root)),
  );
}

/** Wraps a Tab at either end of `root`, and brings a Tab from outside it back in. */
function trapTab(e: KeyboardEvent, root: HTMLElement | null) {
  if (!root || root.closest("[inert]")) return;
  const holder = document.activeElement;
  // Another modal holding focus, such as a confirm over this one or the palette, owns the key.
  const overlay = holder?.closest('[aria-modal="true"]');
  if (overlay && overlay !== root && !overlay.contains(root)) return;
  // Focus outside every modal belongs to the top one, the last opened.
  const top = Array.from(document.querySelectorAll('[aria-modal="true"]')).at(-1);
  if (!root.contains(holder) && top && !top.contains(root) && !root.contains(top)) return;
  const stops = tabStops(root);
  const first = stops.at(0);
  const last = stops.at(-1);
  const wrapTo = !root.contains(holder)
    ? e.shiftKey
      ? last
      : first
    : e.shiftKey && (holder === first || holder === root)
      ? last
      : !e.shiftKey && holder === last
        ? first
        : undefined;
  if (wrapTo || stops.length === 0) {
    e.preventDefault();
    e.stopPropagation();
    wrapTo?.focus();
  }
}

/**
 * Keeps Tab and Shift+Tab inside a modal while `active`. With `restoreFocus`, closing
 * hands focus back to the element that held it at mount, unless something else has
 * taken it since. Escape and initial focus stay with the caller.
 */
export function useFocusTrap(
  root: () => HTMLElement | null,
  { active = true, restoreFocus = false }: { active?: boolean; restoreFocus?: boolean } = {},
) {
  const [opener] = useState(() => document.activeElement as HTMLElement | null);
  const currentRoot = useEffectEvent(root);
  useEffect(() => {
    if (!active) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Tab") trapTab(e, currentRoot());
    };
    // Capture phase, so a terminal or editor behind the modal never sees the key.
    document.addEventListener("keydown", onKey, true);
    return () => {
      document.removeEventListener("keydown", onKey, true);
      const lost = !document.activeElement || document.activeElement === document.body;
      if (restoreFocus && lost && opener?.isConnected) opener.focus();
    };
  }, [active, restoreFocus, opener]);
}
