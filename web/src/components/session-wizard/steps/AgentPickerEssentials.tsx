import type { AgentInfo } from "../../../lib/types";

interface WizardData {
  tool: string;
  [key: string]: unknown;
}

interface Props {
  data: WizardData;
  onChange: (field: string, value: unknown) => void;
  agents: AgentInfo[];
}

/** Always-visible essentials of the agent section: just the agent picker
 *  grid. The structured-view choice lives in `AgentOptions` under the More
 *  options fold (#2210). */
export function AgentPickerEssentials({ data, onChange, agents }: Props) {
  const selectableAgents = agents.filter((agent) => agent.kind === "custom" || agent.installed);

  return (
    <div>
      {/* No agents installed */}
      {selectableAgents.length === 0 && agents.length > 0 && (
        <div className="mb-5 p-4 rounded-lg border border-status-warning/30 bg-status-warning/5">
          <p className="text-sm font-semibold text-status-warning mb-2">No agents installed</p>
          <p className="text-sm text-text-muted mb-3">Install at least one AI coding agent to create a session.</p>
          <div className="space-y-1.5">
            {agents
              .filter((a) => ["claude", "codex", "gemini"].includes(a.name))
              .map((agent) => (
                <div key={agent.name} className="flex items-baseline gap-2">
                  <span className="text-sm font-medium text-text-primary w-20">{agent.name}</span>
                  <code className="text-xs text-text-dim font-mono">{agent.install_hint}</code>
                </div>
              ))}
          </div>
        </div>
      )}

      {/* Agent picker */}
      <div className="grid grid-cols-2 gap-2">
        {selectableAgents.map((agent) => (
          <button
            type="button"
            key={agent.name}
            onClick={() => onChange("tool", agent.name)}
            className={`min-h-[44px] text-left p-3 rounded-lg border transition-colors cursor-pointer focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand-600 ${
              data.tool === agent.name
                ? "border-brand-600 bg-surface-900"
                : "border-surface-700 bg-surface-950 hover:border-surface-600"
            }`}
          >
            <div className="flex flex-wrap items-center gap-2">
              <span className="text-sm font-semibold text-text-primary">{agent.name}</span>
              {agent.kind === "custom" && (
                <span className="rounded px-1.5 py-px text-[10px] font-mono uppercase tracking-wide bg-surface-700 text-text-dim">
                  Custom
                </span>
              )}
            </div>
          </button>
        ))}
      </div>
    </div>
  );
}
