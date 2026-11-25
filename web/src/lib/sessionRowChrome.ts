// Class recipe for a session row's active and selected chrome, resolved in one place so the states don't fight by stylesheet order.

const ACTIVE_BAR = "shadow-[inset_2px_0_0_0_var(--color-session-active)]";

/** The open row is a filled pill, as in Claude Code desktop, edged by a `session-active` bar that meets the
 *  WCAG non-text contrast floor; selection gets a thin translucent ring. Hover is withheld from the open row. */
export function sessionRowChromeClass(isActive: boolean, isSelected: boolean): string {
  if (isActive) {
    return isSelected
      ? `bg-surface-800 text-text-primary ${ACTIVE_BAR} ring-1 ring-inset ring-brand-500/60`
      : `bg-surface-800 text-text-primary ${ACTIVE_BAR}`;
  }
  return isSelected
    ? "ring-1 ring-inset ring-brand-500/60 bg-brand-500/10 hover:bg-surface-800/60"
    : "hover:bg-surface-800/60";
}
