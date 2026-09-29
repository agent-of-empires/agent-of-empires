import { useCallback, useEffect, useReducer, useRef, useState } from "react";
import { createPortal } from "react-dom";
import type { CreateProgress, CreateSessionRequest, SessionResponse } from "../../lib/types";
import {
  fetchAgents,
  fetchGroups,
  fetchDockerStatus,
  fetchProfiles,
  fetchProjects,
  fetchSettings,
  createSession,
  fetchCreateProgress,
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
import { useMobileKeyboard } from "../../hooks/useMobileKeyboard";
import {
  claimPendingCreate,
  peekPendingCreate,
  registerPendingCreate,
  releasePendingCreate,
  resolvePendingCreate,
  type PendingCreate,
} from "../../lib/pendingCreates";
import { hasFinePointer } from "../../lib/platform";
import { ProjectStep } from "./steps/ProjectStep";
import { ExtraReposPicker } from "./steps/ExtraReposPicker";
import { AgentPanel } from "./steps/AgentPanel";
import { WorktreePanel } from "./steps/WorktreePanel";
import { SandboxPanel } from "./steps/SandboxPanel";
import { ProfilePresetPicker } from "./steps/ProfilePresetPicker";
import { CreateProgressView } from "./CreateProgressView";
import { FieldRow, NavRow, PanelHeader, ROW_INPUT, RowGroup, SwitchRow } from "./WizardRows";
import { LaunchFooter } from "./LaunchFooter";
import { initialData, reducer, type WizardData } from "./wizardReducer";
import { buildCreateRequest } from "./createRequest";
import { commandMapsFromSettings, EMPTY_COMMAND_MAPS, type CommandMaps } from "./commandMaps";
import { profileDefaults, type ProfileDefaults } from "./profileDefaults";

// Validated against ACP_CAPABLE_TOOLS on read, since another install may have written it.
const LAST_USED_TOOL_KEY = "aoe-acp-last-tool";
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

type Panel = "project" | "repos" | "profile" | "agent" | "worktree" | "sandbox";

const PANEL_TITLE: Record<Panel, string> = {
  project: "Project",
  repos: "Extra repos",
  profile: "Profile",
  agent: "Agent",
  worktree: "Worktree",
  sandbox: "Sandbox",
};

// Hook output appears only for creates slow enough to notice.
const PROGRESS_DELAY_MS = 800;
const PROGRESS_POLL_MS = 700;
// Retries after a dropped connection; the idempotency key makes them join the running create.
const NETWORK_RETRIES = 3;
const MAX_RETRY_DELAY_MS = 30_000;
const UNKNOWN_OUTCOME_ERROR =
  "Lost connection while creating. The session may still be created; Launch retries the same request.";

function newIdempotencyKey(): string {
  // randomUUID needs a secure context, which a LAN or tunnel dashboard may not be.
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") return crypto.randomUUID();
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}-${Math.random().toString(36).slice(2)}`;
}

function waitUntilOnline(): Promise<void> {
  if (typeof navigator === "undefined" || navigator.onLine !== false) return Promise.resolve();
  return new Promise((resolve) => window.addEventListener("online", () => resolve(), { once: true }));
}

function waitUntilVisible(): Promise<void> {
  if (typeof document === "undefined" || document.visibilityState === "visible") return Promise.resolve();
  return new Promise((resolve) => {
    const onChange = () => {
      if (document.visibilityState !== "visible") return;
      document.removeEventListener("visibilitychange", onChange);
      resolve();
    };
    document.addEventListener("visibilitychange", onChange);
  });
}

/** Worktree row summary: branch name, attach mode and base. */
function worktreeSummary(data: WizardData): string {
  const name = data.worktreeBranch.trim() || "auto";
  if (data.attachExisting) return `${name}, existing branch`;
  return data.baseBranch.trim() ? `${name}, new from ${data.baseBranch.trim()}` : `${name}, new branch`;
}

const basename = (path: string) => path.replace(/\/+$/, "").split("/").pop() || path;

interface Props {
  onClose: () => void;
  onCreated: (session?: SessionResponse) => void;
  /** A create the user sent to the background finished; the wizard is already closed. */
  onCreatedInBackground?: (session?: SessionResponse) => void;
  prefill?: WizardPrefill;
  /** CityHall client mode: only a title is asked; the server derives the rest. */
  nameOnly?: boolean;
}

export function SessionWizard({ onClose, onCreated, onCreatedInBackground, prefill, nameOnly = false }: Props) {
  const [state, dispatch] = useReducer(reducer, {
    data: initialWizardData(prefill, nameOnly),
    isSubmitting: false,
    error: null,
    agents: [],
    groups: [],
    profiles: [],
    dockerAvailable: false,
  });

  // No project yet is the TUI's focused Path field: open straight on the picker.
  const [panel, setPanel] = useState<Panel | null>(() =>
    !nameOnly && (prefill?.initialTab || (!state.data.path && !state.data.scratch)) ? "project" : null,
  );
  const [progress, setProgress] = useState<CreateProgress | null>(null);
  const [progressKey, setProgressKey] = useState<string | null>(null);
  const [showProgress, setShowProgress] = useState(false);
  // A create whose response never arrived; Launch and reconnects retry it under its key.
  // Seeded from a create an earlier wizard left unresolved, so Launch retries it under its key.
  const [unknownOutcome, setUnknownOutcome] = useState<PendingCreate | null>(peekPendingCreate);
  const unknownOutcomeRef = useRef(unknownOutcome);
  useEffect(() => {
    unknownOutcomeRef.current = unknownOutcome;
  }, [unknownOutcome]);
  // Set once the user leaves a create running, so its outcome is reported, not navigated to.
  const backgroundRef = useRef(false);
  const { keyboardHeight } = useMobileKeyboard();

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

  useEffect(() => {
    if (!progressKey) return;
    let cancelled = false;
    const delay = setTimeout(() => !cancelled && setShowProgress(true), PROGRESS_DELAY_MS);
    const poll = setInterval(() => {
      void fetchCreateProgress(progressKey).then((p) => {
        if (!cancelled && p) setProgress(p);
      });
    }, PROGRESS_POLL_MS);
    return () => {
      cancelled = true;
      clearTimeout(delay);
      clearInterval(poll);
      setShowProgress(false);
      setProgress(null);
    };
  }, [progressKey]);

  const handleProfileChange = async (profileName: string) => {
    const d = state.data;
    // A hand-set view is not in `profileDirty`, but the profile's view default would replace it.
    if ((d.profileDirty || d.structuredViewDirty) && profileName) {
      const ok = window.confirm("Selecting a profile will reset your settings to that profile's defaults. Continue?");
      if (!ok) return;
    }
    handleChange("profile", profileName);
    setPanel(null);
    if (!profileName) return;
    try {
      const settings = await fetchSettings(profileName);
      if (settings) {
        handleApplyProfileDefaults({
          ...profileDefaults(settings, "", d.tool),
          resetStructuredViewDirty: true,
          commandMaps: commandMapsFromSettings(settings),
        });
      }
    } catch {
      // Keep just the profile name.
    }
  };

  const runCreate = async (body: CreateSessionRequest, tool: string, since = Date.now()) => {
    setUnknownOutcome(null);
    setProgressKey(body.idempotency_key ?? null);
    const key = body.idempotency_key;
    // Recorded before the request goes out, so a reload mid-flight still has the key.
    if (key) registerPendingCreate({ body: { ...body, idempotency_key: key }, tool, since }, { claimed: true });
    let result = await createSession(body);
    for (let attempt = 0; result.network && body.idempotency_key && attempt < NETWORK_RETRIES; attempt++) {
      await waitUntilOnline();
      await waitUntilVisible();
      await new Promise((r) => setTimeout(r, Math.min(1000 * (attempt + 1), MAX_RETRY_DELAY_MS)));
      result = await createSession(body);
    }
    setProgressKey(null);
    const background = backgroundRef.current;
    // No answer is not a refusal: the detached server create may still finish, so
    // keep the key and reconcile under it rather than report a failure.
    if (result.network && key) {
      if (background) {
        // The wizard is gone, so the app-level owner keeps retrying under this key.
        releasePendingCreate(key);
      } else {
        setUnknownOutcome({ body: { ...body, idempotency_key: key }, tool, since });
        dispatch({ type: "SUBMIT_ERROR", error: UNKNOWN_OUTCOME_ERROR });
      }
      return;
    }
    if (key) resolvePendingCreate(key);
    if (result.ok) {
      dispatch({ type: "SUBMIT_SUCCESS" });
      if (ACP_CAPABLE_TOOLS.has(tool)) safeSetItem(LAST_USED_TOOL_KEY, tool);
      safeSetItem(LAST_USED_INSTRUCTION_KEY, body.custom_instruction ?? "");
      if (body.path.startsWith("/")) safeSetItem(LAST_USED_PROJECT_KEY, body.path);
      for (const w of result.session?.warnings ?? []) toastBus.handler?.error(w);
      if (background) onCreatedInBackground?.(result.session);
      else onCreated(result.session);
    } else if (background) {
      toastBus.handler?.error(`Session was not created: ${result.error || "Unknown error"}`);
    } else if (result.hooksNeedTrust && !body.trust_hooks) {
      // The trust_hooks guard stops a loop if the server refuses again after opting in.
      setHooksTrust({ info: result.hooksNeedTrust, body, tool });
    } else {
      dispatch({ type: "SUBMIT_ERROR", error: result.error || "Unknown error" });
    }
  };

  const handleSubmit = async () => {
    dispatch({ type: "SUBMIT_START" });
    if (unknownOutcome) {
      await runCreate(unknownOutcome.body, unknownOutcome.tool, unknownOutcome.since);
      return;
    }
    const d = state.data;
    const body = {
      ...buildCreateRequest(
        d,
        isAcpEligible(
          d.tool,
          state.agents.find((a) => a.name === d.tool),
        ),
      ),
      idempotency_key: newIdempotencyKey(),
    };
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

  // Own the adopted create while open; on close, hand an unresolved create back to the
  // app-level owner. A create still in flight at close stays claimed by its request and
  // reports through the background path.
  useEffect(() => {
    // Setup restores foreground ownership: StrictMode runs setup, cleanup, setup on one mount.
    backgroundRef.current = false;
    const adopted = unknownOutcomeRef.current;
    if (adopted) claimPendingCreate(adopted.body.idempotency_key);
    return () => {
      backgroundRef.current = true;
      if (unknownOutcomeRef.current) releasePendingCreate(unknownOutcomeRef.current.body.idempotency_key);
    };
  }, []);

  // Reconnecting reconciles an unknown outcome without waiting for another Launch.
  useEffect(() => {
    if (!unknownOutcome || state.isSubmitting) return;
    const retry = () => {
      dispatch({ type: "SUBMIT_START" });
      void runCreate(unknownOutcome.body, unknownOutcome.tool, unknownOutcome.since);
    };
    window.addEventListener("online", retry);
    return () => window.removeEventListener("online", retry);
    // runCreate is recreated each render; the outcome and submit state are what matter.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [unknownOutcome, state.isSubmitting]);

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

  const handleBackground = () => {
    backgroundRef.current = true;
    onClose();
  };

  const d = state.data;
  const selectedAgent = state.agents.find((a) => a.name === d.tool);
  const acpCapable = isAcpEligible(d.tool, selectedAgent);
  const isHostOnly = selectedAgent?.host_only ?? false;
  const hostOnlyReason = `${d.tool} runs on the host only`;
  const worktreeBlocked = d.scratch
    ? "not for scratch sessions"
    : !d.pathIsGitRepo
      ? "not a git repository"
      : isHostOnly
        ? hostOnlyReason
        : null;
  const sandboxBlocked = isHostOnly ? hostOnlyReason : !state.dockerAvailable ? "Docker is not running" : null;
  const agentCustomized = !!(d.extraArgs || d.commandOverride || d.customInstruction);
  const openPanel = (next: Panel) => () => setPanel(next);

  const renderPanel = (p: Panel) => {
    switch (p) {
      case "project":
        return (
          <ProjectStep
            data={d}
            onChange={handleChange}
            initialTab={prefill?.initialTab}
            agents={state.agents}
            onSelectSavedProject={(override) => dispatch({ type: "SEED_PROJECT_WORKTREE_OVERRIDE", override })}
            onPicked={() => setPanel(null)}
          />
        );
      case "repos":
        return (
          <ExtraReposPicker
            primaryPath={d.path}
            selectedPaths={d.extraRepoPaths}
            onChange={(paths) => handleChange("extraRepoPaths", paths)}
            repoBases={d.repoBases}
            onRepoBasesChange={(bases) => handleChange("repoBases", bases)}
            basesEnabled={d.useWorktree && !d.attachExisting}
          />
        );
      case "profile":
        return (
          <ProfilePresetPicker
            profiles={state.profiles}
            selected={d.profile}
            dirty={d.profileDirty}
            onSelect={(name) => void handleProfileChange(name)}
          />
        );
      case "agent":
        return <AgentPanel data={d} onChange={handleChange} agents={state.agents} commandMaps={commandMaps} />;
      case "worktree":
        return <WorktreePanel data={d} onChange={handleChange} />;
      case "sandbox":
        return <SandboxPanel data={d} onChange={handleChange} />;
    }
  };

  const titleRow = (
    <FieldRow label="Title" htmlFor="wizard-title">
      <input
        id="wizard-title"
        type="text"
        // A keyboard user lands on the title once the project is known, like the TUI.
        autoFocus={hasFinePointer()}
        value={d.title}
        onChange={(e) => handleChange("title", e.target.value)}
        placeholder="Auto-generated if empty"
        className={ROW_INPUT}
      />
    </FieldRow>
  );

  const form = nameOnly ? (
    <RowGroup>{titleRow}</RowGroup>
  ) : (
    <div className="space-y-4">
      <RowGroup>
        {state.profiles.length > 1 && (
          <NavRow
            label="Profile"
            value={`${d.profile || "Server default"}${d.profile && d.profileDirty ? " (custom)" : ""}`}
            onOpen={openPanel("profile")}
          />
        )}
        <NavRow
          label="Project"
          testId="wizard-project-row"
          value={
            d.scratch ? (
              "Scratch folder"
            ) : d.path ? (
              <>
                {basename(d.path)} <span className="text-text-dim">{d.path}</span>
              </>
            ) : (
              ""
            )
          }
          placeholder="Choose a project"
          highlight={!d.scratch && !d.path}
          onOpen={openPanel("project")}
        />
        {d.path && !d.scratch && (
          <NavRow
            label="Extra repos"
            value={d.extraRepoPaths.map(basename).join(", ")}
            placeholder="none"
            onOpen={openPanel("repos")}
          />
        )}
        {titleRow}
        <NavRow
          label="Agent"
          testId="wizard-agent-row"
          value={`${d.tool}${agentCustomized ? " (customized)" : ""}`}
          onOpen={openPanel("agent")}
        />
      </RowGroup>

      <RowGroup>
        <SwitchRow
          label="Structured"
          switchLabel="Use structured view"
          checked={acpCapable && d.useStructuredView}
          onChange={(v) => handleChange("useStructuredView", v)}
          disabled={!acpCapable}
          summary={
            !acpCapable
              ? selectedAgent?.acp_allowed === false
                ? "not on the allowed agents list"
                : selectedAgent?.kind === "custom"
                  ? "needs agent_acp_cmd"
                  : "no ACP adapter, terminal only"
              : d.useStructuredView
                ? "plan, tool calls and diffs"
                : "raw terminal"
          }
        />
        <SwitchRow
          label="Auto-approve"
          switchLabel="Auto-approve actions"
          checked={d.yoloMode}
          onChange={(v) => handleChange("yoloMode", v)}
          summary="skip permission prompts"
        />
        <SwitchRow
          label="Worktree"
          switchLabel="Create a worktree"
          testId="wizard-worktree-row"
          // A blocked switch left on still shows on and can be turned off; it is what the create sends.
          checked={d.useWorktree}
          onChange={(v) => handleChange("useWorktree", v)}
          disabled={!!worktreeBlocked && !d.useWorktree}
          summary={worktreeBlocked ?? (d.useWorktree ? worktreeSummary(d) : "run in the repo folder")}
          onConfigure={!worktreeBlocked && d.useWorktree ? openPanel("worktree") : undefined}
        />
        <SwitchRow
          label="Sandbox"
          switchLabel="Run in a safe container"
          testId="wizard-sandbox-row"
          checked={d.sandboxEnabled}
          onChange={(v) => handleChange("sandboxEnabled", v)}
          disabled={!!sandboxBlocked && !d.sandboxEnabled}
          summary={sandboxBlocked ?? (d.sandboxEnabled ? d.sandboxImage || "default image" : "run on the host")}
          onConfigure={!sandboxBlocked && d.sandboxEnabled ? openPanel("sandbox") : undefined}
        />
        <FieldRow label="Group" htmlFor="wizard-group">
          <input
            id="wizard-group"
            type="text"
            value={d.group}
            onChange={(e) => handleChange("group", e.target.value)}
            placeholder="Optional"
            list="wizard-groups"
            className={ROW_INPUT}
          />
          <datalist id="wizard-groups">
            {state.groups.map((g) => (
              <option key={g.path} value={g.path} />
            ))}
          </datalist>
        </FieldRow>
      </RowGroup>
    </div>
  );

  const creating = state.isSubmitting && showProgress && !globConfirm && !hooksTrust;

  // Portaled to the body: under the app shell's fixed layers its z-[60] would only rank
  // inside theirs, and body-level z-50 layers (hover tooltips) would paint over it.
  return createPortal(
    <div className="fixed inset-0 z-[60] flex items-end md:items-center md:justify-center">
      <div className="absolute inset-0 bg-black/60" onClick={creating ? undefined : onClose} />
      <div
        data-testid="session-wizard"
        style={{ paddingBottom: keyboardHeight || undefined }}
        // Phones get a bottom sheet sized to its content, so Launch sits under the last row
        // within thumb reach; a long panel grows it to just below the notch and scrolls.
        className="relative w-full max-h-[calc(100dvh-env(safe-area-inset-top))] md:max-w-lg bg-surface-800 border-t border-surface-700/30 rounded-t-lg md:border md:rounded-lg flex flex-col md:max-h-[min(720px,90vh)]"
      >
        <div className="flex items-center justify-between px-4 md:px-5 py-3 border-b border-surface-700/20">
          <h1 className="text-sm font-medium text-text-secondary">{creating ? "Creating session" : "New session"}</h1>
          <button
            onClick={creating ? handleBackground : onClose}
            className="w-8 h-8 flex items-center justify-center text-text-dim hover:text-text-secondary cursor-pointer rounded-md hover:bg-surface-700/50 transition-colors"
            aria-label="Close"
          >
            &times;
          </button>
        </div>
        <div className="flex-1 overflow-y-auto px-4 md:px-5 py-4">
          {creating ? (
            <CreateProgressView progress={progress} />
          ) : panel ? (
            <>
              <PanelHeader title={PANEL_TITLE[panel]} onBack={() => setPanel(null)} />
              {renderPanel(panel)}
            </>
          ) : (
            form
          )}
        </div>
        {!panel || creating ? (
          <div
            className="px-4 md:px-5 pt-3 border-t border-surface-700/20"
            style={{ paddingBottom: keyboardHeight ? "0.75rem" : "max(0.75rem, env(safe-area-inset-bottom))" }}
          >
            <LaunchFooter
              data={d}
              isSubmitting={state.isSubmitting}
              error={state.error ?? (unknownOutcome ? UNKNOWN_OUTCOME_ERROR : null)}
              onSubmit={handleSubmit}
              nameOnly={nameOnly}
              defaultsReady={defaultsReady}
              onBackground={creating && onCreatedInBackground ? handleBackground : undefined}
            />
          </div>
        ) : (
          <div
            className="px-4 md:px-5 pt-3 border-t border-surface-700/20"
            style={{ paddingBottom: keyboardHeight ? "0.75rem" : "max(0.75rem, env(safe-area-inset-bottom))" }}
          >
            <button
              type="button"
              onClick={() => setPanel(null)}
              className="w-full py-2.5 rounded-lg text-sm font-medium text-text-primary bg-surface-700 hover:bg-surface-600 cursor-pointer transition-colors"
            >
              Done
            </button>
          </div>
        )}
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
    </div>,
    document.body,
  );
}
