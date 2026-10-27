import type { AgentInfo, CreateFailure, GroupInfo, ProfileInfo, ProjectOverrides } from "../../lib/types";
import { normalizeProjectPathKey } from "../../lib/registeredProjects";
import { slugifyBranch } from "./sessionNames";

export interface WizardData {
  path: string;
  title: string;
  worktreeBranch: string;
  worktreeBranchDirty: boolean;
  useWorktree: boolean;
  /** Profile-resolved worktree default, independent of any project override. */
  profileWorktreeDefault: boolean;
  /** The selected saved project's worktree override; outranks `profileWorktreeDefault`. */
  projectWorktreeOverride: boolean | undefined;
  /** Profile-resolved sandbox default, independent of any project override. */
  profileSandboxDefault: boolean;
  /** The selected saved project's sandbox override; outranks `profileSandboxDefault`. */
  projectSandboxOverride: boolean | undefined;
  /** Set only by a direct `sandboxEnabled` edit, so a project override seed does not clobber it. */
  sandboxDirty: boolean;
  /** Set only by a direct `useWorktree` edit, so an unrelated profile-field edit does not block
   *  a project override seed. */
  worktreeDirty: boolean;
  /** Attach to an existing branch's worktree (`create_new_branch: false`). */
  attachExisting: boolean;
  /** Empty means the project's default branch. */
  baseBranch: string;
  group: string;
  tool: string;
  profile: string;
  yoloMode: boolean;
  sandboxEnabled: boolean;
  sandboxImage: string;
  extraEnv: string[];
  extraRepoPaths: string[];
  /** Per extra repo base branch, keyed by path; outranks `baseBranch`. */
  repoBases: Record<string, string>;
  customInstruction: string;
  extraArgs: string;
  commandOverride: string;
  /** Set when the user edits an agent-step field after defaults were applied. */
  profileDirty: boolean;
  /** Mutually exclusive with `path`, `extraRepoPaths` and `useWorktree` (enforced in SET_FIELD). */
  scratch: boolean;
  /** Optimistically true until the wizard's probe says otherwise; false forces `useWorktree` off. */
  pathIsGitRepo: boolean;
  /** Per-session structured view choice, seeded from `acp.default_new_session_view`; deliberately
   *  not persisted or tracked in `profileDirty`. */
  useStructuredView: boolean;
  /** Set by a direct `useStructuredView` edit so mount-time seeding keeps it. */
  structuredViewDirty: boolean;
  agentModel: string;
  agentEffort: string;
  /** Existing native ACP session id to import and resume. */
  importAcpSessionId: string;
  [key: string]: unknown;
}

export interface WizardState {
  data: WizardData;
  isSubmitting: boolean;
  error: string | null;
  /** A create the server refused, shown as the failure box instead of `error`. */
  failure: CreateFailure | null;
  agents: AgentInfo[];
  /** Until "ready", which agents always run in Yolo mode is unknown. */
  agentsStatus: "loading" | "ready" | "failed";
  groups: GroupInfo[];
  /** Which profile `groups` was fetched for; any other profile sees no Groups. */
  groupsFor: string | null;
  profiles: ProfileInfo[];
  dockerAvailable: boolean;
  /** The configured Container runtime's display name; null when the probe failed. */
  dockerRuntimeName: string | null;
  /** The Docker probe has answered, even if the answer was a failure. */
  dockerChecked: boolean;
}

export type Action =
  | { type: "SET_FIELD"; field: string; value: unknown }
  | { type: "SUBMIT_START" }
  | { type: "SUBMIT_ERROR"; error: string }
  | { type: "SUBMIT_FAILED"; failure: CreateFailure }
  | { type: "SUBMIT_SUCCESS" }
  | { type: "SUBMIT_CANCEL" }
  | { type: "SET_AGENTS"; agents: AgentInfo[] }
  | { type: "AGENTS_FAILED" }
  | { type: "SET_GROUPS"; groups: GroupInfo[]; groupsFor: string }
  | { type: "SET_PROFILES"; profiles: ProfileInfo[] }
  | { type: "SET_DOCKER"; available: boolean; runtimeName: string | null }
  | {
      type: "APPLY_PROFILE_DEFAULTS";
      yoloMode: boolean;
      sandboxEnabled: boolean;
      worktreeEnabled: boolean;
      tool: string;
      extraEnv: string[];
      agentModel?: string;
      agentEffort?: string;
      useStructuredView?: boolean;
      /** Mount-time seeding sets this so a late settings response cannot clobber user edits. */
      skipIfDirty?: boolean;
    }
  /** `path`, when set, drops the seed if the selected path has since changed. */
  /** A confirmed profile pick drops the hand-set view; edits made afterward still win over its response. */
  | { type: "RESET_STRUCTURED_VIEW_DIRTY" }
  | { type: "SEED_PROJECT_OVERRIDES"; overrides: ProjectOverrides | undefined; path?: string };

export const initialData: WizardData = {
  path: "",
  title: "",
  worktreeBranch: "",
  worktreeBranchDirty: false,
  // Matches the backend `worktree.enabled` default; seeded from settings on mount.
  useWorktree: false,
  profileWorktreeDefault: false,
  projectWorktreeOverride: undefined,
  profileSandboxDefault: false,
  projectSandboxOverride: undefined,
  sandboxDirty: false,
  worktreeDirty: false,
  attachExisting: false,
  baseBranch: "",
  group: "",
  tool: "claude",
  profile: "",
  yoloMode: false,
  sandboxEnabled: false,
  sandboxImage: "",
  extraEnv: [],
  extraRepoPaths: [],
  repoBases: {},
  profileDirty: false,
  customInstruction: "",
  extraArgs: "",
  commandOverride: "",
  scratch: false,
  pathIsGitRepo: true,
  useStructuredView: true,
  structuredViewDirty: false,
  agentModel: "",
  agentEffort: "",
  importAcpSessionId: "",
};

// Tracked even without a profile so mount-time seeding does not stomp early edits.
const PROFILE_FIELDS = ["yoloMode", "sandboxEnabled", "useWorktree", "tool", "extraEnv", "agentModel", "agentEffort"];

function setField(data: WizardData, field: string, value: unknown): WizardData {
  const next = { ...data, [field]: value };
  if (field === "title" && !data.worktreeBranchDirty && !data.importAcpSessionId) {
    next.worktreeBranch = slugifyBranch(String(value));
  }
  // Any branch edit, even clearing it, stops the title mirror.
  if (field === "worktreeBranch") next.worktreeBranchDirty = true;
  if (field === "scratch" && value === true) {
    Object.assign(next, {
      path: "",
      extraRepoPaths: [],
      repoBases: {},
      useWorktree: false,
      pathIsGitRepo: true,
      importAcpSessionId: "",
      // The project's overrides leave with it; only a manual sandbox choice outlives it.
      projectWorktreeOverride: undefined,
      projectSandboxOverride: undefined,
    });
    if (!data.sandboxDirty) next.sandboxEnabled = data.profileSandboxDefault;
  }
  if (
    (field === "path" && typeof value === "string" && value.length > 0) ||
    (field === "extraRepoPaths" && Array.isArray(value) && value.length > 0)
  ) {
    next.scratch = false;
    // Extra repos each get a new worktree, so they cannot share an attached primary one.
    if (field === "extraRepoPaths") {
      next.attachExisting = false;
      // The attached branch already exists, so it cannot seed a new branch in every repo.
      if (data.attachExisting) Object.assign(next, { worktreeBranch: "", worktreeBranchDirty: false });
    }
    // The import picker dispatches `importAcpSessionId` after `path`, so its own pick survives.
    next.importAcpSessionId = "";
  }
  if (field === "importAcpSessionId" && value) {
    Object.assign(next, {
      scratch: false,
      useWorktree: false,
      sandboxEnabled: false,
      attachExisting: false,
      extraRepoPaths: [],
      repoBases: {},
      worktreeBranch: "",
      baseBranch: "",
      projectWorktreeOverride: undefined,
      projectSandboxOverride: undefined,
      worktreeDirty: false,
      sandboxDirty: false,
    });
  }
  if (field === "path") next.pathIsGitRepo = true;
  if (field === "pathIsGitRepo" && value === false) next.useWorktree = false;
  if (PROFILE_FIELDS.includes(field)) next.profileDirty = true;
  if (field === "useWorktree") next.worktreeDirty = true;
  if (field === "sandboxEnabled") next.sandboxDirty = true;
  if (field === "useStructuredView") next.structuredViewDirty = true;
  return next;
}

/** An import (structured on disk) and a hand-set view outrank the seeded view. */
function seededStructuredView(data: WizardData, action: { useStructuredView?: boolean }): boolean {
  if (data.importAcpSessionId || data.structuredViewDirty) return data.useStructuredView;
  return action.useStructuredView ?? data.useStructuredView;
}

export function reducer(state: WizardState, action: Action): WizardState {
  switch (action.type) {
    case "SET_FIELD":
      return { ...state, data: setField(state.data, action.field, action.value), error: null };
    case "RESET_STRUCTURED_VIEW_DIRTY":
      return { ...state, data: { ...state.data, structuredViewDirty: false } };
    case "SEED_PROJECT_OVERRIDES": {
      if (state.data.importAcpSessionId || state.data.scratch) return state;
      // A manual toggle wins; still record the override for a later profile reset.
      // Same path identity as the registry lookup: `/repo/` and `/repo` are one project.
      if (
        action.path !== undefined &&
        normalizeProjectPathKey(action.path) !== normalizeProjectPathKey(state.data.path)
      )
        return state;
      const projectWorktreeOverride = action.overrides?.worktree_enabled;
      const projectSandboxOverride = action.overrides?.sandbox_enabled;
      const { data } = state;
      const useWorktree = data.worktreeDirty
        ? data.useWorktree
        : data.scratch || data.pathIsGitRepo === false
          ? false
          : (projectWorktreeOverride ?? data.profileWorktreeDefault);
      const sandboxEnabled = data.sandboxDirty
        ? data.sandboxEnabled
        : (projectSandboxOverride ?? data.profileSandboxDefault);
      return {
        ...state,
        data: { ...data, projectWorktreeOverride, projectSandboxOverride, useWorktree, sandboxEnabled },
      };
    }
    case "SUBMIT_START":
      return { ...state, isSubmitting: true, error: null, failure: null };
    case "SUBMIT_ERROR":
      return { ...state, isSubmitting: false, error: action.error, failure: null };
    case "SUBMIT_FAILED":
      return { ...state, isSubmitting: false, error: null, failure: action.failure };
    case "SUBMIT_SUCCESS":
      return { ...state, isSubmitting: false };
    case "SUBMIT_CANCEL":
      return { ...state, isSubmitting: false, error: null, failure: null };
    case "SET_AGENTS":
      return { ...state, agents: action.agents, agentsStatus: "ready" };
    case "AGENTS_FAILED":
      return { ...state, agentsStatus: "failed" };
    case "SET_GROUPS":
      return { ...state, groups: action.groups, groupsFor: action.groupsFor };
    case "SET_PROFILES":
      return { ...state, profiles: action.profiles };
    case "SET_DOCKER":
      return {
        ...state,
        dockerAvailable: action.available,
        dockerRuntimeName: action.runtimeName,
        dockerChecked: true,
      };
    case "APPLY_PROFILE_DEFAULTS": {
      // A remembered or prefilled path can resolve its repo probe before this arrives, so the
      // seeded default must not flip a worktree back on for a scratch session or a non-repo path.
      const resolvedUseWorktree = () =>
        state.data.importAcpSessionId || state.data.scratch || state.data.pathIsGitRepo === false
          ? false
          : (state.data.projectWorktreeOverride ?? action.worktreeEnabled);
      const useStructuredView = seededStructuredView(state.data, action);
      const resolvedSandbox = state.data.importAcpSessionId
        ? false
        : (state.data.projectSandboxOverride ?? action.sandboxEnabled);
      if (action.skipIfDirty && state.data.profileDirty) {
        // Still record the real default, or a later project with no override falls back to `false`.
        // The view is not a profile-tracked field, so it seeds past other edits.
        return {
          ...state,
          data: {
            ...state.data,
            useStructuredView,
            profileWorktreeDefault: action.worktreeEnabled,
            profileSandboxDefault: action.sandboxEnabled,
            useWorktree: state.data.worktreeDirty ? state.data.useWorktree : resolvedUseWorktree(),
            sandboxEnabled: state.data.sandboxDirty ? state.data.sandboxEnabled : resolvedSandbox,
          },
        };
      }
      return {
        ...state,
        data: {
          ...state.data,
          yoloMode: action.yoloMode,
          sandboxEnabled: resolvedSandbox,
          profileSandboxDefault: action.sandboxEnabled,
          useWorktree: resolvedUseWorktree(),
          profileWorktreeDefault: action.worktreeEnabled,
          tool: action.tool || state.data.tool,
          extraEnv: action.extraEnv,
          agentModel: action.agentModel ?? "",
          agentEffort: action.agentEffort ?? "",
          useStructuredView,
          profileDirty: false,
          worktreeDirty: false,
          sandboxDirty: false,
        },
      };
    }
    default:
      return state;
  }
}
