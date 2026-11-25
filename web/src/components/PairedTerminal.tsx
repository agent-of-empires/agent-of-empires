import { LiveTerminalView } from "./LiveTerminalView";
import { effectiveShellMode } from "../lib/pairedShells";
import type { SessionResponse } from "../lib/types";

export type ShellMode = "host" | "container";

/** Host or Container for a sandboxed Session's shell; the caller renders it only for sandboxed Sessions. */
export function ShellModeToggle({
  mode,
  onModeChange,
  label,
}: {
  mode: ShellMode;
  onModeChange: (mode: ShellMode) => void;
  /** Names the shell, e.g. "Vikings terminal 2". */
  label: string;
}) {
  const option = (value: ShellMode, text: string) => (
    <button
      type="button"
      onClick={() => onModeChange(value)}
      aria-pressed={mode === value}
      data-testid={`shell-mode-${value}`}
      className={`h-5 px-1.5 rounded font-mono text-[11px] cursor-pointer transition-colors ${
        mode === value ? "bg-surface-700 text-text-secondary" : "text-text-dim hover:text-text-secondary"
      }`}
    >
      {text}
    </button>
  );
  return (
    <div role="group" aria-label={`${label} shell`} className="flex items-center gap-0.5">
      {option("host", "Host")}
      {option("container", "Container")}
    </div>
  );
}

/** Host/container shell shared by desktop docks and the mobile paired view. */
export function PairedShellPane({
  readOnly,
  session,
  active = true,
  terminalIndex = 0,
  mode = "host",
}: {
  readOnly: boolean;
  session: SessionResponse | null;
  active?: boolean;
  /** Which paired-terminal instance this tab renders (#2437). 0 is the
   *  primary shell shared with the native TUI. */
  terminalIndex?: number;
  /** The owner's chosen mode; {@link ShellModeToggle} changes it. */
  mode?: ShellMode;
}) {
  const effectiveMode = effectiveShellMode(session, mode);

  return (
    <div className="flex-1 flex flex-col min-h-0 overflow-hidden">
      {!session ? (
        <div className="flex-1 flex items-center justify-center bg-surface-950 text-text-dim">
          <p className="text-xs">Select a session</p>
        </div>
      ) : (
        <LiveTerminalView
          key={`${session.id}-${effectiveMode}-${terminalIndex}`}
          session={session}
          readOnly={readOnly}
          active={active}
          surface={effectiveMode === "container" ? "paired-container" : "paired-host"}
          terminalIndex={terminalIndex}
        />
      )}
    </div>
  );
}
