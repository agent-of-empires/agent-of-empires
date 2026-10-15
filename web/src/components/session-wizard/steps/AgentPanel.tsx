import type { AgentInfo } from "../../../lib/types";
import { isAcpEligible } from "../../../lib/acpCapableTools";
import { resolveLaunchCommand } from "../../../lib/launchCommand";
import { EMPTY_COMMAND_MAPS, type CommandMaps } from "../commandMaps";
import { AgentPickerEssentials } from "./AgentPickerEssentials";
import { LabeledInput } from "./LaunchFields";
import type { WizardData } from "../wizardReducer";

interface Props {
  data: WizardData;
  onChange: (field: string, value: unknown) => void;
  agents: AgentInfo[];
  /** Profile-resolved maps for the launch command preview. */
  commandMaps?: CommandMaps;
}

/** Agent choice, instructions and launch-command knobs behind the wizard's Agent row. */
export function AgentPanel({ data, onChange, agents, commandMaps = EMPTY_COMMAND_MAPS }: Props) {
  const selectedAgent = agents.find((a) => a.name === data.tool);
  const willUseStructuredView = isAcpEligible(data.tool, selectedAgent) && data.useStructuredView;
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

  return (
    <div className="space-y-5">
      <AgentPickerEssentials data={data} onChange={onChange} agents={agents} />

      <div>
        <label htmlFor="wizard-instructions" className="block text-sm text-text-dim mb-1.5">
          Agent instructions
        </label>
        <textarea
          id="wizard-instructions"
          value={data.customInstruction}
          onChange={(e) => onChange("customInstruction", e.target.value)}
          placeholder="Custom instructions for this session..."
          rows={3}
          className="w-full bg-surface-900 border border-surface-700 rounded-lg px-3 py-2 text-base md:text-sm text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none resize-y"
        />
      </div>

      <LabeledInput
        label="Additional arguments"
        value={data.extraArgs}
        onChange={(v) => onChange("extraArgs", v)}
        placeholder="e.g. --port 8080"
      >
        {extraArgsIgnored && (
          <p className="mt-1.5 text-xs text-status-warning" data-testid="extra-args-ignored">
            Extra args are ignored for structured-view sessions; use the command override to change the launch command.
          </p>
        )}
      </LabeledInput>

      <LabeledInput
        label="Command override"
        value={data.commandOverride}
        onChange={(v) => onChange("commandOverride", v)}
        placeholder="Override the agent launch command"
      >
        {resolvedCommand && (
          <p className="mt-1.5 text-xs text-text-dim" data-testid="resolved-launch-command">
            Resolved launch command: <code className="font-mono text-text-secondary">{resolvedCommand}</code>
          </p>
        )}
      </LabeledInput>
    </div>
  );
}
