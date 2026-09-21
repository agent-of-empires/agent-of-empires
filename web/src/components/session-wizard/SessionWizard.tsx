import { useCallback, useEffect, useReducer, useState } from "react";
import type { CreateSessionRequest, SessionResponse } from "../../lib/types";
import {
  fetchAgents,
  fetchGroups,
  fetchDockerStatus,
  fetchProfiles,
  fetchProjects,
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
import { ACP_CAPABLE_TOOLS, isAcpEligible } from "../../lib/acpCapableTools";
import { safeGetItem, safeSetItem } from "../../lib/safeStorage";
import { toastBus } from "../../lib/toastBus";
import { normalizeProjectPathKey } from "../../lib/registeredProjects";
import { ProjectStep } from "./steps/ProjectStep";
import { SessionStep } from "./steps/SessionStep";
import { AgentPickerEssentials } from "./steps/AgentPickerEssentials";
import { AgentOptions } from "./steps/AgentOptions";
import { LaunchFooter } from "./LaunchFooter";
import { initialData, reducer, type WizardData } from "./wizardReducer";
import { buildCreateRequest } from "./createRequest";
import { commandMapsFromSettings, EMPTY_COMMAND_MAPS, type CommandMaps } from "./commandMaps";
import { profileDefaults, type ProfileDefaults } from "./profileDefaults";

// Validated against ACP_CAPABLE_TOOLS on read, since another install may have written it.
const LAST_USED_TOOL_KEY = "aoe-acp-last-tool";
const MORE_OPTIONS_OPEN_KEY = "aoe-new-session-more-options-open";
const LAST_USED_INSTRUCTION_KEY = "aoe-new-session-last-instruction";

// Path of the last launched session, seeded into a plain open. Absolute paths only.
const LAST_USED_PROJECT_KEY = "aoe-new-session-last-project";

function loadLastUsedTool(): string {
  const stored = safeGetItem(LAST_USED_TOOL_KEY);
  return stored && ACP_CAPABLE_TOOLS.has(stored) ? stored : "claude";
}

type Obj = Record<string, unknown> | undefined;

export interface WizardPrefill {
  path?: string;
  tool?: string;
  yoloMode?: boolean;
  sandboxEnabled?: boolean;
  profile?: string;
  group?: string;
  initialTab?: "recent" | "browse" | "clone";
  scratch?: boolean;
  /** The registered project's worktree override for `path`; `undefined` means none. */
  worktreeEnabled?: boolean;
}

function initialWizardData(prefill: WizardPrefill | undefined, nameOnly: boolean): WizardData {
  const lastProject = safeGetItem(LAST_USED_PROJECT_KEY) ?? "";
  const base = {
    ...initialData,
    // A name-only wizard's path is derived server-side, so it is never seeded.
    path: !nameOnly && lastProject.startsWith("/") ? lastProject : "",
    tool: loadLastUsedTool(),
    customInstruction: safeGetItem(LAST_USED_INSTRUCTION_KEY) ?? "",
  };
  if (!prefill) return base;
  return {
    ...base,
    path: prefill.scratch ? "" : prefill.path || "",
    tool: prefill.tool || base.tool,
    yoloMode: prefill.yoloMode ?? false,
    sandboxEnabled: prefill.sandboxEnabled ?? false,
    profile: prefill.profile || "",
    group: prefill.group || "",
    scratch: prefill.scratch ?? false,
    useWorktree: prefill.scratch ? false : (prefill.worktreeEnabled ?? base.useWorktree),
    // Seeded here rather than dispatched so APPLY_PROFILE_DEFAULTS cannot clobber it.
    projectWorktreeOverride: prefill.scratch ? undefined : prefill.worktreeEnabled,
    extraRepoPaths: prefill.scratch ? [] : base.extraRepoPaths,
  };
}

interface Props {
  onClose: () => void;
  onCreated: (session?: SessionResponse) => void;
  prefill?: WizardPrefill;
  /** CityHall client mode: only a title is asked; the server derives the rest. */
  nameOnly?: boolean;
}

export function SessionWizard({ onClose, onCreated, prefill, nameOnly = false }: Props) {
  const [state, dispatch] = useReducer(reducer, {
    data: initialWizardData(prefill, nameOnly),
    isSubmitting: false,
    error: null,
    agents: [],
    groups: [],
    profiles: [],
    dockerAvailable: false,
  });

  const [moreOpen, setMoreOpen] = useState(() => safeGetItem(MORE_OPTIONS_OPEN_KEY) === "true");
  const toggleMoreOpen = useCallback(() => {
    setMoreOpen((open) => {
      safeSetItem(MORE_OPTIONS_OPEN_KEY, open ? "false" : "true");
      return !open;
    });
  }, []);

  // Launch-command preview maps, derived from the settings already fetched.
  const [commandMaps, setCommandMaps] = useState<CommandMaps>(EMPTY_COMMAND_MAPS);
  // Creates paused on a confirm dialog, replayed once the user proceeds.
  const [globConfirm, setGlobConfirm] = useState<{
    globs: VolumeIgnoresGlobPreview[];
    body: CreateSessionRequest;
  } | null>(null);
  const [hooksTrust, setHooksTrust] = useState<{
    info: HooksNeedTrust;
    body: CreateSessionRequest;
    tool: string;
  } | null>(null);
  // A remembered path satisfies the submit gate at mount, so Launch waits for
  // the defaults below rather than sending initialData's sandbox/worktree/yolo.
  // Set on every outcome, so a failed fetch still leaves the form usable.
  const [defaultsReady, setDefaultsReady] = useState(false);

  useEffect(() => {
    fetchAgents().then((a) => dispatch({ type: "SET_AGENTS", agents: a }));
    fetchGroups().then((g) => dispatch({ type: "SET_GROUPS", groups: g }));
    fetchDockerStatus().then((d) => dispatch({ type: "SET_DOCKER", available: d.available }));
    // A remembered or prefilled path is never selected in ProjectStep, so seed its override here.
    const initialPath = state.data.path;
    const projectSeed = initialPath
      ? fetchProjects()
          .then((projects) => {
            const key = normalizeProjectPathKey(initialPath);
            const override = projects.find((p) => normalizeProjectPathKey(p.path) === key)?.overrides?.worktree_enabled;
            if (override !== undefined) {
              dispatch({ type: "SEED_PROJECT_WORKTREE_OVERRIDE", override, path: initialPath });
            }
          })
          .catch(() => {})
      : Promise.resolve();
    // Seed resolved profile defaults: the profile picker is hidden for single-profile users.
    const settingsSeed = fetchProfiles()
      // A failed profiles fetch must not skip settings: an explicit prefill
      // profile, or the unresolved global config, still applies.
      .catch(() => [] as Awaited<ReturnType<typeof fetchProfiles>>)
      .then((p) => {
        dispatch({ type: "SET_PROFILES", profiles: p });
        const effectiveProfile = prefill?.profile || p.find((x) => x.is_default)?.name || "";
        return fetchSettings(effectiveProfile || undefined);
      })
      .then((s) => {
        if (!s) return;
        setCommandMaps(commandMapsFromSettings(s));
        const img = ((s.sandbox as Obj)?.default_image as string) || "";
        if (img) dispatch({ type: "SET_FIELD", field: "sandboxImage", value: img });
        const defaults = profileDefaults(s, prefill?.tool ?? "", state.data.tool);
        dispatch({
          type: "APPLY_PROFILE_DEFAULTS",
          ...defaults,
          // Explicit prefill values win over the profile.
          yoloMode: prefill?.yoloMode ?? defaults.yoloMode,
          sandboxEnabled: prefill?.sandboxEnabled ?? defaults.sandboxEnabled,
          skipIfDirty: true,
        });
      })
      .catch(() => {});
    void Promise.all([settingsSeed, projectSeed]).then(() => setDefaultsReady(true));
    // Seed once; a re-render with a new prefill object must not stomp user edits.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Only a definitive probe answer applies; a failed probe (null) keeps the optimistic default.
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

  const handleApplyProfileDefaults = useCallback((defaults: ProfileDefaults & { commandMaps?: CommandMaps }) => {
    const { commandMaps: maps, ...rest } = defaults;
    if (maps) setCommandMaps(maps);
    dispatch({ type: "APPLY_PROFILE_DEFAULTS", ...rest });
  }, []);

  const runCreate = async (body: CreateSessionRequest, tool: string) => {
    const result = await createSession(body);
    if (result.ok) {
      dispatch({ type: "SUBMIT_SUCCESS" });
      if (ACP_CAPABLE_TOOLS.has(tool)) safeSetItem(LAST_USED_TOOL_KEY, tool);
      safeSetItem(LAST_USED_INSTRUCTION_KEY, body.custom_instruction ?? "");
      if (body.path.startsWith("/")) safeSetItem(LAST_USED_PROJECT_KEY, body.path);
      for (const w of result.session?.warnings ?? []) toastBus.handler?.error(w);
      onCreated(result.session);
    } else if (result.hooksNeedTrust && !body.trust_hooks) {
      // The trust_hooks guard stops a loop if the server refuses again after opting in.
      setHooksTrust({ info: result.hooksNeedTrust, body, tool });
    } else {
      dispatch({ type: "SUBMIT_ERROR", error: result.error || "Unknown error" });
    }
  };

  const handleSubmit = async () => {
    dispatch({ type: "SUBMIT_START" });
    const d = state.data;
    const body = buildCreateRequest(
      d,
      isAcpEligible(
        d.tool,
        state.agents.find((a) => a.name === d.tool),
      ),
    );
    // A failed preview counts as nothing to confirm, so it never blocks creation.
    if (d.sandboxEnabled && !d.scratch && d.path) {
      const preview = await fetchVolumeIgnoresPreview(d.path, d.profile || undefined);
      if (preview && !preview.acknowledged && preview.globs.length > 0) {
        setGlobConfirm({ globs: preview.globs, body });
        return;
      }
    }
    await runCreate(body, d.tool);
  };

  const cancelPending = () => {
    setGlobConfirm(null);
    setHooksTrust(null);
    dispatch({ type: "SUBMIT_CANCEL" });
  };

  const handleGlobConfirm = async (dontShowAgain: boolean) => {
    const pending = globConfirm;
    if (!pending) return;
    if (dontShowAgain) await markVolumeIgnoresGlobsAcknowledged();
    setGlobConfirm(null);
    await runCreate(pending.body, state.data.tool);
  };

  const handleHooksTrustConfirm = async () => {
    const pending = hooksTrust;
    if (!pending) return;
    setHooksTrust(null);
    await runCreate({ ...pending.body, trust_hooks: true }, pending.tool);
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
          {!nameOnly && (
            <ProjectStep
              data={state.data}
              onChange={handleChange}
              initialTab={prefill?.initialTab}
              agents={state.agents}
              onSelectSavedProject={(override) => dispatch({ type: "SEED_PROJECT_WORKTREE_OVERRIDE", override })}
            />
          )}

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

          {!nameOnly && (
            <>
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
                    <SessionStep data={state.data} onChange={handleChange} />
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
            </>
          )}
        </div>
        <div className="px-5 py-4 border-t border-surface-700/20">
          <LaunchFooter
            data={state.data}
            isSubmitting={state.isSubmitting}
            error={state.error}
            onSubmit={handleSubmit}
            nameOnly={nameOnly}
            defaultsReady={defaultsReady}
          />
        </div>
      </div>
      {globConfirm && (
        <VolumeIgnoresGlobDialog globs={globConfirm.globs} onConfirm={handleGlobConfirm} onCancel={cancelPending} />
      )}
      {hooksTrust && (
        <HooksTrustDialog
          onCreate={hooksTrust.info.onCreate}
          onLaunch={hooksTrust.info.onLaunch}
          onDestroy={hooksTrust.info.onDestroy}
          needsMcpTrust={hooksTrust.info.needsMcpTrust}
          onConfirm={handleHooksTrustConfirm}
          onCancel={cancelPending}
        />
      )}
    </div>
  );
}
