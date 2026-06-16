import type { AgentInfo, ProfileInfo } from "../../../lib/types";
import type { CommandMaps } from "../commandMaps";
import { AgentPickerEssentials } from "./AgentPickerEssentials";
import { AgentOptions } from "./AgentOptions";

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
  commandMaps?: CommandMaps;
}

/** Legacy whole-section view of the agent step: picker essentials on top,
 *  then the options block with its own collapsible Advanced fold. The
 *  single-screen wizard (#2210) renders `AgentPickerEssentials` and
 *  `AgentOptions` directly instead; this wrapper stays so the isolated
 *  AgentStep unit tests keep exercising both halves together. */
export function AgentStep({
  data,
  onChange,
  agents,
  profiles,
  dockerAvailable,
  onApplyProfileDefaults,
  commandMaps,
}: Props) {
  return (
    <div>
      <h2 className="text-lg font-semibold text-text-primary mb-1">Which AI agent?</h2>
      <p className="text-sm text-text-muted mb-5">Pick the coding assistant and configure your session.</p>
      <AgentPickerEssentials data={data} onChange={onChange} agents={agents} />
      <AgentOptions
        data={data}
        onChange={onChange}
        agents={agents}
        profiles={profiles}
        dockerAvailable={dockerAvailable}
        onApplyProfileDefaults={onApplyProfileDefaults}
        commandMaps={commandMaps}
        collapsibleAdvanced
      />
    </div>
  );
}
