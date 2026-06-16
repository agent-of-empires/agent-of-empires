import type { AgentInfo } from "../../../lib/types";
import { isAcpCapable } from "../../../lib/acpCapableTools";
import { Toggle } from "./Toggle";

interface WizardData {
  tool: string;
  sandboxEnabled: boolean;
  useStructuredView: boolean;
  [key: string]: unknown;
}

interface Props {
  data: WizardData;
  onChange: (field: string, value: unknown) => void;
  agents: AgentInfo[];
}

/** Read-only callout when the selected tool cannot run in the structured view. This
 *  includes built-in tools without ACP support and custom agents that do
 *  not provide `agent_acp_cmd`. ACP-capable tools render
 *  `ViewPickerCard` instead. */
function ViewNotice({ tool, customAgent }: { tool: string; customAgent: boolean }) {
  return (
    <div className="mb-5 rounded-lg border border-surface-700 bg-surface-950 px-3 py-2.5">
      <div className="flex items-center gap-2">
        <span className="text-sm font-semibold text-text-primary">Terminal</span>
        <span className="rounded px-1.5 py-px text-[10px] font-mono uppercase tracking-wide bg-surface-700 text-text-dim">
          Fallback
        </span>
      </div>
      <p className="mt-1 text-xs text-text-dim leading-snug">
        {customAgent
          ? "Custom agents run in the terminal unless they define agent_acp_cmd in config or TUI settings."
          : `${tool} has no ACP adapter yet, so this session runs in the terminal view. Pick a tool with an ACP adapter (e.g. claude, opencode, gemini) to use the structured view.`}
      </p>
    </div>
  );
}

/** Interactive view picker shown when the selected tool is ACP-capable.
 *  Defaults on (the structured view is the default); turning it off launches a
 *  terminal-view session instead (see #1580). */
function ViewPickerCard({
  checked,
  onChange,
  sandboxEnabled,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  sandboxEnabled: boolean;
}) {
  const sandboxedStructuredView = checked && sandboxEnabled;
  // Styled to match the sibling Core toggles (sandbox / auto-approve):
  // a full-row clickable label, so the whole card is a hit target, not just
  // the 12px switch. See #2101.
  return (
    <label
      className="mb-5 flex items-center justify-between gap-3 p-3 bg-surface-900 border border-surface-700 rounded-lg cursor-pointer"
      onClick={(e) => {
        if ((e.target as HTMLElement).closest('button[role="switch"]')) return;
        onChange(!checked);
      }}
    >
      <div className="flex-1">
        <div className="text-sm font-medium text-text-primary">Structured view</div>
        <p className="text-xs text-text-dim mt-0.5 leading-snug">
          {sandboxedStructuredView
            ? "Structured view + container: the agent runs inside the sandbox container, so its file and terminal access stay inside the container's mounts. Turn off to run this session in the terminal view instead."
            : checked
              ? "Renders the agent's plan, tool calls, and diffs in the structured view. Turn off to run this session in the terminal view instead."
              : "This session will run in the terminal view (raw tmux). Turn on to use the structured view; you can also switch views from the session later."}
        </p>
      </div>
      <Toggle checked={checked} onChange={onChange} label="Use structured view" />
    </label>
  );
}

/** Always-visible essentials of the agent section: the agent picker grid
 *  and the structured-view choice. Split out of the old monolithic
 *  AgentStep (#2210) so the single-screen wizard can show these up top
 *  while the rest of the agent controls live behind the More options
 *  fold. */
export function AgentPickerEssentials({ data, onChange, agents }: Props) {
  const selectableAgents = agents.filter((agent) => agent.kind === "custom" || agent.installed);
  const selectedAgent = agents.find((a) => a.name === data.tool);
  const selectedCustomAgent = selectedAgent?.kind === "custom";
  const acpCapable = isAcpCapable(data.tool, selectedAgent?.acp_capable);

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
      <div className="grid grid-cols-2 gap-2 mb-5">
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

      {/* View picker. ACP-capable tools get a per-session structured-view toggle
          (default on, see #1580) so they can opt down to a terminal-view
          session. Tools that are not ACP-capable show a read-only fallback
          notice instead. */}
      {acpCapable ? (
        <ViewPickerCard
          checked={data.useStructuredView}
          onChange={(v) => onChange("useStructuredView", v)}
          sandboxEnabled={data.sandboxEnabled}
        />
      ) : (
        <ViewNotice tool={data.tool} customAgent={selectedCustomAgent} />
      )}
    </div>
  );
}
