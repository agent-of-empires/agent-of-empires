// Reducer + state shape for `SessionWizard`. Lives in its own file so
// the wizard component file stays a pure component module (the
// `react-refresh/only-export-components` rule fires when a file mixes
// components with other exports). The mount-time profile-defaults
// seeder added in #1142 dispatches APPLY_PROFILE_DEFAULTS the same way
// the picker-driven path in `AgentStep.handleProfileChange` does, so
// keeping the reducer in this file lets us unit-test the merge rules
// without mounting React.

import type { AgentInfo, GroupInfo, ProfileInfo } from "../../lib/types";
import { applyBranchOverride, slugifyBranch } from "./sessionNames";

export interface WizardData {
  path: string;
  title: string;
  worktreeBranch: string;
  worktreeBranchDirty: boolean;
  useWorktree: boolean;
  /** When true, attach to an existing branch's worktree (`create_new_branch: false`
   *  on the API). Mirrors the TUI new-session "Attach to existing branch"
   *  toggle (`src/tui/dialogs/new_session/render.rs:851`). See #969. */
  attachExisting: boolean;
  /** Optional base branch for the new worktree branch. Empty string =
   *  use the project's default branch. Lives under "Advanced" in the
   *  session step. See #948. */
  baseBranch: string;
  group: string;
  tool: string;
  profile: string;
  yoloMode: boolean;
  sandboxEnabled: boolean;
  sandboxImage: string;
  extraEnv: string[];
  /** Additional repo paths to include in the multi-repo workspace.
   *  Free-text paths and registered project paths flow into the same list. */
  extraRepoPaths: string[];
  advancedEnabled: boolean;
  customInstruction: string;
  extraArgs: string;
  commandOverride: string;
  /** Tracks whether the user has manually edited fields after a profile selection */
  profileDirty: boolean;
  /** Scratch-session mode. When true, the wizard skips the project-path
   *  picker, hides the worktree controls, and submits `path: ""` so the
   *  server provisions a fresh directory under `<app_dir>/scratch/<id>/`.
   *  The reducer enforces mutual exclusion bidirectionally: enabling
   *  `scratch` clears `path`/`useWorktree`/`extraRepoPaths`; setting any
   *  of those back to a non-empty value clears `scratch`. */
  scratch: boolean;
  /** Per-session opt-in to structured view rendering for ACP-capable tools.
   *  Defaults true so ACP-capable tools render in the structured view by
   *  default ("ACP tools run in structured view" behavior); the user
   *  can turn it off in AgentStep to launch a tmux/terminal session. The
   *  submit path sends `view: "structured"` only when the tool is
   *  ACP-capable and this flag is set; the server re-validates
   *  capability (src/server/api/sessions.rs). Intentionally not
   *  tracked in `profileDirty` (see SET_FIELD) and not persisted: a
   *  remembered opt-out would silently override the per-session default. */
  useStructuredView: boolean;
  agentModel: string;
  agentEffort: string;
  [key: string]: unknown;
}

export interface WizardState {
  currentStep: number;
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
  | { type: "SET_STEP"; step: number }
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
      tool: string;
      extraEnv: string[];
      agentModel?: string;
      agentEffort?: string;
      /** When true, skip the apply if the user has already edited an
       *  agent-step field. The picker-driven path always sets this false
       *  (the user has already confirmed the overwrite); the mount-time
       *  seeder (#1142) sets it true so a slow /api/settings response
       *  doesn't clobber edits the user already made. */
      skipIfDirty?: boolean;
    };

export const initialData: WizardData = {
  path: "",
  title: "",
  worktreeBranch: "",
  worktreeBranchDirty: false,
  useWorktree: true,
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
  advancedEnabled: false,
  profileDirty: false,
  customInstruction: "",
  extraArgs: "",
  commandOverride: "",
  scratch: false,
  useStructuredView: true,
  agentModel: "",
  agentEffort: "",
};

export function reducer(state: WizardState, action: Action): WizardState {
  switch (action.type) {
    case "SET_FIELD": {
      const newData = { ...state.data, [action.field]: action.value };
      if (action.field === "title" && !state.data.worktreeBranchDirty) {
        newData.worktreeBranch = slugifyBranch(String(action.value));
      }
      if (action.field === "worktreeBranch") {
        const override = applyBranchOverride(String(newData.title), String(action.value));
        newData.worktreeBranch = override.worktreeBranch;
        newData.worktreeBranchDirty = override.worktreeBranchDirty;
      }
      // Scratch mutual exclusion. Enabling scratch clears the path-source
      // fields so a stale "Recent" selection cannot leak into the submit
      // payload; conversely, setting a real path or extra repos turns
      // scratch off so the wizard can never claim both.
      if (action.field === "scratch" && action.value === true) {
        newData.path = "";
        newData.extraRepoPaths = [];
        newData.useWorktree = false;
      }
      if (
        (action.field === "path" && typeof action.value === "string" && action.value.length > 0) ||
        (action.field === "extraRepoPaths" && Array.isArray(action.value) && action.value.length > 0)
      ) {
        newData.scratch = false;
      }
      // Mark dirty whenever the user manually edits an agent-step
      // field. Guarded against `state.data.profile` previously, but the
      // mount-time seeder (#1142) also needs the flag with no profile
      // set: a user who toggles yoloMode before the late /api/settings
      // response resolves would otherwise have their edit stomped, since
      // APPLY_PROFILE_DEFAULTS dispatches with skipIfDirty: true and the
      // no-profile guard would leave profileDirty false. The picker
      // path's window.confirm() also benefits: picking a profile after
      // unprofiled edits now prompts before overwriting.
      if (["yoloMode", "sandboxEnabled", "tool", "extraEnv", "agentModel", "agentEffort"].includes(action.field)) {
        newData.profileDirty = true;
      }
      return { ...state, data: newData, error: null };
    }
    case "SET_STEP":
      return { ...state, currentStep: action.step };
    case "SUBMIT_START":
      return { ...state, isSubmitting: true, error: null };
    case "SUBMIT_ERROR":
      return { ...state, isSubmitting: false, error: action.error };
    case "SUBMIT_SUCCESS":
      return { ...state, isSubmitting: false };
    case "SUBMIT_CANCEL":
      // User backed out of a pre-create confirmation (e.g. the glob
      // volume_ignores modal); re-enable the submit button without an error.
      return { ...state, isSubmitting: false, error: null };
    case "SET_AGENTS":
      return { ...state, agents: action.agents };
    case "SET_GROUPS":
      return { ...state, groups: action.groups };
    case "SET_PROFILES":
      return { ...state, profiles: action.profiles };
    case "SET_DOCKER":
      return { ...state, dockerAvailable: action.available };
    case "APPLY_PROFILE_DEFAULTS":
      // The mount-time seeder (#1142) sets `skipIfDirty` so a slow
      // /api/settings response doesn't clobber edits the user already
      // made. The picker-driven path leaves it false; it has already
      // shown a window.confirm() to the user before dispatching.
      if (action.skipIfDirty && state.data.profileDirty) return state;
      return {
        ...state,
        data: {
          ...state.data,
          yoloMode: action.yoloMode,
          sandboxEnabled: action.sandboxEnabled,
          tool: action.tool || state.data.tool,
          extraEnv: action.extraEnv,
          agentModel: action.agentModel ?? "",
          agentEffort: action.agentEffort ?? "",
          profileDirty: false,
        },
      };
    default:
      return state;
  }
}
