/** Which "worker stopped" banner explains why the worker went down. A startup
 *  error owns the chrome with its own retry path, so it suppresses all of them. */
export type WorkerStoppedVariant = "none" | "trashed" | "archived" | "snoozed" | "generic";

export function pickWorkerStoppedVariant(args: {
  workerStopped: boolean;
  startupError: string | null;
  trashedAt: string | null;
  archivedAt: string | null;
  snoozedUntil: string | null;
  /** While the daemon proves the runner dead, the stopping banner replaces the generic one. */
  workerStopping?: boolean;
}): WorkerStoppedVariant {
  if (!args.workerStopped) return "none";
  if (args.startupError) return "none";
  // Trash wins: a trashed session is recoverable only by restoring it.
  if (args.trashedAt) return "trashed";
  if (args.archivedAt) return "archived";
  if (args.snoozedUntil) return "snoozed";
  return args.workerStopping ? "none" : "generic";
}

export function showWorkerStoppingBanner(args: { acpWorkerState: string; startupError: string | null }): boolean {
  return args.acpWorkerState === "stopping" && !args.startupError;
}
