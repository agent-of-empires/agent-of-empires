import { useCallback, useEffect, useMemo, useReducer } from "react";
import type { CreateSessionRequest, SessionResponse } from "../../lib/types";
import { fetchAgents, fetchGroups, fetchDockerStatus, fetchProfiles, fetchSettings, createSession } from "../../lib/api";
import { ACP_CAPABLE_TOOLS, isAcpCapable } from "../../lib/acpCapableTools";
import { safeGetItem, safeSetItem } from "../../lib/safeStorage";
import { toastBus } from "../../lib/toastBus";
import { StepIndicator } from "./StepIndicator";
import type { StepDef, StepId } from "./StepIndicator";
import { ProjectStep } from "./steps/ProjectStep";
import { SessionStep } from "./steps/SessionStep";
import { AgentStep } from "./steps/AgentStep";
import { ReviewStep } from "./steps/ReviewStep";
import { getSubmittedBranch } from "./sessionNames";
import { initialData, reducer, type WizardData } from "./wizardReducer";

/** localStorage key persisting the last tool the user picked in the
 *  wizard. Per-browser, scoped by tool registry key. Validated against
 *  ACP_CAPABLE_TOOLS on read so an outdated value (or one written by a
 *  different aoe install with extra agents registered) doesn't crash
 *  the wizard. See #1133 thread 7 / #1135. */
const LAST_USED_TOOL_KEY = "aoe-cockpit-last-tool";

function loadLastUsedTool(): string {
  const stored = safeGetItem(LAST_USED_TOOL_KEY);
  if (stored && ACP_CAPABLE_TOOLS.has(stored)) {
    return stored;
  }
  return "claude";
}

function saveLastUsedTool(tool: string): void {
  if (!ACP_CAPABLE_TOOLS.has(tool)) return;
  safeSetItem(LAST_USED_TOOL_KEY, tool);
}

/** Layer the last-used tool over the shared `initialData` template so
 *  fresh wizard opens default to whatever the user picked last. The
 *  prefill path overrides this when `prefill.tool` is set. */
function buildInitialData(): WizardData {
  return { ...initialData, tool: loadLastUsedTool() };
}

// Wizard: project path → session (title + worktree) → agent → review
function computeSteps(_data: WizardData): StepDef[] {
  return [
    { id: "project", label: "Project" },
    { id: "session", label: "Session" },
    { id: "agent", label: "Agent" },
    { id: "review", label: "Review" },
  ];
}

export interface WizardPrefill {
  path?: string;
  tool?: string;
  yoloMode?: boolean;
  sandboxEnabled?: boolean;
  profile?: string;
  group?: string;
  /** If true, skip to the review step (all fields pre-filled) */
  skipToReview?: boolean;
  /** Which tab to show initially on the project step */
  initialTab?: "recent" | "browse" | "clone";
  /** Open the wizard pre-configured for a scratch session: the
   *  `scratch` flag is on, no path is required, worktree controls are
   *  hidden. Pairs with `skipToReview` for the Cmd+Shift+N then
   *  Cmd+Enter fast-create flow. */
  scratch?: boolean;
}

interface Props {
  onClose: () => void;
  onCreated: (session?: SessionResponse) => void;
  prefill?: WizardPrefill;
  /** Live value of the cockpit master switch (`config.cockpit.enabled`).
   *  When true, ACP-capable tools create cockpit sessions automatically;
   *  when false, every new session is tmux. */
  cockpitMasterEnabled: boolean;
}

export function SessionWizard({ onClose, onCreated, prefill, cockpitMasterEnabled }: Props) {
  const baseInitial = buildInitialData();
  const prefillData: WizardData = prefill
    ? {
        ...baseInitial,
        path: prefill.scratch ? "" : (prefill.path || ""),
        tool: prefill.tool || baseInitial.tool,
        yoloMode: prefill.yoloMode ?? false,
        sandboxEnabled: prefill.sandboxEnabled ?? false,
        profile: prefill.profile || "",
        group: prefill.group || "",
        scratch: prefill.scratch ?? false,
        // Scratch mode clears worktree/extra-repos so the submit
        // payload mirrors what the reducer's SET_FIELD arm would emit
        // for a user-triggered scratch toggle. See wizardReducer.ts.
        useWorktree: prefill.scratch ? false : baseInitial.useWorktree,
        extraRepoPaths: prefill.scratch ? [] : baseInitial.extraRepoPaths,
      }
    : baseInitial;

  const [state, dispatch] = useReducer(reducer, {
    // Only `skipToReview` jumps directly to Review. The fast-create
    // shortcut sets both `scratch: true` and `skipToReview: true`, so
    // pairing them is still a single keystroke flow; gating on the
    // scratch flag alone conflicted with WizardPrefill's documented
    // contract (a wizard opened with `scratch: true` for "open at
    // ProjectStep with scratch pre-enabled" would have skipped past
    // the project step entirely).
    currentStep:
      prefill?.skipToReview
        ? 3
        : (prefill?.path ? 1 : 0),
    data: prefillData, isSubmitting: false, error: null,
    agents: [], groups: [], profiles: [], dockerAvailable: false,
  });

  const steps = useMemo(() => computeSteps(state.data),
    [state.data.sandboxEnabled, state.data.advancedEnabled]);

  const currentStepDef = steps[state.currentStep];
  const isFirst = state.currentStep === 0;
  const isLast = currentStepDef?.id === "review";

  useEffect(() => {
    fetchAgents().then((a) => dispatch({ type: "SET_AGENTS", agents: a }));
    fetchGroups().then((g) => dispatch({ type: "SET_GROUPS", groups: g }));
    fetchDockerStatus().then((d) => dispatch({ type: "SET_DOCKER", available: d.available }));

    // Seed the wizard with the resolved (global + active profile) defaults so
    // single-profile users get yolo_mode_default and friends without ever
    // touching the profile picker. The picker is hidden when
    // profiles.length <= 1 (`AgentStep.tsx`), so its onChange-driven
    // `APPLY_PROFILE_DEFAULTS` path never fires and the wizard would
    // otherwise fall back to default permissions, ignoring the profile.
    // See #1142.
    fetchProfiles().then((p) => {
      dispatch({ type: "SET_PROFILES", profiles: p });
      // Prefer an explicit prefill profile; otherwise use the server's active
      // profile (`is_default: true`). If neither resolves, pass undefined so
      // `fetchSettings` loads the unresolved global config.
      const effectiveProfile =
        prefill?.profile || p.find((x) => x.is_default)?.name || "";
      fetchSettings(effectiveProfile || undefined).then((s) => {
        if (!s) return;
        const sandbox = s.sandbox as Record<string, unknown> | undefined;
        const session = s.session as Record<string, unknown> | undefined;
        const img = (sandbox?.default_image as string) || "";
        if (img) dispatch({ type: "SET_FIELD", field: "sandboxImage", value: img });
        const env = Array.isArray(sandbox?.environment)
          ? (sandbox?.environment as unknown[]).filter(
              (v): v is string => typeof v === "string",
            )
          : [];
        // Honor explicit prefill values so a caller that sets yoloMode/
        // sandboxEnabled/tool isn't silently overridden by profile defaults.
        // Mirrors the per-field guards `AgentStep.handleProfileChange` skips
        // by going through the user-driven onChange path.
        dispatch({
          type: "APPLY_PROFILE_DEFAULTS",
          yoloMode:
            prefill?.yoloMode ??
            ((session?.yolo_mode_default as boolean) ?? false),
          sandboxEnabled:
            prefill?.sandboxEnabled ??
            ((sandbox?.enabled_by_default as boolean) ?? false),
          tool: prefill?.tool || (session?.default_tool as string) || "",
          extraEnv: env,
          skipIfDirty: true,
        });
      });
    });
    // prefill is captured at first render; we don't want to re-seed defaults
    // (and stomp on user edits) if the parent re-renders with a new object
    // identity.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const handleChange = useCallback((field: string, value: unknown) => {
    dispatch({ type: "SET_FIELD", field, value });
  }, []);

  const handleApplyProfileDefaults = useCallback((defaults: { yoloMode: boolean; sandboxEnabled: boolean; tool: string; extraEnv: string[] }) => {
    dispatch({ type: "APPLY_PROFILE_DEFAULTS", ...defaults });
  }, []);

  const goNext = () => { if (state.currentStep < steps.length - 1) dispatch({ type: "SET_STEP", step: state.currentStep + 1 }); };
  const goBack = () => { if (state.currentStep > 0) dispatch({ type: "SET_STEP", step: state.currentStep - 1 }); };
  const jumpTo = (stepId: StepId) => { const idx = steps.findIndex((s) => s.id === stepId); if (idx >= 0) dispatch({ type: "SET_STEP", step: idx }); };

  const handleSubmit = async () => {
    dispatch({ type: "SUBMIT_START" });
    const d = state.data;
    const selectedAgentAcpCapable = isAcpCapable(
      d.tool,
      state.agents.find((a) => a.name === d.tool)?.acp_capable,
    );
    // Scratch sessions: server provisions the working directory and
    // ignores `path`. Force-omit every worktree-related field so a
    // stale reducer state cannot make the server return 400 on the
    // `scratch + worktree_branch` mutex.
    const body: CreateSessionRequest = {
      path: d.scratch ? "" : d.path,
      tool: d.tool,
      title: d.title || undefined, group: d.group || undefined,
      yolo_mode: d.yoloMode,
      worktree_branch:
        !d.scratch && d.useWorktree
          ? getSubmittedBranch(d.title, d.worktreeBranch)
          : undefined,
      create_new_branch: !d.scratch && d.useWorktree && !d.attachExisting,
      base_branch:
        !d.scratch && d.useWorktree && !d.attachExisting && d.baseBranch.trim()
          ? d.baseBranch.trim()
          : undefined,
      sandbox: d.sandboxEnabled,
      sandbox_image: d.sandboxEnabled ? d.sandboxImage : undefined,
      extra_env: d.sandboxEnabled && d.extraEnv.length > 0 ? d.extraEnv.filter(Boolean) : undefined,
      extra_repo_paths:
        !d.scratch && d.extraRepoPaths.length > 0 ? d.extraRepoPaths : undefined,
      extra_args: d.extraArgs || undefined,
      command_override: d.commandOverride || undefined,
      custom_instruction: d.customInstruction || undefined,
      profile: d.profile || undefined,
      // Cockpit runs only when the master switch is on, the agent is
      // ACP-capable, and the user kept the per-session toggle on
      // (default). Capability comes from the server's per-agent
      // `acp_capable` flag (including custom agents with an
      // `agent_cockpit_cmd`) with hardcoded fallback while loading. The
      // server re-resolves capability and re-applies the master switch
      // (see src/server/api/sessions.rs), so a tampered request can't
      // escalate cockpit on for a non-capable agent.
      cockpit_mode:
        cockpitMasterEnabled &&
        selectedAgentAcpCapable &&
        d.useCockpit,
      scratch: d.scratch || undefined,
    };
    const result = await createSession(body);
    if (result.ok) {
      dispatch({ type: "SUBMIT_SUCCESS" });
      saveLastUsedTool(d.tool);
      const warnings = result.session?.warnings;
      if (warnings && warnings.length > 0) {
        for (const w of warnings) toastBus.handler?.error(w);
      }
      onCreated(result.session);
    } else dispatch({ type: "SUBMIT_ERROR", error: result.error || "Unknown error" });
  };

  useEffect(() => {
    if (state.currentStep >= steps.length) dispatch({ type: "SET_STEP", step: steps.length - 1 });
  }, [steps.length, state.currentStep]);

  const renderStep = () => {
    switch (currentStepDef?.id) {
      case "project":
        return <ProjectStep data={state.data} onChange={handleChange} initialTab={prefill?.initialTab} />;
      case "session":
        return <SessionStep data={state.data} onChange={handleChange} />;
      case "agent":
        return (
          <AgentStep
            data={state.data}
            onChange={handleChange}
            agents={state.agents}
            profiles={state.profiles}
            dockerAvailable={state.dockerAvailable}
            onApplyProfileDefaults={handleApplyProfileDefaults}
            cockpitMasterEnabled={cockpitMasterEnabled}
          />
        );
      case "review":
        return <ReviewStep data={state.data} onChange={handleChange} agents={state.agents} isSubmitting={state.isSubmitting} error={state.error} onSubmit={handleSubmit} onJumpTo={jumpTo} steps={steps} cockpitMasterEnabled={cockpitMasterEnabled} />;
      default:
        return null;
    }
  };

  // Scratch selection satisfies the project-step "need a project" gate
  // without a path: the server provisions the working directory on
  // submit. Otherwise require a path as before.
  const nextDisabled =
    currentStepDef?.id === "project" &&
    !state.data.scratch &&
    !state.data.path;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-black/60" onClick={onClose} />
      <div className="relative w-full max-w-lg bg-surface-800 border border-surface-700/30 rounded-xl flex flex-col max-h-[min(720px,90vh)]">
        <div className="flex items-center justify-between px-5 py-4 border-b border-surface-700/20">
          <h1 className="text-sm font-medium text-text-secondary">New session</h1>
          <button onClick={onClose} className="w-8 h-8 flex items-center justify-center text-text-dim hover:text-text-secondary cursor-pointer rounded-md hover:bg-surface-700/50 transition-colors" aria-label="Close">&times;</button>
        </div>
        <div className="flex-1 overflow-y-auto px-5 py-5">
          <StepIndicator steps={steps} currentIndex={state.currentStep} />
          {renderStep()}
        </div>
        {!isLast && (
          <div className="flex justify-between px-5 py-4 border-t border-surface-700/20">
            <button onClick={isFirst ? onClose : goBack}
              className="px-5 py-2.5 text-sm rounded-lg border border-surface-700 text-text-secondary hover:bg-surface-800 active:bg-surface-700 cursor-pointer transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand-600">
              {isFirst ? "Cancel" : "Back"}
            </button>
            <button onClick={goNext} disabled={nextDisabled}
              className={`px-5 py-2.5 text-sm rounded-lg font-semibold transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand-600 ${
                nextDisabled
                  ? "bg-brand-600/50 text-surface-900/50 cursor-not-allowed"
                  : "bg-brand-600 hover:bg-brand-700 active:bg-brand-800 text-surface-900 cursor-pointer"
              }`}>
              Next
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
