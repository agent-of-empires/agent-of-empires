import { RefreshCw } from "lucide-react";
import { useEffect, useState } from "react";
import { fetchSetupRuntimes, type ContainerRuntimeKind, type RuntimeStatus } from "../../../lib/api";
import type { SettingsFieldDescriptor } from "../../../lib/types";
import { SchemaSection } from "../../settings/SchemaSection";
import { GhostButton, PANEL, RadioRow, Section } from "../ui";

const RUNTIMES: Record<ContainerRuntimeKind, { label: string; start: string }> = {
  docker: { label: "Docker", start: "docker desktop start" },
  podman: { label: "Podman", start: "podman machine start" },
  apple_container: { label: "Apple Container", start: "container system start" },
};

interface Props {
  schema: SettingsFieldDescriptor[];
  values: Record<string, unknown>;
  onSaveField: (section: string, field: string, value: unknown) => Promise<boolean>;
}

function runtimeState(r: RuntimeStatus) {
  if (r.running) return <span className="text-xs text-status-running">● Running</span>;
  if (r.installed) return <span className="text-xs text-status-error-text">× Installed, not running</span>;
  return <span className="text-xs text-text-dim">○ Not installed</span>;
}

export function SandboxStep({ schema, values, onSaveField }: Props) {
  const [runtimes, setRuntimes] = useState<RuntimeStatus[] | null>(null);
  const [checking, setChecking] = useState(true);
  const [checks, setChecks] = useState(0);
  useEffect(() => {
    let live = true;
    void fetchSetupRuntimes().then((result) => {
      if (!live) return;
      setRuntimes(result ?? []);
      setChecking(false);
    });
    return () => {
      live = false;
    };
  }, [checks]);
  const check = () => {
    setChecking(true);
    setChecks((n) => n + 1);
  };

  const [selected, setSelected] = useState(
    typeof values.container_runtime === "string" ? values.container_runtime : "docker",
  );
  const pick = async (kind: ContainerRuntimeKind) => {
    const previous = selected;
    setSelected(kind);
    if (!(await onSaveField("sandbox", "container_runtime", kind))) setSelected(previous);
  };

  const none = runtimes !== null && !runtimes.some((r) => r.installed);

  return (
    <>
      <Section
        title="Runtime"
        hint={none ? "No container runtime found. Sessions run on the host until you install one." : undefined}
        action={
          <GhostButton onClick={check} disabled={checking}>
            <span className="inline-flex items-center gap-1.5">
              <RefreshCw size={12} className={checking ? "animate-spin" : undefined} />
              {checking ? "Checking" : "Check again"}
            </span>
          </GhostButton>
        }
      >
        <div className={PANEL}>
          {runtimes === null && <div className="px-4 py-3 text-sm text-text-dim">Checking container runtimes…</div>}
          {runtimes?.map((r) => (
            <RadioRow key={r.kind} name="runtime" checked={selected === r.kind} onSelect={() => void pick(r.kind)}>
              <div className="flex items-center gap-6">
                <span className="w-32 text-sm text-text-primary">{RUNTIMES[r.kind].label}</span>
                {runtimeState(r)}
              </div>
              {r.installed && !r.running && (
                <p className="mt-1 text-xs text-text-dim">
                  To start it: <span className="font-mono text-text-secondary">{RUNTIMES[r.kind].start}</span>
                </p>
              )}
            </RadioRow>
          ))}
        </div>
      </Section>
      {!none && (
        <div className="rounded-lg border border-surface-700 bg-surface-900 p-4">
          <SchemaSection
            section="sandbox"
            schema={schema}
            values={values}
            onSaveField={onSaveField}
            onlyFields={["enabled_by_default", "default_image"]}
          />
        </div>
      )}
    </>
  );
}
