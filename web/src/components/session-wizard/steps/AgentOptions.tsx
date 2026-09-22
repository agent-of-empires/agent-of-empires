import { useCallback } from "react";
import type { AgentInfo, ProfileInfo } from "../../../lib/types";
import { fetchSettings } from "../../../lib/api";
import { isAcpEligible } from "../../../lib/acpCapableTools";
import { resolveLaunchCommand } from "../../../lib/launchCommand";
import { commandMapsFromSettings, EMPTY_COMMAND_MAPS, type CommandMaps } from "../commandMaps";
import { profileDefaults, type ProfileDefaults } from "../profileDefaults";
import { AdvancedLaunchFields } from "./AdvancedLaunchFields";
import { ProfilePresetPicker } from "./ProfilePresetPicker";
import { ToggleRow } from "./Toggle";
import type { WizardData } from "../wizardReducer";

interface Props {
  data: WizardData;
  onChange: (field: string, value: unknown) => void;
  agents: AgentInfo[];
  profiles: ProfileInfo[];
  dockerAvailable: boolean;
  onApplyProfileDefaults: (defaults: ProfileDefaults & { commandMaps?: CommandMaps }) => void;
  /** Profile-resolved maps for the launch command preview. */
  commandMaps?: CommandMaps;
}

/** Terminal fallback notice for tools that cannot use the structured view. */
function ViewNotice({
  tool,
  customAgent,
  policyDenied,
}: {
  tool: string;
  customAgent: boolean;
  policyDenied: boolean;
}) {
  return (
    <div className="mb-5 rounded-lg border border-surface-700 bg-surface-950 px-3 py-2.5">
      <div className="flex items-center gap-2">
        <span className="text-sm font-semibold text-text-primary">Terminal</span>
        <span className="rounded px-1.5 py-px text-[10px] font-mono uppercase tracking-wide bg-surface-700 text-text-dim">
          Fallback
        </span>
      </div>
      <p className="mt-1 text-xs text-text-dim leading-snug">
        {policyDenied
          ? `${tool} is not on the operator's allowed agents list, so this session runs in the terminal view. Pick a permitted agent to use the structured view.`
          : customAgent
            ? "Custom agents run in the terminal unless they define agent_acp_cmd in config or TUI settings."
            : `${tool} has no ACP adapter yet, so this session runs in the terminal view. Pick a tool with an ACP adapter (e.g. claude, opencode, gemini) to use the structured view.`}
      </p>
    </div>
  );
}

function ViewPickerCard({
  checked,
  onChange,
  sandboxEnabled,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  sandboxEnabled: boolean;
}) {
  return (
    <ToggleRow
      className="mb-5 cursor-pointer"
      title="Structured view"
      description={
        checked && sandboxEnabled
          ? "Structured view + container: the agent runs inside the sandbox container, so its file and terminal access stay inside the container's mounts. Turn off to run this session in the terminal view instead."
          : checked
            ? "Renders the agent's plan, tool calls, and diffs in the structured view. Turn off to run this session in the terminal view instead."
            : "This session will run in the terminal view (raw tmux). Turn on to use the structured view; you can also switch views from the session later."
      }
      checked={checked}
      onChange={onChange}
      switchLabel="Use structured view"
    />
  );
}

/** Structured view choice, workflow preset, sandbox and auto-approve toggles, and launch knobs. */
export function AgentOptions({
  data,
  onChange,
  agents,
  profiles,
  dockerAvailable,
  onApplyProfileDefaults,
  commandMaps = EMPTY_COMMAND_MAPS,
}: Props) {
  const selectedAgent = agents.find((a) => a.name === data.tool);
  const selectedCustomAgent = selectedAgent?.kind === "custom";
  const acpCapable = isAcpEligible(data.tool, selectedAgent);
  const isHostOnly = selectedAgent?.host_only ?? false;
  const showProfilePicker = profiles.length > 1;

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
      // A hand-set view is not in `profileDirty`, but the profile's view default would replace it.
      if ((data.profileDirty || data.structuredViewDirty) && profileName) {
        const ok = window.confirm("Selecting a profile will reset your settings to that profile's defaults. Continue?");
        if (!ok) return;
      }

      onChange("profile", profileName);

      if (!profileName) return;

      try {
        const settings = await fetchSettings(profileName);
        if (settings) {
          onApplyProfileDefaults({
            ...profileDefaults(settings, "", data.tool),
            resetStructuredViewDirty: true,
            commandMaps: commandMapsFromSettings(settings),
          });
        }
      } catch {
        // Keep just the profile name.
      }
    },
    [data.profileDirty, data.structuredViewDirty, data.tool, onChange, onApplyProfileDefaults],
  );

  return (
    <div>
      {acpCapable ? (
        <ViewPickerCard
          checked={data.useStructuredView}
          onChange={(v) => onChange("useStructuredView", v)}
          sandboxEnabled={data.sandboxEnabled}
        />
      ) : (
        <ViewNotice
          tool={data.tool}
          customAgent={selectedCustomAgent}
          policyDenied={selectedAgent?.acp_allowed === false}
        />
      )}

      {showProfilePicker && (
        <ProfilePresetPicker
          profiles={profiles}
          selected={data.profile}
          dirty={data.profileDirty}
          onSelect={(name) => void handleProfileChange(name)}
        />
      )}

      <div className="space-y-2 mb-4">
        <ToggleRow
          title="Run in a safe container"
          description={
            !dockerAvailable
              ? "Docker is not running. Install or start Docker to use containers."
              : "Isolate the agent so it can't affect your system"
          }
          checked={data.sandboxEnabled}
          onChange={(v) => onChange("sandboxEnabled", v)}
          disabled={isHostOnly || !dockerAvailable}
        />
        <ToggleRow
          title="Auto-approve actions"
          description="Let the agent run commands without asking. Faster, less safe."
          checked={data.yoloMode}
          onChange={(v) => onChange("yoloMode", v)}
        />
      </div>

      {isHostOnly && (
        <p className="text-xs text-status-warning mt-3 mb-3">
          {selectedAgent?.name} can only run on the host. Container is disabled
          {data.useWorktree ? "; turn off “Create a worktree” under More options too." : "."}
        </p>
      )}

      <AdvancedLaunchFields
        data={data}
        extraArgsIgnored={extraArgsIgnored}
        resolvedCommand={resolvedCommand}
        onChange={onChange}
      />
    </div>
  );
}
