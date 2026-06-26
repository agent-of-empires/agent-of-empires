import { useState } from "react";
import { LiveTerminalView } from "./LiveTerminalView";
import type { SessionResponse } from "../lib/types";

type ShellMode = "host" | "container";

/** Host/container shell switch plus the paired terminal. Used both in the
 *  desktop right-panel split and as the promoted single full-viewport mobile
 *  pane. Renders the capture-snapshot live view on every device (same
 *  architecture as the agent pane); the xterm.js PTY relay was removed. */
export function PairedShellPane({
  session,
  sessionId,
  terminalIndex = 0,
}: {
  session: SessionResponse | null;
  sessionId: string | null;
  /** Which paired-terminal instance this tab renders (#2437). 0 is the
   *  primary shell shared with the native TUI. */
  terminalIndex?: number;
}) {
  const [shellMode, setShellMode] = useState<ShellMode>("host");
  const isSandboxed = session?.is_sandboxed ?? false;
  // Container mode is only valid while the session is sandboxed; derive the
  // effective mode during render rather than resetting state in an effect, so
  // a session that loses its container falls back to host without a stale tab.
  const effectiveMode: ShellMode = isSandboxed ? shellMode : "host";

  const shellTabClass = (active: boolean) =>
    `text-[12px] px-2 py-0.5 rounded cursor-pointer transition-colors ${
      active ? "text-accent-500 bg-accent-500/10" : "text-text-dim hover:text-text-muted"
    }`;

  return (
    <div className="flex-1 flex flex-col min-h-0 overflow-hidden">
      <div className="flex items-center gap-1 px-2 py-1 bg-surface-900 border-b border-surface-700/20 shrink-0">
        <span className="text-xs text-text-dim mr-1">Shell</span>
        <button onClick={() => setShellMode("host")} className={shellTabClass(effectiveMode === "host")}>
          Host
        </button>
        {isSandboxed && (
          <button onClick={() => setShellMode("container")} className={shellTabClass(effectiveMode === "container")}>
            Container
          </button>
        )}
      </div>

      {!sessionId || !session ? (
        <div className="flex-1 flex items-center justify-center bg-surface-950 text-text-dim">
          <p className="text-xs">Select a session</p>
        </div>
      ) : (
        <LiveTerminalView
          key={`${sessionId}-${effectiveMode}-${terminalIndex}`}
          session={session}
          surface={effectiveMode === "container" ? "paired-container" : "paired-host"}
          terminalIndex={terminalIndex}
        />
      )}
    </div>
  );
}
