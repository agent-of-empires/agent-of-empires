/** Which "worker stopped" banner variant to render in the structured view
 *  view, given the session's triage state. The variant matches the
 *  reason the worker was torn down so the user sees a banner that
 *  actually explains their situation (and offers the right next
 *  step) instead of the generic `aoe acp stop` message. See
 *  #1581.
 *
 *  Returns:
 *   - `"none"`     : worker is not stopped, no banner.
 *   - `"trashed"`  : the session is in the trash (#2489); reconnect
 *                    is not the right next step (the user must restore
 *                    it first). Takes precedence over archived/snoozed.
 *   - `"archived"` : worker was torn down by the sidebar archive
 *                    action; reconnect is not the right next step
 *                    (the user must unarchive first).
 *   - `"snoozed"`  : worker was torn down by the sidebar snooze
 *                    action; the reconciler will respawn it when
 *                    the snooze expires.
 *   - `"generic"`  : everything else (`aoe acp stop`, manual
 *                    teardown, etc.).
 *
 *  `startupError` takes precedence over every "stopped" banner
 *  variant because the startup-error banner has its own retry path;
 *  callers should bail before invoking this helper when a startup
 *  error is in flight, but we still defensively return `"none"` to
 *  stay safe under refactors. */
export type WorkerStoppedVariant = "none" | "trashed" | "archived" | "snoozed" | "generic";

export function pickWorkerStoppedVariant(args: {
  workerStopped: boolean;
  startupError: string | null;
  trashedAt: string | null;
  archivedAt: string | null;
  snoozedUntil: string | null;
}): WorkerStoppedVariant {
  if (!args.workerStopped) return "none";
  if (args.startupError) return "none";
  // Trash supersedes archive/snooze: a trashed session is recoverable only
  // by restoring it, so its banner must win even if archived_at is also set.
  if (args.trashedAt) return "trashed";
  if (args.archivedAt) return "archived";
  if (args.snoozedUntil) return "snoozed";
  return "generic";
}
