/** Class recipe for a sidebar session row's active / multi-selected chrome.
 *  Split out of `components/WorkspaceSidebar.tsx` so the two states resolve in
 *  one place: both want a ring, and Tailwind would otherwise settle a
 *  simultaneous `ring-1` / `ring-2` by stylesheet order rather than by which
 *  state matters. */

/** Chrome for the open session's row and for multi-selection.
 *
 *  The open session gets a full inset frame in `session-active`, the accent
 *  lifted until it clears the WCAG non-text floor against every surface it can
 *  sit on (see `src/tui/styles/resolved.rs`). A background lift alone cannot
 *  carry it: the sidebar panel is already surface-800, so the old surface-850
 *  row sat at ~1.03:1 against it (#3912). Multi-selection keeps a thinner,
 *  translucent ring over a brand tint so a selected row still reads apart from
 *  the open one. Hover is withheld from the open row only, so it can never
 *  repaint over the frame. */
export function sessionRowChromeClass(isActive: boolean, isSelected: boolean): string {
  if (isActive) {
    return isSelected
      ? "ring-2 ring-inset ring-session-active bg-brand-500/15"
      : "ring-2 ring-inset ring-session-active";
  }
  return isSelected
    ? "ring-1 ring-inset ring-brand-500/60 bg-brand-500/10 hover:bg-surface-700/40"
    : "hover:bg-surface-700/40";
}
