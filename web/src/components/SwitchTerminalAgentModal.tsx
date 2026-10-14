import { useEffect, useRef, useState } from "react";
import { fetchAgents, switchTerminalAgent } from "../lib/api";
import type { AgentInfo } from "../lib/types";
import { reportError, reportInfo } from "../lib/toastBus";

/** Restarts a terminal/tmux session on another agent CLI. Unlike the structured
 * view picker the AoE session, project path and worktree all stay put: only the
 * pane's CLI changes, and it starts a fresh native session instead of resuming
 * the outgoing tool's conversation. */
interface Props {
  open: boolean;
  sessionId: string | null;
  currentTool: string | null;
  onClose: () => void;
}

export function SwitchTerminalAgentModal({ open, sessionId, currentTool, onClose }: Props) {
  const [agents, setAgents] = useState<AgentInfo[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const confirmRef = useRef<HTMLButtonElement>(null);
  const previousFocusRef = useRef<HTMLElement | null>(null);

  // Reset during render; the key also tracks closed so a same-tool reopen refetches.
  const [depKey, setDepKey] = useState(() => `${open}-${sessionId}-${currentTool}`);
  const currentKey = `${open}-${sessionId}-${currentTool}`;
  if (currentKey !== depKey) {
    setDepKey(currentKey);
    if (open) setLoading(true);
  }

  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    // What this machine can actually launch, plus custom entries the user wired up.
    void fetchAgents().then((list) => {
      if (cancelled) return;
      const available = list.filter((a) => a.installed || a.kind === "custom");
      setAgents(available);
      setSelected(available.find((a) => a.name !== currentTool)?.name ?? null);
      setLoading(false);
    });
    return () => {
      cancelled = true;
    };
  }, [open, sessionId, currentTool, depKey]);

  // No dismissal mid-switch, so a half-done relaunch can finish.
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !submitting) onClose();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [open, submitting, onClose]);

  useEffect(() => {
    if (!open) return;
    previousFocusRef.current = document.activeElement as HTMLElement | null;
    requestAnimationFrame(() => confirmRef.current?.focus());
    return () => {
      previousFocusRef.current?.focus?.();
      previousFocusRef.current = null;
    };
  }, [open]);

  if (!open) return null;

  const handleConfirm = async () => {
    if (!sessionId || !selected) return;
    setSubmitting(true);
    try {
      const reply = await switchTerminalAgent(sessionId, selected);
      if (reply.ok) {
        reportInfo(
          `Switched terminal agent to ${reply.result.tool}. ${
            reply.result.context_handoff === "sent"
              ? "Previous terminal context was handed off."
              : "Previous terminal context was unavailable."
          }`,
        );
        onClose();
        return;
      }
      // A committed swap already moved the row to the target tool; keeping the
      // dialog open would only offer a target the row now refuses as `same_tool`.
      reportError(reply.message ?? "Could not switch the terminal agent.");
      if (reply.committed) onClose();
    } finally {
      setSubmitting(false);
    }
  };

  const alternatives = agents.filter((a) => a.name !== currentTool);

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="switch-terminal-agent-title"
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 px-4"
      onClick={(e) => {
        if (e.target === e.currentTarget && !submitting) onClose();
      }}
    >
      <div className="w-full max-w-md rounded-lg border border-surface-700 bg-surface-900 p-5 shadow-xl text-text-primary">
        <h2 id="switch-terminal-agent-title" className="text-base font-semibold">
          Switch terminal agent
        </h2>
        <p className="mt-1 text-xs text-text-muted">
          Restart this session on a different agent CLI. The project, worktree and AoE session stay the same, and the
          new agent starts a fresh conversation rather than resuming the outgoing one.
        </p>

        {loading ? (
          <div className="mt-4 text-xs text-text-muted">Loading agents...</div>
        ) : alternatives.length === 0 ? (
          <div className="mt-4 text-xs text-status-error">
            No other installed agent was found. Install one and try again.
          </div>
        ) : (
          <ul className="mt-4 max-h-64 space-y-1 overflow-y-auto">
            {alternatives.map((a) => (
              <li key={a.name}>
                <label
                  className={`flex items-center gap-3 rounded border px-3 py-2 transition-colors cursor-pointer ${
                    selected === a.name ? "border-brand-500 bg-brand-900/30" : "border-surface-700 hover:bg-surface-800"
                  }`}
                >
                  <input
                    type="radio"
                    name="terminal-agent-target"
                    value={a.name}
                    checked={selected === a.name}
                    onChange={() => setSelected(a.name)}
                    disabled={submitting}
                  />
                  <span className="flex-1 text-sm font-mono">{a.name}</span>
                  <span className="font-mono text-xs text-text-muted">{a.binary}</span>
                </label>
              </li>
            ))}
          </ul>
        )}

        <div className="mt-5 flex justify-end gap-2">
          <button
            type="button"
            onClick={() => {
              if (!submitting) onClose();
            }}
            disabled={submitting}
            className="rounded border border-surface-700 px-3 py-1 text-xs font-medium hover:bg-surface-800 disabled:cursor-not-allowed disabled:opacity-60"
          >
            Cancel
          </button>
          <button
            ref={confirmRef}
            type="button"
            onClick={handleConfirm}
            disabled={!selected || submitting || alternatives.length === 0}
            className="rounded border border-brand-700 bg-brand-900/40 px-3 py-1 text-xs font-medium text-brand-100 hover:bg-brand-900/60 disabled:cursor-not-allowed disabled:opacity-60"
          >
            {submitting ? "Switching..." : "Switch agent"}
          </button>
        </div>
      </div>
    </div>
  );
}
