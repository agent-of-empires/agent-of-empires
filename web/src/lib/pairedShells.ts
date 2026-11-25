import type { ShellMode } from "../components/PairedTerminal";
import type { TerminalRef } from "./panes";
import type { SessionResponse } from "./types";

/** One paired shell a terminal tab can show: a Session's shell index in one mode. */
export interface PairedShell extends TerminalRef {
  mode: ShellMode;
  /** A terminal tab shows this shell now. */
  open: boolean;
}

export interface PairedShellGroup {
  session: SessionResponse;
  shells: PairedShell[];
}

/** Indices of `session`'s shells the Daemon sees alive, so a new tab can skip them. */
export function liveShellIndices(session: SessionResponse | undefined): number[] {
  return (session?.auxiliary ?? []).flatMap((o) =>
    o.state === "alive" && o.target.kind !== "tool" ? [o.target.index] : [],
  );
}

/**
 * Every live paired shell plus each shell an open tab shows, grouped by Session, `firstSessionId` first and the
 * rest in `sessions` order. `shownMode` is the mode a tab of that shell shows.
 */
export function pairedShellGroups(
  sessions: readonly SessionResponse[],
  openTabs: readonly TerminalRef[],
  shownMode: (session: SessionResponse, index: number) => ShellMode,
  firstSessionId: string | null,
): PairedShellGroup[] {
  const groups: PairedShellGroup[] = [];
  for (const session of sessions) {
    const tabIndices = new Set(openTabs.filter((t) => t.sessionId === session.id).map((t) => t.index));
    const seen = new Set<string>();
    const shells: PairedShell[] = [];
    const add = (index: number, mode: ShellMode) => {
      const key = `${index}:${mode}`;
      if (seen.has(key)) return;
      seen.add(key);
      shells.push({
        sessionId: session.id,
        index,
        mode,
        open: tabIndices.has(index) && shownMode(session, index) === mode,
      });
    };
    for (const o of session.auxiliary ?? []) {
      if (o.state !== "alive" || o.target.kind === "tool") continue;
      if (o.target.kind === "container" && !session.is_sandboxed) continue;
      add(o.target.index, o.target.kind);
    }
    for (const index of tabIndices) add(index, shownMode(session, index));
    if (shells.length === 0) continue;
    shells.sort((a, b) => a.index - b.index || (a.mode === b.mode ? 0 : a.mode === "host" ? -1 : 1));
    groups.push({ session, shells });
  }
  const first = groups.findIndex((g) => g.session.id === firstSessionId);
  if (first > 0) groups.unshift(...groups.splice(first, 1));
  return groups;
}
