import type { AgentInfo } from "../../../lib/types";
import { effectiveLifecycle, type AgentLifecycleInfo } from "../../../lib/agentProfiles";

interface WizardData {
  tool: string;
  [key: string]: unknown;
}

interface Props {
  data: WizardData;
  onChange: (field: string, value: unknown) => void;
  agents: AgentInfo[];
}

/** One-line deprecation warning for a selected deprecated agent. Server
 *  lifecycle wins when present; falls back to the static profile mirror so
 *  the notice still renders against older daemons. Only ever called with
 *  the deprecated arm, where since/note are required strings. */
function lifecycleWarningText(name: string, lifecycle: Extract<AgentLifecycleInfo, { state: "deprecated" }>): string {
  const replacement = lifecycle.replacement ? `; consider switching to ${lifecycle.replacement}` : "";
  return `${name} is deprecated (since ${lifecycle.since}): ${lifecycle.note}${replacement}`;
}

/** Always-visible essentials of the agent section: just the agent picker
 *  grid. The structured-view choice lives in `AgentOptions` under the More
 *  options fold (#2210). */
export function AgentPickerEssentials({ data, onChange, agents }: Props) {
  const selectableAgents = agents.filter((agent) => agent.kind === "custom" || agent.installed);
  // The daemon's /api/agents lifecycle wins; the static profile mirror
  // covers older daemons and keys the server does not list.
  const selected = agents.find((agent) => agent.name === data.tool);
  const selectedLifecycle = effectiveLifecycle(selected, selected?.name);
  const selectedDeprecated =
    selected && selectedLifecycle.state === "deprecated"
      ? lifecycleWarningText(selected.name, selectedLifecycle)
      : null;

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
              {effectiveLifecycle(agent, agent.name).state === "deprecated" && (
                <span
                  className="rounded-full px-1.5 py-px text-[10px] uppercase tracking-wide bg-status-warning/15 text-status-warning"
                  data-testid={`wizard-agent-deprecated-badge-${agent.name}`}
                >
                  Deprecated
                </span>
              )}
            </div>
          </button>
        ))}
      </div>

      {/* Selected agent is deprecated: non-blocking notice under the grid. */}
      {selectedDeprecated && (
        <p className="mt-2 text-xs text-status-warning" data-testid="wizard-agent-deprecated-warning">
          {selectedDeprecated}
        </p>
      )}
    </div>
  );
}
