import { useCallback, useState } from "react";
import type { AgentInfo, ProfileInfo } from "../../../lib/types";
import { fetchSettings } from "../../../lib/api";
import { isAcpCapable } from "../../../lib/acpCapableTools";
import { resolveLaunchCommand } from "../../../lib/launchCommand";
import { commandMapsFromSettings, EMPTY_COMMAND_MAPS, type CommandMaps } from "../commandMaps";

interface WizardData {
  tool: string;
  title: string;
  worktreeBranch: string;
  useWorktree: boolean;
  profile: string;
  profileDirty: boolean;
  sandboxEnabled: boolean;
  yoloMode: boolean;
  advancedEnabled: boolean;
  sandboxImage: string;
  extraEnv: string[];
  customInstruction: string;
  extraArgs: string;
  commandOverride: string;
  useStructuredView: boolean;
  [key: string]: unknown;
}

interface Props {
  data: WizardData;
  onChange: (field: string, value: unknown) => void;
  agents: AgentInfo[];
  profiles: ProfileInfo[];
  dockerAvailable: boolean;
  onApplyProfileDefaults: (defaults: {
    yoloMode: boolean;
    sandboxEnabled: boolean;
    tool: string;
    extraEnv: string[];
    agentModel?: string;
    agentEffort?: string;
    commandMaps?: CommandMaps;
  }) => void;
  /** Profile-resolved override / custom-agent maps, used to preview the
   *  exact launch command. Sourced from the settings the wizard already
   *  fetched, so this step issues no extra request. See #1911. */
  commandMaps?: CommandMaps;
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
  return (
    <div className="mb-5 rounded-lg border border-surface-700 bg-surface-900 px-3 py-2.5">
      <div className="flex items-center justify-between gap-3">
        <div className="flex-1">
          <div className="flex items-center gap-2">
            <span className="text-sm font-semibold text-text-primary">Structured view</span>
          </div>
          <p className="mt-1 text-xs text-text-dim leading-snug">
            {sandboxedStructuredView
              ? "Structured view + container: the agent runs inside the sandbox container, so its file and terminal access stay inside the container's mounts. Turn off to run this session in the terminal view instead."
              : checked
                ? "Renders the agent's plan, tool calls, and diffs in the structured view. Turn off to run this session in the terminal view instead."
                : "This session will run in the terminal view (raw tmux). Turn on to use the structured view; you can also switch views from the session later."}
          </p>
        </div>
        <Toggle checked={checked} onChange={onChange} label="Use structured view" />
      </div>
    </div>
  );
}

function Toggle({
  checked,
  onChange,
  disabled,
  label,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  disabled?: boolean;
  label?: string;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      onClick={() => !disabled && onChange(!checked)}
      className={`relative inline-flex h-7 w-12 shrink-0 items-center rounded-full transition-colors duration-200 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand-600 ${
        disabled ? "opacity-40 cursor-not-allowed" : "cursor-pointer"
      } ${checked ? "bg-brand-600" : "bg-surface-700"}`}
    >
      <span
        className={`inline-block h-5 w-5 rounded-full bg-white shadow-sm transition-transform duration-200 ${
          checked ? "translate-x-6" : "translate-x-1"
        }`}
      />
    </button>
  );
}

export function AgentStep({
  data,
  onChange,
  agents,
  profiles,
  dockerAvailable,
  onApplyProfileDefaults,
  commandMaps = EMPTY_COMMAND_MAPS,
}: Props) {
  const selectableAgents = agents.filter((agent) => agent.kind === "custom" || agent.installed);
  const selectedAgent = agents.find((a) => a.name === data.tool);
  const selectedCustomAgent = selectedAgent?.kind === "custom";
  const acpCapable = isAcpCapable(data.tool, selectedAgent?.acp_capable);
  const isHostOnly = selectedAgent?.host_only ?? false;
  const [showAdvanced, setShowAdvanced] = useState(data.advancedEnabled);
  const showProfilePicker = profiles.length > 1;

  // Mirror SessionWizard.handleSubmit / ReviewStep so the preview shows
  // the view the session will actually launch with (#1580).
  const willUseStructuredView = acpCapable && data.useStructuredView;
  const resolvedCommand = resolveLaunchCommand({
    tool: data.tool,
    useStructuredView: willUseStructuredView,
    binary: selectedAgent?.binary,
    acpCommand: selectedAgent?.acp_command,
    acpArgs: selectedAgent?.acp_args,
    extraArgs: data.extraArgs,
    manualOverride: data.commandOverride,
    agentCommandOverride: commandMaps.agentCommandOverride,
    customAgents: commandMaps.customAgents,
  }).full;
  const extraArgsIgnored = willUseStructuredView && data.extraArgs.trim().length > 0;

  const handleProfileChange = useCallback(
    async (profileName: string) => {
      // If user had manual edits, confirm before overwriting
      if (data.profileDirty && profileName) {
        const ok = window.confirm("Selecting a profile will reset your settings to that profile's defaults. Continue?");
        if (!ok) return;
      }

      onChange("profile", profileName);

      if (!profileName) return;

      // Load profile-resolved settings (global + profile overrides merged)
      try {
        const settings = await fetchSettings(profileName);
        if (settings) {
          const session = settings.session as Record<string, unknown> | undefined;
          const sandbox = settings.sandbox as Record<string, unknown> | undefined;
          // Pre-populate sandbox env from the profile so the user can see and edit
          // it before submission; without this, an empty extra_env is sent and the
          // backend falls back to the wrong (globally-default) profile's env vars.
          const env = Array.isArray(sandbox?.environment)
            ? (sandbox.environment as unknown[]).filter((v): v is string => typeof v === "string")
            : [];
          const defaultTool = (session?.default_tool as string) || data.tool;
          const acpDefaults = session?.acp_defaults as Record<string, unknown> | undefined;
          const acpDefault = acpDefaults?.[defaultTool] as Record<string, unknown> | undefined;
          onApplyProfileDefaults({
            yoloMode: (session?.yolo_mode_default as boolean) ?? false,
            sandboxEnabled: (sandbox?.enabled_by_default as boolean) ?? false,
            tool: defaultTool,
            extraEnv: env,
            agentModel: typeof acpDefault?.model === "string" ? acpDefault.model : "",
            agentEffort: typeof acpDefault?.effort === "string" ? acpDefault.effort : "",
            commandMaps: commandMapsFromSettings(settings),
          });
        }
      } catch {
        // If we can't load profile settings, just set the profile name
      }
    },
    [data.profileDirty, data.tool, onChange, onApplyProfileDefaults],
  );

  return (
    <div>
      <h2 className="text-lg font-semibold text-text-primary mb-1">Which AI agent?</h2>
      <p className="text-sm text-text-muted mb-5">Pick the coding assistant and configure your session.</p>

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

      {/* Profile selector. We render a card list (rather than a native
          <select>) so each profile can carry a short description beneath
          its name. The card list also makes the active selection more
          obvious on touch devices. See #949. */}
      {showProfilePicker && (
        <div className="mb-5">
          <label className="block text-sm text-text-dim mb-1.5">Workflow preset</label>
          <p className="text-xs text-text-dim mb-2">
            Profiles preload tool, sandbox, auto-approve, and env defaults for common workflows.
          </p>
          <div role="radiogroup" aria-label="Workflow preset" className="space-y-1.5">
            <button
              type="button"
              role="radio"
              aria-checked={data.profile === ""}
              onClick={() => handleProfileChange("")}
              className={`w-full min-h-[44px] text-left p-3 rounded-lg border transition-colors cursor-pointer focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand-600 ${
                data.profile === ""
                  ? "border-brand-600 bg-surface-900"
                  : "border-surface-700 bg-surface-950 hover:border-surface-600"
              }`}
            >
              <div className="text-sm font-semibold text-text-primary">Server default</div>
              <div className="mt-0.5 text-xs text-text-dim leading-snug">
                Use the active profile on the server with no client-side preset.
              </div>
            </button>
            {profiles.map((p) => (
              <button
                type="button"
                role="radio"
                aria-checked={data.profile === p.name}
                key={p.name}
                onClick={() => handleProfileChange(p.name)}
                className={`w-full min-h-[44px] text-left p-3 rounded-lg border transition-colors cursor-pointer focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand-600 ${
                  data.profile === p.name
                    ? "border-brand-600 bg-surface-900"
                    : "border-surface-700 bg-surface-950 hover:border-surface-600"
                }`}
              >
                <div className="flex flex-wrap items-baseline gap-2">
                  <span className="text-sm font-semibold text-text-primary">{p.name}</span>
                  {p.is_default && (
                    <span className="rounded px-1.5 py-px text-[10px] font-mono uppercase tracking-wide bg-surface-700 text-text-dim">
                      Active
                    </span>
                  )}
                </div>
                {p.description && <div className="mt-0.5 text-xs text-text-dim leading-snug">{p.description}</div>}
              </button>
            ))}
          </div>
          {data.profile && data.profileDirty && (
            <p className="text-xs text-brand-500 mt-1">(Custom) Settings differ from preset defaults</p>
          )}
        </div>
      )}

      {/* Core toggles */}
      <div className="space-y-2 mb-4">
        <label
          className="flex items-center justify-between gap-3 p-3 bg-surface-900 border border-surface-700 rounded-lg cursor-pointer"
          onClick={() => !(isHostOnly || !dockerAvailable) && onChange("sandboxEnabled", !data.sandboxEnabled)}
        >
          <div className="flex-1">
            <div className="text-sm font-medium text-text-primary">Run in a safe container</div>
            <div className="text-xs text-text-dim mt-0.5 leading-snug">
              {!dockerAvailable
                ? "Docker is not running. Install or start Docker to use containers."
                : "Isolate the agent so it can't affect your system"}
            </div>
          </div>
          <Toggle
            checked={data.sandboxEnabled}
            onChange={(v) => onChange("sandboxEnabled", v)}
            disabled={isHostOnly || !dockerAvailable}
          />
        </label>

        <label
          className="flex items-center justify-between gap-3 p-3 bg-surface-900 border border-surface-700 rounded-lg cursor-pointer"
          onClick={() => onChange("yoloMode", !data.yoloMode)}
        >
          <div className="flex-1">
            <div className="text-sm font-medium text-text-primary">Auto-approve actions</div>
            <div className="text-xs text-text-dim mt-0.5 leading-snug">
              Let the agent run commands without asking. Faster, less safe.
            </div>
          </div>
          <Toggle checked={data.yoloMode} onChange={(v) => onChange("yoloMode", v)} />
        </label>
      </div>

      {isHostOnly && (
        <p className="text-xs text-status-warning mt-3 mb-3">
          {selectedAgent?.name} can only run on the host. Container is disabled
          {data.useWorktree ? "; go back and turn off “Create a worktree” too." : "."}
        </p>
      )}

      {/* Advanced settings (collapsible) */}
      <button
        onClick={() => {
          setShowAdvanced(!showAdvanced);
          if (!showAdvanced) onChange("advancedEnabled", true);
        }}
        className="flex items-center gap-2 text-sm text-text-dim hover:text-text-secondary py-2 cursor-pointer w-full"
      >
        <svg
          className={`w-3 h-3 transition-transform ${showAdvanced ? "rotate-90" : ""}`}
          viewBox="0 0 12 12"
          fill="currentColor"
        >
          <path
            d="M4.5 2l4.5 4-4.5 4"
            stroke="currentColor"
            strokeWidth="1.5"
            fill="none"
            strokeLinecap="round"
            strokeLinejoin="round"
          />
        </svg>
        Advanced settings
      </button>

      {showAdvanced && (
        <div className="mt-2 space-y-4 border-t border-surface-700/30 pt-4">
          {/* Container config (if sandbox enabled) */}
          {data.sandboxEnabled && (
            <>
              <div>
                <label className="block text-sm text-text-dim mb-1.5">Container image</label>
                <input
                  type="text"
                  value={data.sandboxImage}
                  onChange={(e) => onChange("sandboxImage", e.target.value)}
                  placeholder="ghcr.io/agent-of-empires/aoe-sandbox:latest"
                  className="w-full bg-surface-900 border border-surface-700 rounded-lg px-3 py-2.5 text-sm font-mono text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none"
                />
              </div>
              <div>
                <label className="block text-sm text-text-dim mb-1.5">Environment variables</label>
                {data.extraEnv.map((env, i) => (
                  <div key={i} className="flex gap-2 mb-1">
                    <input
                      type="text"
                      value={env}
                      onChange={(e) => {
                        const updated = [...data.extraEnv];
                        updated[i] = e.target.value;
                        onChange("extraEnv", updated);
                      }}
                      placeholder="KEY=value"
                      className="flex-1 bg-surface-900 border border-surface-700 rounded-md px-2 py-1.5 text-sm font-mono text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none"
                    />
                    <button
                      onClick={() =>
                        onChange(
                          "extraEnv",
                          data.extraEnv.filter((_, j) => j !== i),
                        )
                      }
                      className="px-2 text-text-dim hover:text-status-error cursor-pointer"
                    >
                      &times;
                    </button>
                  </div>
                ))}
                <button
                  onClick={() => onChange("extraEnv", [...data.extraEnv, ""])}
                  className="text-xs text-text-dim hover:text-text-secondary cursor-pointer"
                >
                  + Add variable
                </button>
              </div>
            </>
          )}

          {/* Custom instruction */}
          <div>
            <label className="block text-sm text-text-dim mb-1.5">Agent instructions</label>
            <textarea
              value={data.customInstruction}
              onChange={(e) => onChange("customInstruction", e.target.value)}
              placeholder="Custom instructions for this session..."
              rows={3}
              className="w-full bg-surface-900 border border-surface-700 rounded-lg px-3 py-2 text-sm text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none resize-y"
            />
          </div>

          {/* Extra args */}
          <div>
            <label className="block text-sm text-text-dim mb-1.5">Additional arguments</label>
            <input
              type="text"
              value={data.extraArgs}
              onChange={(e) => onChange("extraArgs", e.target.value)}
              placeholder="e.g. --port 8080"
              className="w-full bg-surface-900 border border-surface-700 rounded-lg px-3 py-2.5 text-sm font-mono text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none"
            />
            {extraArgsIgnored && (
              <p className="mt-1.5 text-xs text-status-warning" data-testid="extra-args-ignored">
                Extra args are ignored for structured-view sessions; use the command override to change the launch
                command.
              </p>
            )}
          </div>

          {/* Command override */}
          <div>
            <label className="block text-sm text-text-dim mb-1.5">Command override</label>
            <input
              type="text"
              value={data.commandOverride}
              onChange={(e) => onChange("commandOverride", e.target.value)}
              placeholder="Override the agent launch command"
              className="w-full bg-surface-900 border border-surface-700 rounded-lg px-3 py-2.5 text-sm font-mono text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none"
            />
            {resolvedCommand && (
              <p className="mt-1.5 text-xs text-text-dim" data-testid="resolved-launch-command">
                Resolved launch command: <code className="font-mono text-text-secondary">{resolvedCommand}</code>
              </p>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
