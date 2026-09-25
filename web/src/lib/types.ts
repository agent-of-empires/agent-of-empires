import type { ProjectResponse as ProjectInfo, SessionResponse as WireSessionResponse } from "./apiWire";
import type { RepoColor } from "./repoAppearance";

// The REST contract is generated from the Rust types into `apiWire.ts`, and
// re-exported here under the names the dashboard already used, so callers keep
// importing from the module they always did.
export type {
  AcpWorkerState,
  AgentInfo,
  AgentKind,
  BrowseResponse,
  ClaudeSessionSummary,
  CleanupDefaults,
  ContextResumeAvailability,
  ContextResumeIndeterminateReason,
  ContextResumeUnavailableReason,
  DirEntry,
  GroupInfo,
  PlanSummary,
  ProfileInfo,
  ProjectOverrides,
  RepoBase,
  RichDiffFilesResponse,
  RichDiffStatus,
  RichFileContentsResponse,
  WorkspaceRepoSummary,
} from "./apiWire";
export type {
  CreateSessionBody as CreateSessionRequest,
  DockerStatus as DockerStatusResponse,
  FieldDescriptor as SettingsFieldDescriptor,
  ObjectFieldDescriptor as SettingsObjectField,
  ObjectFieldWidget as SettingsObjectFieldWidget,
  OptionSource as SettingsOptionSource,
  ProjectResponse as ProjectInfo,
  RichDiffFileInfo as RichDiffFile,
  SelectOption as SettingsSelectOption,
  ValidationKind as SettingsValidation,
  WebWritePolicy as SettingsWebWritePolicy,
  WidgetKind as SettingsWidget,
} from "./apiWire";

/** One session as the daemon reports it. Generated from `src/daemon/wire.rs`
 *  into `apiWire.ts`; `status` is narrowed here to the values this dashboard
 *  renders, because the wire carries a plain string so that a client older
 *  than a status still parses the row. */
export type SessionResponse = Omit<WireSessionResponse, "status"> & {
  status: SessionStatus;
};

export type SessionStatus =
  | "Running"
  | "Waiting"
  | "Idle"
  | "Error"
  | "Starting"
  | "Stopped"
  | "Unknown"
  | "Deleting"
  | "Creating";

// The rest of this file is what the wire cannot describe: view models this
// dashboard derives from the rows it fetched, and the one response that is an
// open map on the Rust side.

/** Produced by `diffPair`, not by the daemon: the contents endpoint sends a
 *  unified patch and the client diffs it. */
export interface RichDiffLine {
  type: "add" | "delete" | "equal";
  old_line_num: number | null;
  new_line_num: number | null;
  content: string;
}

export interface RichDiffHunk {
  old_start: number;
  old_lines: number;
  new_start: number;
  new_lines: number;
  lines: RichDiffLine[];
}

export type WorkspaceStatus = "active" | "idle";

/** Sidebar grouping, computed client-side from the session list. */
export interface RepoGroup {
  id: string;
  repoPath: string;
  displayName: string;
  defaultDisplayName: string;
  alias: string | null;
  color: RepoColor | null;
  remoteOwner: string | null;
  /** "owner@host"; null when `remoteOwner` is null. */
  remoteOwnerKey: string | null;
  workspaces: Workspace[];
  status: WorkspaceStatus;
  collapsed: boolean;
  /** Registry entries for this repo path; several when registered in multiple scopes. Entries without workspaces make a pinned-but-empty project. */
  registeredProjects: ProjectInfo[];
}

export interface Workspace {
  id: string;
  branch: string | null;
  projectPath: string;
  displayName: string;
  agents: string[];
  primaryAgent: string;
  status: WorkspaceStatus;
  sessions: SessionResponse[];
}

/** Read out of `ProfileSettingsResponse`; undefined inherits, any array (even empty) overrides. Read-only on the dashboard. */
export interface HooksOverride {
  on_create?: string[];
  on_launch?: string[];
  on_destroy?: string[];
}

/** Serialized `ProfileConfig`, whose overrides are a flattened open map, so
 *  there is no Rust shape to generate. Only the fields the dashboard reads are
 *  typed. */
export interface ProfileSettingsResponse {
  description?: string | null;
  hooks?: HooksOverride;
  [key: string]: unknown;
}
