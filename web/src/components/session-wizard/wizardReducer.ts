import type { AgentInfo, GroupInfo, ProfileInfo } from "../../lib/types";
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
  /** Existing Claude session id to import and resume. */
  importAcpSessionId: string;
  [key: string]: unknown;
}

export interface WizardState {
  data: WizardData;
  isSubmitting: boolean;
  error: string | null;
  agents: AgentInfo[];
  groups: GroupInfo[];
  profiles: ProfileInfo[];
  dockerAvailable: boolean;
}

export type Action =
  | { type: "SET_FIELD"; field: string; value: unknown }
  | { type: "SUBMIT_START" }
  | { type: "SUBMIT_ERROR"; error: string }
  | { type: "SUBMIT_SUCCESS" }
  | { type: "SUBMIT_CANCEL" }
  | { type: "SET_AGENTS"; agents: AgentInfo[] }
  | { type: "SET_GROUPS"; groups: GroupInfo[] }
  | { type: "SET_PROFILES"; profiles: ProfileInfo[] }
  | { type: "SET_DOCKER"; available: boolean }
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
      /** Set by the profile picker, whose overwrite the user has confirmed. */
      resetStructuredViewDirty?: boolean;
      /** Mount-time seeding sets this so a late settings response cannot clobber user edits. */
      skipIfDirty?: boolean;
    }
  /** `path`, when set, drops the seed if the selected path has since changed. */
  | { type: "SEED_PROJECT_WORKTREE_OVERRIDE"; override: boolean | undefined; path?: string };

export const initialData: WizardData = {
  path: "",
  title: "",
  worktreeBranch: "",
  worktreeBranchDirty: false,
  // Matches the backend `worktree.enabled` default; seeded from settings on mount.
  useWorktree: false,
  profileWorktreeDefault: false,
  projectWorktreeOverride: undefined,
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
  if (field === "title" && !data.worktreeBranchDirty) {
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
    });
  }
  if (
    (field === "path" && typeof value === "string" && value.length > 0) ||
    (field === "extraRepoPaths" && Array.isArray(value) && value.length > 0)
  ) {
    next.scratch = false;
    // The import picker dispatches `importAcpSessionId` after `path`, so its own pick survives.
    next.importAcpSessionId = "";
  }
  if (field === "path") next.pathIsGitRepo = true;
  if (field === "pathIsGitRepo" && value === false) next.useWorktree = false;
  if (PROFILE_FIELDS.includes(field)) next.profileDirty = true;
  if (field === "useWorktree") next.worktreeDirty = true;
  if (field === "useStructuredView") next.structuredViewDirty = true;
  return next;
}

/** An import (structured on disk) and a hand-set view outrank the seeded view, except that a
 *  confirmed profile change resets the latter. */
function seededStructuredView(
  data: WizardData,
  action: { useStructuredView?: boolean; resetStructuredViewDirty?: boolean },
): boolean {
  if (data.importAcpSessionId) return data.useStructuredView;
  if (data.structuredViewDirty && !action.resetStructuredViewDirty) return data.useStructuredView;
  return action.useStructuredView ?? data.useStructuredView;
}

export function reducer(state: WizardState, action: Action): WizardState {
  switch (action.type) {
    case "SET_FIELD":
      return { ...state, data: setField(state.data, action.field, action.value), error: null };
    case "SEED_PROJECT_WORKTREE_OVERRIDE": {
      // A manual worktree toggle wins; still record the override for a later profile reset.
      if (action.path !== undefined && action.path !== state.data.path) return state;
      const projectWorktreeOverride = action.override;
      if (state.data.worktreeDirty) {
        return { ...state, data: { ...state.data, projectWorktreeOverride } };
      }
      const useWorktree =
        state.data.scratch || state.data.pathIsGitRepo === false
          ? false
          : (projectWorktreeOverride ?? state.data.profileWorktreeDefault);
      return {
        ...state,
        data: { ...state.data, projectWorktreeOverride, useWorktree },
      };
    }
    case "SUBMIT_START":
      return { ...state, isSubmitting: true, error: null };
    case "SUBMIT_ERROR":
      return { ...state, isSubmitting: false, error: action.error };
    case "SUBMIT_SUCCESS":
      return { ...state, isSubmitting: false };
    case "SUBMIT_CANCEL":
      return { ...state, isSubmitting: false, error: null };
    case "SET_AGENTS":
      return { ...state, agents: action.agents };
    case "SET_GROUPS":
      return { ...state, groups: action.groups };
    case "SET_PROFILES":
      return { ...state, profiles: action.profiles };
    case "SET_DOCKER":
      return { ...state, dockerAvailable: action.available };
    case "APPLY_PROFILE_DEFAULTS": {
      // A remembered or prefilled path can resolve its repo probe before this arrives, so the
      // seeded default must not flip a worktree back on for a scratch session or a non-repo path.
      const resolvedUseWorktree = () =>
        state.data.scratch || state.data.pathIsGitRepo === false
          ? false
          : (state.data.projectWorktreeOverride ?? action.worktreeEnabled);
      const useStructuredView = seededStructuredView(state.data, action);
      if (action.skipIfDirty && state.data.profileDirty) {
        // Still record the real default, or a later project with no override falls back to `false`.
        // The view is not a profile-tracked field, so it seeds past other edits.
        return {
          ...state,
          data: {
            ...state.data,
            useStructuredView,
            profileWorktreeDefault: action.worktreeEnabled,
            useWorktree: state.data.worktreeDirty ? state.data.useWorktree : resolvedUseWorktree(),
          },
        };
      }
      return {
        ...state,
        data: {
          ...state.data,
          yoloMode: action.yoloMode,
          sandboxEnabled: action.sandboxEnabled,
          useWorktree: resolvedUseWorktree(),
          profileWorktreeDefault: action.worktreeEnabled,
          tool: action.tool || state.data.tool,
          extraEnv: action.extraEnv,
          agentModel: action.agentModel ?? "",
          agentEffort: action.agentEffort ?? "",
          useStructuredView,
          structuredViewDirty: action.resetStructuredViewDirty ? false : state.data.structuredViewDirty,
          profileDirty: false,
          worktreeDirty: false,
        },
      };
    }
    default:
      return state;
  }
}
