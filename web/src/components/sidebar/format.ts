/** Mirrors the TUI presets in `src/tui/dialogs/snooze_duration.rs`. */
export const SNOOZE_PRESETS: readonly { label: string; minutes: number }[] = [
  { label: "1 hour", minutes: 60 },
  { label: "2 hours", minutes: 120 },
  { label: "3 hours", minutes: 180 },
  { label: "4 hours", minutes: 240 },
  { label: "5 hours", minutes: 300 },
  { label: "6 hours", minutes: 360 },
  { label: "1 day", minutes: 1440 },
  { label: "1 week", minutes: 10080 },
];

/** Server bound from `SNOOZE_MAX_MINUTES` in `src/session/config/mod.rs`. */
export const SNOOZE_MAX_MINUTES = 30 * 24 * 60;

/** `45s`, `3m`, `1h 7m`: sub-minute resolution is dropped above one minute. */
export function formatDurationSecondsShort(seconds: number): string {
  if (seconds < 60) return `${seconds}s`;
  const m = Math.floor(seconds / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  const remM = m % 60;
  return remM === 0 ? `${h}h` : `${h}h ${remM}m`;
}

/** Static remaining-time label for the snooze chip; computed per render, no ticker. */
export function formatSnoozeRemainingShort(snoozedUntilIso: string): string {
  const target = Date.parse(snoozedUntilIso);
  if (!Number.isFinite(target)) return "snoozed";
  const remainingMs = target - Date.now();
  if (remainingMs <= 0) return "soon";
  const minutes = Math.floor(remainingMs / 60_000);
  if (minutes < 1) return "<1m";
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h`;
  return `${Math.floor(hours / 24)}d`;
}
