import { useCallback, useEffect, useReducer, useState } from "react";
import type { CreateSessionRequest, SessionResponse } from "../../lib/types";
import {
  fetchAgents,
  fetchGroups,
  fetchDockerStatus,
  fetchProfiles,
  fetchSettings,
  createSession,
  fetchVolumeIgnoresPreview,
  fetchIsGitRepo,
  markVolumeIgnoresGlobsAcknowledged,
  type VolumeIgnoresGlobPreview,
  type HooksNeedTrust,
} from "../../lib/api";
import { VolumeIgnoresGlobDialog } from "./VolumeIgnoresGlobDialog";
import { HooksTrustDialog } from "./HooksTrustDialog";
import { ACP_CAPABLE_TOOLS, isAcpCapable } from "../../lib/acpCapableTools";
import { safeGetItem, safeSetItem } from "../../lib/safeStorage";
import { toastBus } from "../../lib/toastBus";
import { ProjectStep } from "./steps/ProjectStep";
import { SessionStep } from "./steps/SessionStep";
import { AgentPickerEssentials } from "./steps/AgentPickerEssentials";
import { AgentOptions } from "./steps/AgentOptions";
import { LaunchFooter } from "./LaunchFooter";
import { initialData, reducer, type WizardData } from "./wizardReducer";
import { commandMapsFromSettings, EMPTY_COMMAND_MAPS, type CommandMaps } from "./commandMaps";

/** localStorage key persisting the last tool the user picked in the
 *  wizard. Per-browser, scoped by tool registry key. Validated against
 *  ACP_CAPABLE_TOOLS on read so an outdated value (or one written by a
 *  different aoe install with extra agents registered) doesn't crash
 *  the wizard. See #1133 thread 7 / #1135. */
const LAST_USED_TOOL_KEY = "aoe-acp-last-tool";

/** localStorage key remembering whether the user expanded the single-screen
 *  wizard's "More options" fold. Collapsed by default on a fresh browser;
 *  once opened it stays open across opens so power users are not re-folded
 *  every time. See #2210. */
const MORE_OPTIONS_OPEN_KEY = "aoe-new-session-more-options-open";

/** localStorage key remembering the last agent-instruction text the user
 *  submitted. Per-browser, so someone who reuses the same instruction on
 *  every session does not retype it. Free text, so no validation on read
 *  unlike the tool key. Not ACP-scoped (custom_instruction ships for
 *  tmux-passthrough sessions too), so it uses the new-session prefix. See
 *  #2614. */
const LAST_USED_INSTRUCTION_KEY = "aoe-new-session-last-instruction";

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

function loadLastUsedInstruction(): string {
  return safeGetItem(LAST_USED_INSTRUCTION_KEY) ?? "";
}

function saveLastUsedInstruction(instruction: string): void {
  safeSetItem(LAST_USED_INSTRUCTION_KEY, instruction);
}

function loadMoreOptionsOpen(): boolean {
  return safeGetItem(MORE_OPTIONS_OPEN_KEY) === "true";
}

function saveMoreOptionsOpen(open: boolean): void {
  safeSetItem(MORE_OPTIONS_OPEN_KEY, open ? "true" : "false");
}

/** Layer the last-used tool over the shared `initialData` template so
 *  fresh wizard opens default to whatever the user picked last. The
 *  prefill path overrides this when `prefill.tool` is set. */
function buildInitialData(): WizardData {
  return { ...initialData, tool: loadLastUsedTool(), customInstruction: loadLastUsedInstruction() };
}

function acpDefaultsFor(session: Record<string, unknown> | undefined, tool: string): { model: string; effort: string } {
  const defaults = session?.acp_defaults as Record<string, unknown> | undefined;
  const entry = defaults?.[tool] as Record<string, unknown> | undefined;
  return {
    model: typeof entry?.model === "string" ? entry.model : "",
    effort: typeof entry?.effort === "string" ? entry.effort : "",
  };
}

export interface WizardPrefill {
  path?: string;
  tool?: string;
  yoloMode?: boolean;
  sandboxEnabled?: boolean;
  profile?: string;
  group?: string;
  /** Which tab to show initially on the project section */
  initialTab?: "recent" | "browse" | "clone";
  /** Open the wizard pre-configured for a scratch session: the
   *  `scratch` flag is on, no path is required, worktree controls are
   *  hidden. The single screen is already one Cmd+Enter from launch. */
  scratch?: boolean;
}

interface Props {
  onClose: () => void;
  onCreated: (session?: SessionResponse) => void;
  prefill?: WizardPrefill;
}

export function SessionWizard({ onClose, onCreated, prefill }: Props) {
  const baseInitial = buildInitialData();
  const prefillData: WizardData = prefill
    ? {
        ...baseInitial,
        path: prefill.scratch ? "" : prefill.path || "",
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
    data: prefillData,
    isSubmitting: false,
    error: null,
    agents: [],
    groups: [],
    profiles: [],
    dockerAvailable: false,
  });

  // "More options" fold. Local UI state (not reducer/domain data),
  // persisted per browser so it stays open for users who expanded it.
  const [moreOpen, setMoreOpen] = useState(loadMoreOptionsOpen);
  const toggleMoreOpen = useCallback(() => {
    setMoreOpen((open) => {
      const next = !open;
      saveMoreOptionsOpen(next);
      return next;
    });
  }, []);

  // Profile-resolved override/custom-agent maps for the launch-command
  // preview. Sourced from the settings the wizard already fetches on open
  // and on a profile switch, so the preview adds no extra request. See
  // #1911.
  const [commandMaps, setCommandMaps] = useState<CommandMaps>(EMPTY_COMMAND_MAPS);
  // Pending sandbox create paused on the glob volume_ignores confirm modal
  // (#2045). Holds the matched patterns to explain and the request to replay
  // once the user proceeds.
  const [globConfirm, setGlobConfirm] = useState<{
    globs: VolumeIgnoresGlobPreview[];
    body: CreateSessionRequest;
  } | null>(null);
  // Pending create paused on the hooks-trust confirm modal (#2066). Holds the
  // commands to show and the request to replay with `trust_hooks: true`.
  const [hooksTrust, setHooksTrust] = useState<{
    info: HooksNeedTrust;
    body: CreateSessionRequest;
    tool: string;
  } | null>(null);

  useEffect(() => {
    fetchAgents().then((a) => dispatch({ type: "SET_AGENTS", agents: a }));
    fetchGroups().then((g) => dispatch({ type: "SET_GROUPS", groups: g }));
    fetchDockerStatus().then((d) => dispatch({ type: "SET_DOCKER", available: d.available }));

    // Seed the wizard with the resolved (global + active profile) defaults so
    // single-profile users get yolo_mode_default and friends without ever
    // touching the profile picker. The picker is hidden when
    // profiles.length <= 1 (`AgentOptions.tsx`), so its onChange-driven
    // `APPLY_PROFILE_DEFAULTS` path never fires and the wizard would
    // otherwise fall back to default permissions, ignoring the profile.
    // See #1142.
    fetchProfiles().then((p) => {
      dispatch({ type: "SET_PROFILES", profiles: p });
      // Prefer an explicit prefill profile; otherwise use the server's active
      // profile (`is_default: true`). If neither resolves, pass undefined so
      // `fetchSettings` loads the unresolved global config.
      const effectiveProfile = prefill?.profile || p.find((x) => x.is_default)?.name || "";
      fetchSettings(effectiveProfile || undefined).then((s) => {
        if (!s) return;
        setCommandMaps(commandMapsFromSettings(s));
        const sandbox = s.sandbox as Record<string, unknown> | undefined;
        const session = s.session as Record<string, unknown> | undefined;
        const worktree = s.worktree as Record<string, unknown> | undefined;
        const img = (sandbox?.default_image as string) || "";
        if (img) dispatch({ type: "SET_FIELD", field: "sandboxImage", value: img });
        const env = Array.isArray(sandbox?.environment)
          ? (sandbox?.environment as unknown[]).filter((v): v is string => typeof v === "string")
          : [];
        const defaultTool = prefill?.tool || (session?.default_tool as string) || "";
        const acpDefaults = acpDefaultsFor(session, defaultTool || state.data.tool);
        // Honor explicit prefill values so a caller that sets yoloMode/
        // sandboxEnabled/tool isn't silently overridden by profile defaults.
        // Mirrors the per-field guards `AgentOptions.handleProfileChange` skips
        // by going through the user-driven onChange path.
        dispatch({
          type: "APPLY_PROFILE_DEFAULTS",
          yoloMode: prefill?.yoloMode ?? (session?.yolo_mode_default as boolean) ?? false,
          sandboxEnabled: prefill?.sandboxEnabled ?? (sandbox?.enabled_by_default as boolean) ?? false,
          worktreeEnabled: (worktree?.enabled as boolean) ?? false,
          tool: defaultTool,
          extraEnv: env,
          agentModel: acpDefaults.model,
          agentEffort: acpDefaults.effort,
          skipIfDirty: true,
        });
      });
    });
    // prefill is captured at first render; we don't want to re-seed defaults
    // (and stomp on user edits) if the parent re-renders with a new object
    // identity.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Probe whether the selected path is a git repository so the worktree
  // toggle can be disabled for a plain folder (e.g. a root picked via "Use
  // this folder"). `/api/git/is-repo` uses the same `GitWorktree::is_git_repo`
  // gate the builder enforces, so the UI matches the server's accept/reject.
  // Only act on a definitive answer: on a transient failure (null) leave the
  // optimistic default so a probe blip can't misreport a repo as a non-repo.
  // Scratch sessions have no path and never use a worktree, so skip the probe.
  const probePath = state.data.scratch ? "" : state.data.path;
  useEffect(() => {
    if (!probePath) return;
    let cancelled = false;
    fetchIsGitRepo(probePath).then((isRepo) => {
      if (!cancelled && isRepo !== null) {
        dispatch({ type: "SET_FIELD", field: "pathIsGitRepo", value: isRepo });
      }
    });
    return () => {
      cancelled = true;
    };
  }, [probePath]);

  const handleChange = useCallback((field: string, value: unknown) => {
    dispatch({ type: "SET_FIELD", field, value });
  }, []);

  const handleApplyProfileDefaults = useCallback(
    (defaults: {
      yoloMode: boolean;
      sandboxEnabled: boolean;
      worktreeEnabled: boolean;
      tool: string;
      extraEnv: string[];
      agentModel?: string;
      agentEffort?: string;
      commandMaps?: CommandMaps;
    }) => {
      const { commandMaps: maps, ...rest } = defaults;
      if (maps) setCommandMaps(maps);
      dispatch({ type: "APPLY_PROFILE_DEFAULTS", ...rest });
    },
    [],
  );

  const handleSubmit = async () => {
    dispatch({ type: "SUBMIT_START" });
    const d = state.data;
    const selectedAgentAcpCapable = isAcpCapable(d.tool, state.agents.find((a) => a.name === d.tool)?.acp_capable);
    // Scratch sessions: server provisions the working directory and
    // ignores `path`. Force-omit every worktree-related field so a
    // stale reducer state cannot make the server return 400 on the
    // `scratch + worktree_branch` mutex.
    const body: CreateSessionRequest = {
      path: d.scratch ? "" : d.path,
      tool: d.tool,
      title: d.title || undefined,
      group: d.group || undefined,
      yolo_mode: d.yoloMode,
      worktree_enabled: !d.scratch && d.useWorktree,
      worktree_branch:
        !d.scratch && d.useWorktree && d.worktreeBranchDirty && d.worktreeBranch.trim()
          ? d.worktreeBranch.trim()
          : undefined,
      create_new_branch: !d.scratch && d.useWorktree && !d.attachExisting,
      base_branch:
        !d.scratch && d.useWorktree && !d.attachExisting && d.baseBranch.trim() ? d.baseBranch.trim() : undefined,
      sandbox: d.sandboxEnabled,
      sandbox_image: d.sandboxEnabled ? d.sandboxImage : undefined,
      extra_env: d.sandboxEnabled && d.extraEnv.length > 0 ? d.extraEnv.filter(Boolean) : undefined,
      extra_repo_paths: !d.scratch && d.extraRepoPaths.length > 0 ? d.extraRepoPaths : undefined,
      extra_args: d.extraArgs || undefined,
      command_override: d.commandOverride || undefined,
      custom_instruction: d.customInstruction || undefined,
      profile: d.profile || undefined,
      // Structured view runs when the agent is ACP-capable and the user
      // kept the per-session toggle on (default). Capability comes from
      // the server's per-agent
      // `acp_capable` flag (including custom agents with an
      // `agent_acp_cmd`) with hardcoded fallback while loading. The
      // server re-resolves capability (see src/server/api/sessions.rs),
      // so a tampered request can't escalate structured view on for a
      // non-capable agent.
      view: selectedAgentAcpCapable && d.useStructuredView ? "structured" : "terminal",
      agent_model: selectedAgentAcpCapable && d.useStructuredView && d.agentModel ? d.agentModel : undefined,
      agent_effort: selectedAgentAcpCapable && d.useStructuredView && d.agentEffort ? d.agentEffort : undefined,
      scratch: d.scratch || undefined,
      // #2276: importing an existing Claude session. The server adopts this
      // id as the session's acp_session_id and resumes it via session/load.
      import_acp_session_id: d.importAcpSessionId || undefined,
    };

    // Sandbox sessions whose resolved config has glob volume_ignores get a
    // one-time snapshot-expansion confirmation before we create (#2045). Skip
    // for scratch (no project path to expand against) and treat any preview
    // failure as "nothing to confirm" so it never blocks creation.
    if (d.sandboxEnabled && !d.scratch && d.path) {
      const preview = await fetchVolumeIgnoresPreview(d.path, d.profile || undefined);
      if (preview && !preview.acknowledged && preview.globs.length > 0) {
        setGlobConfirm({ globs: preview.globs, body });
        return;
      }
    }

    await runCreate(body, d.tool);
  };

  const runCreate = async (body: CreateSessionRequest, tool: string) => {
    const result = await createSession(body);
    if (result.ok) {
      dispatch({ type: "SUBMIT_SUCCESS" });
      saveLastUsedTool(tool);
      saveLastUsedInstruction(body.custom_instruction ?? "");
      const warnings = result.session?.warnings;
      if (warnings && warnings.length > 0) {
        for (const w of warnings) toastBus.handler?.error(w);
      }
      onCreated(result.session);
    } else if (result.hooksNeedTrust && !body.trust_hooks) {
      // The repo's hooks need approval (#2066). Pause and show the trust
      // dialog; on confirm we replay with `trust_hooks: true`. The
      // `!body.trust_hooks` guard avoids looping if the server still refuses
      // after we already opted in.
      setHooksTrust({ info: result.hooksNeedTrust, body, tool });
    } else
      dispatch({
        type: "SUBMIT_ERROR",
        error: result.error || "Unknown error",
      });
  };

  const handleGlobConfirm = async (dontShowAgain: boolean) => {
    const pending = globConfirm;
    if (!pending) return;
    if (dontShowAgain) await markVolumeIgnoresGlobsAcknowledged();
    setGlobConfirm(null);
    await runCreate(pending.body, state.data.tool);
  };

  const handleGlobCancel = () => {
    setGlobConfirm(null);
    dispatch({ type: "SUBMIT_CANCEL" });
  };

  const handleHooksTrustConfirm = async () => {
    const pending = hooksTrust;
    if (!pending) return;
    setHooksTrust(null);
    await runCreate({ ...pending.body, trust_hooks: true }, pending.tool);
  };

  const handleHooksTrustCancel = () => {
    setHooksTrust(null);
    dispatch({ type: "SUBMIT_CANCEL" });
  };

  return (
    <div className="fixed inset-0 z-[60] flex items-center justify-center">
      <div className="absolute inset-0 bg-black/60" onClick={onClose} />
      <div
        data-testid="session-wizard"
        className="relative w-full max-w-lg bg-surface-800 border border-surface-700/30 rounded-lg flex flex-col max-h-[min(720px,90vh)]"
      >
        <div className="flex items-center justify-between px-5 py-4 border-b border-surface-700/20">
          <h1 className="text-sm font-medium text-text-secondary">New session</h1>
          <button
            onClick={onClose}
            className="w-8 h-8 flex items-center justify-center text-text-dim hover:text-text-secondary cursor-pointer rounded-md hover:bg-surface-700/50 transition-colors"
            aria-label="Close"
          >
            &times;
          </button>
        </div>
        <div className="flex-1 overflow-y-auto px-5 py-5 space-y-6">
          <ProjectStep
            data={state.data}
            onChange={handleChange}
            initialTab={prefill?.initialTab}
            agents={state.agents}
          />

          <div>
            <label className="block text-sm text-text-dim mb-1.5">Session title</label>
            <input
              type="text"
              value={state.data.title}
              onChange={(e) => handleChange("title", e.target.value)}
              placeholder="Auto-generated if empty"
              className="w-full bg-surface-900 border border-surface-700 rounded-lg px-3 py-2.5 text-base font-mono text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none"
            />
            <p className="text-xs text-text-dim mt-1">
              Shown in the dashboard. Renaming it later does not rename the git branch.
            </p>
          </div>

          <div>
            <h2 className="text-lg font-semibold text-text-primary mb-1">Which AI agent?</h2>
            <p className="text-sm text-text-muted mb-5">Pick the coding assistant for this session.</p>
            <AgentPickerEssentials data={state.data} onChange={handleChange} agents={state.agents} />
          </div>

          <div className="border-t border-surface-700/20 pt-4">
            <button
              type="button"
              onClick={toggleMoreOpen}
              aria-expanded={moreOpen}
              className="flex items-center gap-2 text-sm font-medium text-text-secondary hover:text-text-primary py-1 cursor-pointer w-full"
            >
              <svg
                className={`w-3 h-3 transition-transform ${moreOpen ? "rotate-90" : ""}`}
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
              More options
            </button>
            {moreOpen && (
              <div className="mt-4 space-y-6">
                <SessionStep data={state.data} onChange={handleChange} embedded />
                <AgentOptions
                  data={state.data}
                  onChange={handleChange}
                  agents={state.agents}
                  profiles={state.profiles}
                  dockerAvailable={state.dockerAvailable}
                  onApplyProfileDefaults={handleApplyProfileDefaults}
                  commandMaps={commandMaps}
                />
              </div>
            )}
          </div>
        </div>
        <div className="px-5 py-4 border-t border-surface-700/20">
          <LaunchFooter
            data={state.data}
            isSubmitting={state.isSubmitting}
            error={state.error}
            onSubmit={handleSubmit}
          />
        </div>
      </div>
      {globConfirm && (
        <VolumeIgnoresGlobDialog globs={globConfirm.globs} onConfirm={handleGlobConfirm} onCancel={handleGlobCancel} />
      )}
      {hooksTrust && (
        <HooksTrustDialog
          onCreate={hooksTrust.info.onCreate}
          onLaunch={hooksTrust.info.onLaunch}
          onDestroy={hooksTrust.info.onDestroy}
          needsMcpTrust={hooksTrust.info.needsMcpTrust}
          onConfirm={handleHooksTrustConfirm}
          onCancel={handleHooksTrustCancel}
        />
      )}
    </div>
  );
}
