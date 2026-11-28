// Two frames let the keypress settle and a phone's drawer start opening. The drawer cannot take focus until its
// opening render commits, and how many frames that takes varies, so a focus that did not land is retried.
const SETTLE_FRAMES = 2;
const RETRY_FRAMES = 10;

/** Moves focus to the active session row, or to the sidebar when no row is shown. */
export function focusSessionSidebar(): void {
  let retries = 0;
  const attempt = () => {
    const sidebar = document.querySelector<HTMLElement>('[role="navigation"][aria-label="Sessions sidebar"]');
    const activeRow = sidebar?.querySelector<HTMLElement>("[data-active-session-row]");
    const target = activeRow && activeRow.getClientRects().length > 0 ? activeRow : sidebar;
    target?.focus();
    if (target && document.activeElement === target) return;
    if (++retries <= RETRY_FRAMES) requestAnimationFrame(attempt);
  };
  const settle = (frames: number) => requestAnimationFrame(frames > 1 ? () => settle(frames - 1) : attempt);
  settle(SETTLE_FRAMES);
}
