import { useEffect, useMemo, useState } from "react";
import { fetchAgents, switchTerminalAgent } from "../lib/api";
import type { AgentInfo } from "../lib/types";
import { reportError, reportInfo } from "../lib/toastBus";

type Props = {
  open: boolean;
  sessionId: string | null;
  currentTool: string | null;
  onClose: () => void;
};

export function SwitchTerminalAgentModal({ open, sessionId, currentTool, onClose }: Props) {
  const [agents, setAgents] = useState<AgentInfo[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [loadedKey, setLoadedKey] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const requestKey = open ? (currentTool ?? "") : null;

  useEffect(() => {
    if (!open || requestKey == null) return;
    let cancelled = false;
    void fetchAgents().then((next) => {
      if (cancelled) return;
      const available = next.filter((agent) => agent.installed || agent.kind === "custom");
      setAgents(available);
      setSelected(available.find((agent) => agent.name !== currentTool)?.name ?? null);
      setLoadedKey(requestKey);
    });
    return () => {
      cancelled = true;
    };
  }, [currentTool, open, requestKey]);

  const alternatives = useMemo(() => agents.filter((agent) => agent.name !== currentTool), [agents, currentTool]);
  const loading = open && loadedKey !== requestKey;

  if (!open) return null;

  const submit = async () => {
    if (!sessionId || !selected || submitting) return;
    setSubmitting(true);
    const result = await switchTerminalAgent(sessionId, selected);
    setSubmitting(false);
    if (!result) {
      reportError("Could not switch the terminal agent. The previous configuration remains recoverable.");
      return;
    }
    const handoff =
      result.context_handoff === "sent"
        ? "Previous terminal context was handed off."
        : "Previous terminal context was unavailable.";
    reportInfo(`Switched terminal agent to ${result.tool}. ${handoff}`);
    onClose();
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 px-4"
      role="presentation"
      onClick={(event) => {
        if (event.target === event.currentTarget && !submitting) onClose();
      }}
    >
      <div
        className="w-full max-w-md rounded-lg border border-surface-700 bg-surface-900 p-5 text-text-primary shadow-xl"
        role="dialog"
        aria-modal="true"
        aria-labelledby="switch-terminal-agent-title"
      >
        <h2 id="switch-terminal-agent-title" className="text-base font-semibold">
          Switch terminal agent
        </h2>
        <p className="mt-1 text-xs text-text-muted">
          The project, worktree, and AoE session stay the same. The target CLI starts without the old agent&apos;s
          resume id.
        </p>
        <div className="mt-4 space-y-2">
          {loading && <p className="text-sm text-text-muted">Loading installed agents…</p>}
          {!loading && alternatives.length === 0 && (
            <p className="text-sm text-text-muted">No other installed or configured agent was found.</p>
          )}
          {!loading &&
            alternatives.map((agent) => (
              <label
                key={agent.name}
                className={`flex cursor-pointer items-center gap-3 rounded border px-3 py-2 text-sm ${
                  selected === agent.name ? "border-brand-500 bg-brand-900/30" : "border-surface-700"
                }`}
              >
                <input
                  type="radio"
                  name="terminal-agent"
                  value={agent.name}
                  checked={selected === agent.name}
                  onChange={() => setSelected(agent.name)}
                  disabled={submitting}
                />
                <span>{agent.name}</span>
                <span className="ml-auto text-xs text-text-muted">{agent.binary}</span>
              </label>
            ))}
        </div>
        <div className="mt-5 flex justify-end gap-2">
          <button
            type="button"
            className="rounded border border-surface-700 px-3 py-1.5 text-sm text-text-secondary hover:bg-surface-800"
            onClick={onClose}
            disabled={submitting}
          >
            Cancel
          </button>
          <button
            type="button"
            className="rounded bg-brand-600 px-3 py-1.5 text-sm text-white hover:bg-brand-500 disabled:cursor-not-allowed disabled:opacity-50"
            onClick={() => void submit()}
            disabled={submitting || loading || selected == null}
          >
            {submitting ? "Switching…" : "Switch agent"}
          </button>
        </div>
      </div>
    </div>
  );
}
