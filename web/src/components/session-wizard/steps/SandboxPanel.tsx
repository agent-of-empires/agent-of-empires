import { EnvVarList, LabeledInput } from "./LaunchFields";
import type { WizardData } from "../wizardReducer";

/** Container image and environment behind the wizard's Sandbox row. */
export function SandboxPanel({
  data,
  onChange,
}: {
  data: WizardData;
  onChange: (field: string, value: unknown) => void;
}) {
  return (
    <div className="space-y-5">
      <LabeledInput
        label="Container image"
        value={data.sandboxImage}
        onChange={(v) => onChange("sandboxImage", v)}
        placeholder="ghcr.io/agent-of-empires/aoe-sandbox:latest"
      />
      <EnvVarList values={data.extraEnv} onChange={(v) => onChange("extraEnv", v)} />
    </div>
  );
}
