// Class recipe for a session row's active and selected chrome, resolved in one place so the states don't fight by stylesheet order.

/** The open row is a soft filled pill with brighter text, as in Claude Code desktop; `aria-current` names it to
 *  assistive tech. Selection gets a thin translucent ring. Hover is withheld from the open row. */
export function sessionRowChromeClass(isActive: boolean, isSelected: boolean): string {
  if (isActive) {
    return isSelected
      ? "bg-surface-700/70 text-text-primary ring-1 ring-inset ring-brand-500/60"
      : "bg-surface-700/70 text-text-primary";
  }
  return isSelected
    ? "ring-1 ring-inset ring-brand-500/60 bg-brand-500/10 hover:bg-surface-800/60"
    : "hover:bg-surface-800/60";
}
