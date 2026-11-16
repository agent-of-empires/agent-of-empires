import { EnvVarList, LabeledInput } from "./LaunchFields";
import type { WizardData } from "../wizardReducer";
import { hasFinePointer } from "../../../lib/platform";

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
        autoFocus={hasFinePointer()}
        value={data.sandboxImage}
        onChange={(v) => onChange("sandboxImage", v)}
        placeholder="ghcr.io/agent-of-empires/aoe-sandbox:latest"
      />
      <EnvVarList label="Container environment" values={data.extraEnv} onChange={(v) => onChange("extraEnv", v)} />
      <EnvVarList
        label="Host hook environment"
        hint="Visible to host_hooks.before_start only; never passed into the container."
        values={data.hookEnv}
        onChange={(v) => onChange("hookEnv", v)}
      />
    </div>
  );
}
