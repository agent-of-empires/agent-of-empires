import { useCallback, useEffect, useReducer, useRef, useState } from "react";
import { createPortal } from "react-dom";
import type {
  CreateProgress,
  CreateSessionRequest,
  SessionResponse,
  CreationTrustFingerprint,
  GroupInfo,
} from "../../lib/types";
import { setAgentNames } from "../../lib/agentDisplayName";
import {
  fetchAgents,
  fetchGroups,
  fetchDockerStatus,
  fetchProfiles,
  fetchProjects,
  fetchSettings,
  createSession,
  reviewCreationTrust,
  fetchCreateBootId,
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
import { normalizeProjectPathKey, overridesForPath } from "../../lib/registeredProjects";
import { useMobileKeyboard } from "../../hooks/useMobileKeyboard";
import {
  claimPendingCreate,
  isPendingCreateExpired,
  PENDING_CREATE_EXPIRED_MESSAGE,
  peekPendingCreate,
  registerPendingCreate,
  releasePendingCreate,
  resolvePendingCreate,
  retryBody,
  type PendingCreate,
} from "../../lib/pendingCreates";
import { hasFinePointer } from "../../lib/platform";
import { ProjectStep } from "./steps/ProjectStep";
import { ExtraReposPicker } from "./steps/ExtraReposPicker";
import { AgentPanel } from "./steps/AgentPanel";
import { WorktreePanel } from "./steps/WorktreePanel";
import { SandboxPanel } from "./steps/SandboxPanel";
import { ProfilePresetPicker } from "./steps/ProfilePresetPicker";
import { GroupCombobox } from "./steps/GroupCombobox";
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
const REGISTRY_UNRESOLVED_ERROR =
  "Could not load this project's settings, so its sandbox and worktree defaults are unknown. Select the project again to retry.";

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
  /** Authority for implicit reads; the machine default is not the served profile. */
  servedProfile?: string;
  onRetryServedProfile?: () => Promise<void>;
  /** CityHall client mode: only a title is asked; the server derives the rest. */
  nameOnly?: boolean;
}

const NO_GROUPS: GroupInfo[] = [];

export function SessionWizard({
  onClose,
  onCreated,
  onCreatedInBackground,
  prefill,
  servedProfile,
  onRetryServedProfile,
  nameOnly = false,
}: Props) {
  const [state, dispatch] = useReducer(reducer, {
    data: initialWizardData(prefill, nameOnly),
    isSubmitting: false,
    error: null,
    agents: [],
    groups: [],
    groupsFor: null,
    profiles: [],
    dockerAvailable: false,
    dockerChecked: false,
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
    fingerprint: CreationTrustFingerprint;
    mcpSummaries: string[];
  } | null>(null);
  const [defaultsProfile, setDefaultsProfile] = useState<string | null>(null);
  const defaultsReady = defaultsProfile !== null && defaultsProfile === (state.data.profile || servedProfile);
  const defaultsGeneration = useRef(0);
  const selectedProfile = useRef<string | null>(null);
  const resetImplicitDefaults = useRef(false);
  const profileSwitchRef = useRef(0);
  const [profileSwitching, setProfileSwitching] = useState(false);
  const [registryUnresolved, setRegistryUnresolved] = useState(false);
  const resolutionRef = useRef(0);
  const knownResolutionRef = useRef<string | null>(null);
  const dataRef = useRef(state.data);
  useEffect(() => {
    dataRef.current = state.data;
  }, [state.data]);
  const [resolvingServedProfile, setResolvingServedProfile] = useState(
    !servedProfile && !prefill?.profile && Boolean(onRetryServedProfile),
  );
  const retryServedProfile = useCallback(async () => {
    if (!onRetryServedProfile) return;
    setResolvingServedProfile(true);
    try {
      await onRetryServedProfile();
    } finally {
      setResolvingServedProfile(false);
    }
  }, [onRetryServedProfile]);
  useEffect(() => {
    if (servedProfile || prefill?.profile || !onRetryServedProfile) return;
    let active = true;
    void onRetryServedProfile().finally(() => {
      if (active) setResolvingServedProfile(false);
    });
    return () => {
      active = false;
    };
  }, [servedProfile, prefill?.profile, onRetryServedProfile]);

  // Groups are per profile, so the list follows the selected one, or the served one, which can
  // change while the Daemon runs. A list is shown only for the profile it was fetched for.
  const groupsKey = state.data.profile || `served:${servedProfile ?? ""}`;
  useEffect(() => {
    let alive = true;
    void fetchGroups(state.data.profile || undefined).then((groups) => {
      if (alive) dispatch({ type: "SET_GROUPS", groups, groupsFor: groupsKey });
    });
    return () => {
      alive = false;
    };
  }, [state.data.profile, groupsKey]);

  useEffect(() => {
    const generations = defaultsGeneration;
    const switches = profileSwitchRef;
    fetchAgents().then((a) => {
      setAgentNames(a);
      dispatch({ type: "SET_AGENTS", agents: a });
    });
    fetchDockerStatus().then((d) => dispatch({ type: "SET_DOCKER", available: d.available }));
    fetchProfiles()
      .then((profiles) => dispatch({ type: "SET_PROFILES", profiles }))
      .catch(() => {});
    return () => {
      generations.current++;
      switches.current++;
    };
  }, []);

  useEffect(() => {
    const generations = defaultsGeneration;
    if (selectedProfile.current !== null) return;
    const effectiveProfile = state.data.profile || servedProfile;
    if (!effectiveProfile) return;
    const generation = ++generations.current;
    const initialPath = state.data.path;
    const initialResolution = resolutionRef.current;
    const resetDefaults = resetImplicitDefaults.current;
    const projectSeed =
      initialPath && !state.data.scratch && !state.data.importAcpSessionId
        ? fetchProjects({ profile: effectiveProfile }).then((projects) => {
            if (generation !== generations.current) return;
            const current = dataRef.current;
            if (
              current.importAcpSessionId ||
              current.scratch ||
              normalizeProjectPathKey(current.path) !== normalizeProjectPathKey(initialPath)
            )
              return;
            if (!projects) {
              if (resolutionRef.current === initialResolution) setRegistryUnresolved(true);
              return;
            }
            const overrides = overridesForPath(projects, initialPath);
            if (overrides?.worktree_enabled !== undefined || overrides?.sandbox_enabled !== undefined) {
              dispatch({ type: "SEED_PROJECT_OVERRIDES", overrides, path: initialPath });
            }
            knownResolutionRef.current = normalizeProjectPathKey(initialPath);
            setRegistryUnresolved(false);
          })
        : Promise.resolve();
    void Promise.all([fetchSettings(effectiveProfile), projectSeed])
      .then(([settings]) => {
        if (generation !== generations.current || !settings) return;
        setCommandMaps(commandMapsFromSettings(settings));
        const image = settings.sandbox?.default_image;
        if (typeof image === "string" && image) dispatch({ type: "SET_FIELD", field: "sandboxImage", value: image });
        const defaults = profileDefaults(settings, prefill?.tool ?? "", dataRef.current.tool);
        dispatch({
          type: "APPLY_PROFILE_DEFAULTS",
          ...defaults,
          yoloMode: resetDefaults ? defaults.yoloMode : (prefill?.yoloMode ?? defaults.yoloMode),
          sandboxEnabled: resetDefaults
            ? defaults.sandboxEnabled
            : (prefill?.sandboxEnabled ?? defaults.sandboxEnabled),
          skipIfDirty: !resetDefaults,
        });
        resetImplicitDefaults.current = false;
      })
      .catch(() => {})
      .finally(() => {
        if (generation === generations.current) setDefaultsProfile(effectiveProfile);
      });
    return () => {
      if (generation === generations.current) generations.current++;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [servedProfile]);

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

  const directEdits = useRef({ sandboxEnabled: 0, useWorktree: 0 });
  const handleChange = useCallback((field: string, value: unknown) => {
    if (field === "sandboxEnabled" || field === "useWorktree") directEdits.current[field]++;
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
    if (d.profileDirty || d.structuredViewDirty) {
      if (!window.confirm("Selecting a profile will reset your settings to that profile's defaults. Continue?")) return;
    }
    setPanel(null);
    const editsAtSelection = { ...directEdits.current };
    dispatch({ type: "RESET_STRUCTURED_VIEW_DIRTY" });
    // Launch waits for this switch, and a response for a superseded profile is dropped.
    const switchId = ++profileSwitchRef.current;
    const isCurrent = () => profileSwitchRef.current === switchId;
    const effectiveProfile = profileName || servedProfile;
    if (!effectiveProfile) {
      selectedProfile.current = null;
      resetImplicitDefaults.current = true;
      handleChange("profile", "");
      setDefaultsProfile(null);
      await retryServedProfile();
      return;
    }
    setProfileSwitching(true);
    try {
      const [projects, settings] = await Promise.all([
        fetchProjects({ profile: effectiveProfile }),
        fetchSettings(effectiveProfile).catch(() => null),
      ]);
      if (!isCurrent()) return;
      const latest = dataRef.current;
      const needsProject = latest.path && !latest.scratch && !latest.importAcpSessionId;
      if (needsProject && !projects) {
        dispatch({
          type: "SUBMIT_ERROR",
          error: "Could not load project settings for profile " + effectiveProfile + ". The profile was not changed.",
        });
        return;
      }
      defaultsGeneration.current++;
      selectedProfile.current = profileName || null;
      resetImplicitDefaults.current = false;
      handleChange("profile", profileName);
      knownResolutionRef.current = needsProject ? normalizeProjectPathKey(latest.path) : null;
      resolutionRef.current++;
      if (needsProject && projects)
        dispatch({
          type: "SEED_PROJECT_OVERRIDES",
          overrides: overridesForPath(projects, latest.path),
          path: latest.path,
        });
      setRegistryUnresolved(false);
      if (settings) {
        handleApplyProfileDefaults({
          ...profileDefaults(settings, "", latest.tool),
          commandMaps: commandMapsFromSettings(settings),
        });
        // The confirmation resets earlier edits, not choices made while this read was pending.
        for (const field of ["sandboxEnabled", "useWorktree"] as const) {
          if (directEdits.current[field] !== editsAtSelection[field]) {
            dispatch({ type: "SET_FIELD", field, value: latest[field] });
          }
        }
      }
      setDefaultsProfile(effectiveProfile);
    } finally {
      if (isCurrent()) setProfileSwitching(false);
    }
  };

  /** Reviews the repository's hooks for a create, showing the trust dialog when it asks
   *  for one, and returns the body to send when the review asks for no trust at all. */
  const openHooksTrust = async (body: CreateSessionRequest, tool: string): Promise<CreateSessionRequest | null> => {
    const review = await reviewCreationTrust({
      path: body.path,
      profile: body.profile,
      scratch: body.scratch,
    });
    if (!review.ok) {
      dispatch({ type: "SUBMIT_ERROR", error: review.error });
      return null;
    }
    if (!review.review.hooks_need_trust && !review.review.mcp_need_trust) {
      return { ...body, trust_hooks: undefined, trust_review: undefined };
    }
    setHooksTrust({
      info: {
        onCreate: review.review.merged_hooks.on_create ?? [],
        onLaunch: review.review.merged_hooks.on_launch ?? [],
        onDestroy: review.review.merged_hooks.on_destroy ?? [],
        needsMcpTrust: review.review.mcp_need_trust,
      },
      body,
      tool,
      fingerprint: review.review.fingerprint,
      mcpSummaries: review.review.mcp_summaries,
    });
    return null;
  };

  // `resume` continues a create whose first send already went out: every send is then
  // a retry naming the daemon run that took the first one.
  const runCreate = async (body: CreateSessionRequest, tool: string, resume?: PendingCreate) => {
    // A create can be sent twice: once, then again after a review that asks for no trust.
    for (let resend = 0; ; resend++) {
      setUnknownOutcome(null);
      setProgressKey(body.idempotency_key ?? null);
      const key = body.idempotency_key;
      const since = resume?.since ?? Date.now();
      // Checked before every send, the first included, since a resumed request can be old
      // and the waits below are unbounded: past the replay window, stop without sending.
      const expired = () => !!key && isPendingCreateExpired(since);
      const giveUp = () => {
        if (key) resolvePendingCreate(key);
        setProgressKey(null);
        if (backgroundRef.current) {
          toastBus.handler?.error(PENDING_CREATE_EXPIRED_MESSAGE);
        } else {
          dispatch({ type: "SUBMIT_ERROR", error: PENDING_CREATE_EXPIRED_MESSAGE });
        }
      };
      if (expired()) return giveUp();
      const origin = resume ? resume.origin : key ? await fetchCreateBootId() : null;
      const pending: PendingCreate | null = key
        ? { body: { ...body, idempotency_key: key }, tool, since, origin }
        : null;
      // Recorded before the request goes out, so a reload mid-flight still has the key.
      if (pending) registerPendingCreate(pending, { claimed: true });
      const send = (retry: boolean) => createSession(pending && retry ? retryBody(pending) : body);
      let result = await send(!!resume);
      for (let attempt = 0; result.network && pending && attempt < NETWORK_RETRIES; attempt++) {
        await waitUntilOnline();
        await waitUntilVisible();
        await new Promise((r) => setTimeout(r, Math.min(1000 * (attempt + 1), MAX_RETRY_DELAY_MS)));
        if (expired()) return giveUp();
        result = await send(true);
      }
      setProgressKey(null);
      const background = backgroundRef.current;
      // No answer is not a refusal: the detached server create may still finish, so
      // keep the key and reconcile under it rather than report a failure.
      if (result.network && pending) {
        if (background) {
          // The wizard is gone, so the app-level owner keeps retrying under this key.
          releasePendingCreate(pending.body.idempotency_key);
        } else {
          setUnknownOutcome(pending);
          dispatch({ type: "SUBMIT_ERROR", error: UNKNOWN_OUTCOME_ERROR });
        }
        return;
      }
      if (key) resolvePendingCreate(key);
      if (result.outcomeUnknown) {
        // The server restarted and cannot say whether the first attempt ran; retrying could
        // run it twice, so this is where it stops.
        if (background) toastBus.handler?.error(result.error ?? "Unknown outcome");
        else dispatch({ type: "SUBMIT_ERROR", error: result.error ?? "Unknown outcome" });
        return;
      }
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
      } else if (result.trustChanged || (result.hooksNeedTrust && !body.trust_hooks)) {
        // A changed hook set is reviewed again from scratch, so the answer to send is dropped.
        const reviewed = await openHooksTrust(
          result.trustChanged ? { ...body, trust_hooks: undefined, trust_review: undefined } : body,
          tool,
        );
        if (!reviewed) return;
        if (resend > 0) {
          dispatch({ type: "SUBMIT_ERROR", error: result.error || "Unknown error" });
          return;
        }
        body = reviewed;
        continue;
      } else {
        dispatch({ type: "SUBMIT_ERROR", error: result.error || "Unknown error" });
      }
      return;
    }
  };

  const handleSubmit = async () => {
    dispatch({ type: "SUBMIT_START" });
    if (unknownOutcome) {
      await runCreate(unknownOutcome.body, unknownOutcome.tool, unknownOutcome);
      return;
    }
    const d = state.data;
    // A seeded default can leave the sandbox on while it is unavailable; the server would reject it.
    if (d.sandboxEnabled && sandboxBlocked) {
      dispatch({ type: "SUBMIT_ERROR", error: `Sandbox unavailable: ${sandboxBlocked}. Turn it off to launch.` });
      return;
    }
    if (d.useWorktree && worktreeBlocked) {
      dispatch({ type: "SUBMIT_ERROR", error: `Worktree unavailable: ${worktreeBlocked}. Turn it off to launch.` });
      return;
    }
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
      void runCreate(unknownOutcome.body, unknownOutcome.tool, unknownOutcome);
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
    await runCreate({ ...pending.body, trust_hooks: true, trust_review: pending.fingerprint }, pending.tool);
  };

  const handleBackground = () => {
    backgroundRef.current = true;
    onClose();
  };

  const d = state.data;
  // A scratch session has no project whose overrides could be unknown.
  const projectUnresolved = registryUnresolved && !d.scratch && !d.importAcpSessionId;
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
  const effectiveProfile = d.profile || servedProfile;

  const renderPanel = (p: Panel) => {
    switch (p) {
      case "project":
        return (
          <ProjectStep
            data={d}
            profile={effectiveProfile}
            onChange={handleChange}
            initialTab={prefill?.initialTab}
            agents={state.agents}
            onSelectSavedProject={(overrides, path) => {
              const key = normalizeProjectPathKey(path);
              if (overrides === null) {
                // Unknown, not "none": a resolution that exists for this very project stays. Otherwise
                // another project's overrides are dropped and Launch waits for this one to load.
                if (key === normalizeProjectPathKey(d.path) && knownResolutionRef.current === key) return;
                knownResolutionRef.current = null;
                dispatch({ type: "SEED_PROJECT_OVERRIDES", overrides: undefined });
                setRegistryUnresolved(true);
                return;
              }
              // Only a successful lookup replaces what an earlier, slower one may still report.
              resolutionRef.current += 1;
              knownResolutionRef.current = key;
              setRegistryUnresolved(false);
              dispatch({ type: "SEED_PROJECT_OVERRIDES", overrides });
            }}
            onPicked={() => setPanel(null)}
          />
        );
      case "repos":
        return (
          <ExtraReposPicker
            profile={effectiveProfile}
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
        {d.structuredViewOffered && (
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
        )}
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
          <GroupCombobox
            id="wizard-group"
            value={d.group}
            onChange={(v) => handleChange("group", v)}
            groups={state.groupsFor === groupsKey ? state.groups : NO_GROUPS}
          />
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
        <div data-wizard-scroll className="flex-1 overflow-y-auto px-4 md:px-5 py-4">
          {!nameOnly && !state.data.profile && !servedProfile && (
            <div
              role="alert"
              className="mb-4 rounded-md border border-status-warning/40 px-3 py-2 text-sm text-text-secondary"
            >
              The server profile could not be loaded. Session defaults cannot be resolved yet.
              <button
                type="button"
                disabled={resolvingServedProfile || !onRetryServedProfile}
                onClick={() => void retryServedProfile()}
                className="ml-2 text-brand-400 hover:underline cursor-pointer disabled:opacity-50"
              >
                Retry server profile
              </button>
            </div>
          )}
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
              error={
                state.error ??
                (projectUnresolved ? REGISTRY_UNRESOLVED_ERROR : null) ??
                (unknownOutcome ? UNKNOWN_OUTCOME_ERROR : null)
              }
              onSubmit={handleSubmit}
              nameOnly={nameOnly}
              defaultsReady={
                defaultsReady && !profileSwitching && !projectUnresolved && !(d.sandboxEnabled && !state.dockerChecked)
              }
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
          mcpSummaries={hooksTrust.mcpSummaries}
          onConfirm={handleHooksTrustConfirm}
          onCancel={cancelPending}
        />
      )}
    </div>,
    document.body,
  );
}
