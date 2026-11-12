import { ArrowRight, ChevronUp, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { useInlineMachineSave } from "../../hooks/useInlineMachineSave";
import {
  fetchMachineSettings,
  fetchTelemetryStatus,
  getSettingsSchema,
  setTelemetryConsent,
  updateSetup,
  type ServerAbout,
  type SettingsResponse,
  type SetupStatus,
  type SetupStepKey,
} from "../../lib/api";
import { configuredCount, firstOpenIndex, openRequiredStep, SETUP_STEP_INFO } from "../../lib/setup";
import type { SettingsFieldDescriptor } from "../../lib/types";
import { SetupElevationBar } from "./SetupElevationBar";
import { SetupRail } from "./SetupRail";
import { GhostButton } from "./ui";
import { AccessStep } from "./steps/AccessStep";
import { AgentsStep } from "./steps/AgentsStep";
import { ProjectStep } from "./steps/ProjectStep";
import { SandboxStep } from "./steps/SandboxStep";
import { ThemeTelemetryStep, type TelemetryLoad } from "./steps/ThemeTelemetryStep";
import { WorktreesStep } from "./steps/WorktreesStep";

interface Props {
  status: SetupStatus;
  about: ServerAbout | null;
  profile: string;
  /** Re-read setup status; the page renders whatever the caller then passes back. */
  refresh: () => Promise<SetupStatus | null>;
  /** Back to the dashboard. */
  onLeave: () => void;
}

type MachineValues = Partial<Record<string, Record<string, unknown>>>;

const LEAVE_NOTE = "Leave whenever you like. Anything unfinished stays on your dashboard.";
const RELOAD_FAILED = "Saved, but couldn't reload setup. Try again.";

export function SetupWizardPage({ status, about, profile, refresh, onLeave }: Props) {
  const steps = status.steps;
  const [params] = useSearchParams();
  const [index, setIndex] = useState(() => {
    const requested = steps.findIndex((s) => s.key === params.get("step"));
    return requested >= 0 ? requested : firstOpenIndex(steps);
  });
  const [schema, setSchema] = useState<SettingsFieldDescriptor[]>([]);
  const [schemaLoad, setSchemaLoad] = useState<TelemetryLoad>("loading");
  const [schemaChecks, setSchemaChecks] = useState(0);
  // `null` until loaded, so steps seed their selections from saved values.
  const [machine, setMachine] = useState<MachineValues | null>(null);
  const [machineFailed, setMachineFailed] = useState(false);
  const [machineChecks, setMachineChecks] = useState(0);
  const [share, setShare] = useState(false);
  const [doNotTrack, setDoNotTrack] = useState(false);
  const [telemetry, setTelemetry] = useState<TelemetryLoad>("loading");
  const [telemetryChecks, setTelemetryChecks] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [sheetOpen, setSheetOpen] = useState(false);
  const sheetRef = useRef<HTMLDialogElement>(null);
  const { save, elevation } = useInlineMachineSave();

  useEffect(() => {
    let live = true;
    void getSettingsSchema().then((s) => {
      if (!live) return;
      if (s) setSchema(s);
      setSchemaLoad(s ? "loaded" : "failed");
    });
    return () => {
      live = false;
    };
  }, [schemaChecks]);
  const retrySchema = () => {
    setSchemaLoad("loading");
    setSchemaChecks((n) => n + 1);
  };

  useEffect(() => {
    let live = true;
    void fetchMachineSettings().then((s: SettingsResponse | null) => {
      if (!live) return;
      // A failed read isn't empty settings: defaults would stand in for the saved choices.
      setMachineFailed(s === null);
      if (s) setMachine(s as MachineValues);
    });
    return () => {
      live = false;
    };
  }, [machineChecks]);
  const retryMachine = () => {
    setMachineFailed(false);
    setMachineChecks((n) => n + 1);
  };

  useEffect(() => {
    let live = true;
    void fetchTelemetryStatus().then((t) => {
      if (!live) return;
      if (t) {
        setShare(t.enabled);
        setDoNotTrack(t.do_not_track);
      }
      setTelemetry(t ? "loaded" : "failed");
    });
    return () => {
      live = false;
    };
  }, [telemetryChecks]);
  const retryTelemetry = () => {
    setTelemetry("loading");
    setTelemetryChecks((n) => n + 1);
  };

  // A modal <dialog> traps focus, makes the page inert, closes on Escape and restores focus.
  useEffect(() => {
    const sheet = sheetRef.current;
    if (sheetOpen) sheet?.showModal();
    else if (sheet?.open) sheet.close();
  }, [sheetOpen]);

  // Stable, so the Agents step's detection doesn't re-run on every status refresh.
  const onAgentsChecked = useCallback(() => void refresh(), [refresh]);

  /** A step that is now set up should no longer read as skipped. */
  const markConfigured = useCallback(
    async (key: SetupStepKey) => {
      const next = await refresh();
      const current = next?.steps.find((s) => s.key === key);
      if (current?.configured && current.skipped) {
        await updateSetup({ unskip_step: key });
        await refresh();
      }
    },
    [refresh],
  );

  const step = steps[index];
  if (!step) return null;
  const last = index === steps.length - 1;

  const saveField = async (section: string, field: string, value: unknown): Promise<boolean> => {
    const ok = await save({ [section]: { [field]: value } });
    if (ok) {
      setMachine((m) => ({ ...m, [section]: { ...m?.[section], [field]: value } }));
      // Not awaited: a slow status read must not reorder this save's success behind a newer one.
      void markConfigured(step.key);
    } else {
      // The write may still have landed; show what the Daemon holds.
      void fetchMachineSettings().then((s) => s && setMachine(s as MachineValues));
    }
    return ok;
  };

  const goTo = (i: number) => {
    setIndex(Math.max(0, Math.min(steps.length - 1, i)));
    setError(null);
    setSheetOpen(false);
  };

  const finish = async (current: SetupStatus) => {
    const open = openRequiredStep(current.steps);
    if (open) {
      goTo(current.steps.indexOf(open));
      setError(`${SETUP_STEP_INFO[open.key].title} is required to start Sessions.`);
      return;
    }
    const result = await updateSetup({ finished: true });
    if (!result.ok) {
      setError(result.message ?? "Couldn't finish setup.");
      return;
    }
    // On the stale status the dashboard would send the user straight back here.
    if (!(await refresh())) {
      setError(RELOAD_FAILED);
      return;
    }
    onLeave();
  };

  const run = async (action: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await action();
    } finally {
      setBusy(false);
    }
  };

  const next = () =>
    run(async () => {
      let current = status;
      // Saving the choice is what sets this step up; Next waits for the saved choice to load.
      if (step.key === "theme_telemetry" && !doNotTrack) {
        if (!(await setTelemetryConsent(share))) {
          setError("Couldn't save your telemetry choice. Try again.");
          return;
        }
        current = (await refresh()) ?? status;
      }
      // Next on an optional step keeps its defaults, which counts as set up.
      if (
        !step.required &&
        step.key !== "theme_telemetry" &&
        !current.steps.find((s) => s.key === step.key)?.configured
      ) {
        const result = await updateSetup({ confirm_step: step.key });
        if (!result.ok) {
          setError(result.message ?? "Couldn't save this step.");
          return;
        }
        current = (await refresh()) ?? current;
      }
      if (last) await finish(current);
      else goTo(index + 1);
    });

  const skip = () =>
    run(async () => {
      const result = await updateSetup({ skip_step: step.key });
      if (!result.ok) {
        setError(result.message ?? "Couldn't skip this step.");
        return;
      }
      const current = (await refresh()) ?? status;
      if (last) await finish(current);
      else goTo(index + 1);
    });

  const leave = () =>
    run(async () => {
      const result = await updateSetup({ deferred: true });
      if (!result.ok) {
        setError(result.message ?? "Couldn't leave setup.");
        return;
      }
      if (!(await refresh())) {
        setError(RELOAD_FAILED);
        return;
      }
      onLeave();
    });

  const done = configuredCount(steps);
  const percent = Math.round((done / steps.length) * 100);
  const needsMachine = step.key !== "theme_telemetry" && step.key !== "project";
  const needsSchema = step.key === "worktrees" || step.key === "sandbox" || step.key === "notifications";
  const blocked =
    (step.required && !step.configured) ||
    (step.key === "theme_telemetry" && telemetry !== "loaded") ||
    (needsMachine && machine === null) ||
    (needsSchema && schemaLoad !== "loaded");
  const info = SETUP_STEP_INFO[step.key];
  const values = (section: string) => machine?.[section] ?? {};

  const rail = <SetupRail steps={steps} current={index} onSelect={goTo} />;
  const leaveLink = (
    <button
      type="button"
      onClick={() => void leave()}
      className="inline-flex items-center gap-1 text-sm text-text-primary underline underline-offset-4 cursor-pointer"
    >
      Go to dashboard <ArrowRight size={12} />
    </button>
  );

  return (
    <div className="h-(--app-height) flex bg-surface-950 text-text-primary safe-area-inset">
      <aside className="hidden md:flex w-72 shrink-0 flex-col border-r border-surface-700 bg-surface-900">
        <div className="p-6 flex flex-col gap-4 border-b border-surface-700">
          <div className="flex items-center gap-2 text-sm">
            <span className="font-semibold">aoe</span>
            <span className="text-text-secondary">Setup</span>
          </div>
          <div className="flex justify-between text-xs text-text-secondary">
            <span>
              {done} of {steps.length} done
            </span>
            <span className="font-mono">{percent}%</span>
          </div>
          <div className="h-1 rounded-full bg-surface-700" aria-hidden="true">
            <div className="h-1 rounded-full bg-brand-600" style={{ width: `${percent}%` }} />
          </div>
        </div>
        <nav aria-label="Setup steps" className="p-3 flex-1 overflow-y-auto">
          {rail}
        </nav>
        <div className="p-6 border-t border-surface-700 flex flex-col gap-3">
          <p className="text-sm text-text-secondary">{LEAVE_NOTE}</p>
          {leaveLink}
        </div>
      </aside>

      <main className="flex-1 min-w-0 flex flex-col">
        <div className="md:hidden flex items-center gap-3 px-4 h-12 border-b border-surface-700">
          <span className="font-mono text-sm text-brand-500">
            Step {index + 1} of {steps.length}
          </span>
          <span className="text-sm text-text-secondary truncate">{info.title}</span>
          <button
            type="button"
            onClick={() => setSheetOpen(true)}
            className="ml-auto h-11 inline-flex items-center gap-1 text-sm cursor-pointer"
          >
            Steps <ChevronUp size={14} />
          </button>
        </div>

        <div className="flex-1 overflow-y-auto px-4 md:px-16 py-8 md:py-12">
          <div className="max-w-3xl flex flex-col gap-8">
            <header className="flex flex-col gap-2">
              <p className="font-mono text-sm text-brand-500">
                Step {index + 1} of {steps.length} · {step.required ? "Required" : "Optional"}
              </p>
              <h1 className="text-2xl font-semibold">{info.title}</h1>
              <p className="text-sm text-text-secondary">{info.lead}</p>
            </header>
            {step.key === "theme_telemetry" && (
              <ThemeTelemetryStep
                share={share}
                onShareChange={setShare}
                doNotTrack={doNotTrack}
                telemetry={telemetry}
                onRetryTelemetry={retryTelemetry}
              />
            )}
            {machine === null && needsMachine && !machineFailed && (
              <p className="text-sm text-text-dim">Loading settings…</p>
            )}
            {machine === null && needsMachine && machineFailed && (
              <div className="flex items-center gap-3">
                <p role="alert" className="text-sm text-status-error-text">
                  Couldn't load your settings.
                </p>
                <GhostButton onClick={retryMachine}>Check again</GhostButton>
              </div>
            )}
            {needsSchema && schemaLoad === "failed" && (
              <div className="flex items-center gap-3">
                <p role="alert" className="text-sm text-status-error-text">
                  Couldn't load the settings for this step.
                </p>
                <GhostButton onClick={retrySchema}>Check again</GhostButton>
              </div>
            )}
            {machine !== null && step.key === "agents" && (
              <AgentsStep
                profile={profile}
                defaultTool={values("session").default_tool as string | undefined}
                structuredView={values("acp").structured_view as string | undefined}
                save={saveField}
                onAgentsChecked={onAgentsChecked}
              />
            )}
            {step.key === "project" && <ProjectStep profile={profile} onAdded={() => void markConfigured("project")} />}
            {machine !== null && step.key === "worktrees" && (
              <WorktreesStep schema={schema} values={values("worktree")} onSaveField={saveField} />
            )}
            {machine !== null && step.key === "sandbox" && (
              <SandboxStep schema={schema} values={values("sandbox")} onSaveField={saveField} />
            )}
            {machine !== null && step.key === "notifications" && (
              <AccessStep
                status={status}
                about={about}
                schema={schema}
                values={values("web")}
                onSaveField={saveField}
              />
            )}
            {error && (
              <p role="alert" className="text-sm text-status-error-text">
                {error}
              </p>
            )}
          </div>
        </div>

        {elevation ? (
          <SetupElevationBar request={elevation} />
        ) : (
          <footer className="border-t border-surface-700 px-4 md:px-16 h-20 flex items-center gap-3">
            {step.required ? (
              <span className="text-sm text-text-dim">Required to start Sessions</span>
            ) : (
              <button
                type="button"
                onClick={() => void skip()}
                disabled={busy}
                className="h-10 px-3 text-sm text-text-secondary hover:text-text-primary cursor-pointer disabled:opacity-50"
              >
                Skip for now
              </button>
            )}
            <div className="ml-auto flex gap-2">
              <button
                type="button"
                onClick={() => goTo(index - 1)}
                disabled={index === 0 || busy}
                className="h-10 px-4 rounded-md border border-surface-700 text-sm text-text-secondary hover:bg-surface-800 disabled:opacity-40 cursor-pointer"
              >
                Back
              </button>
              <button
                type="button"
                onClick={() => void next()}
                disabled={busy || blocked}
                className="h-10 px-4 rounded-md bg-brand-600 text-sm font-medium text-white hover:bg-brand-700 disabled:opacity-50 cursor-pointer"
              >
                {last ? "Finish setup" : "Next"}
              </button>
            </div>
          </footer>
        )}
      </main>

      <dialog
        ref={sheetRef}
        aria-label="Setup steps"
        onClose={() => setSheetOpen(false)}
        className="md:hidden m-0 mt-auto w-full max-w-none bg-transparent p-0 text-text-primary backdrop:bg-black/60"
      >
        {sheetOpen && (
          <div className="rounded-t-lg border-t border-surface-700 bg-surface-900 p-4 flex flex-col gap-3">
            <div className="flex items-center justify-between">
              <span className="text-sm font-semibold">Setup steps</span>
              <span className="ml-auto mr-3 text-xs text-text-secondary">
                {done} of {steps.length} done
              </span>
              <button
                type="button"
                aria-label="Close"
                onClick={() => setSheetOpen(false)}
                className="h-11 w-11 flex items-center justify-center cursor-pointer"
              >
                <X size={16} />
              </button>
            </div>
            {rail}
            <p className="text-sm text-text-secondary">{LEAVE_NOTE}</p>
            {leaveLink}
          </div>
        )}
      </dialog>
    </div>
  );
}
