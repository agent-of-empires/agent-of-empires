import { createContext, useContext } from "react";

import type { Workspace, WorkspaceRepoSummary } from "./types";

export type SessionRowTagMode = "none" | "auto" | "profile" | "sandbox" | "branch";

export interface SessionRowTag {
  content: string;
  title: string;
  kind: Exclude<SessionRowTagMode, "none">;
}

const DEFAULT_ROW_TAG_MODE: SessionRowTagMode = "branch";
const PROFILE_TAG_WIDTH = 4;
const BRANCH_TAG_WIDTH = 12;

export const SessionRowTagContext = createContext<SessionRowTagMode>(DEFAULT_ROW_TAG_MODE);

export function parseSessionRowTagMode(settings: Record<string, unknown> | null | undefined): SessionRowTagMode {
  const session = settings?.session;
  if (!session || typeof session !== "object") return DEFAULT_ROW_TAG_MODE;
  const raw = (session as Record<string, unknown>).row_tag;
  switch (raw) {
    case "none":
    case "auto":
    case "profile":
    case "sandbox":
    case "branch":
      return raw;
    default:
      return DEFAULT_ROW_TAG_MODE;
  }
}

export function useSessionRowTagMode(): SessionRowTagMode {
  return useContext(SessionRowTagContext);
}

export function computeSessionRowTag(workspace: Workspace, mode: SessionRowTagMode): SessionRowTag | null {
  const primary = workspace.sessions[0];
  if (!primary) return null;

  switch (mode) {
    case "none":
      return null;
    case "auto":
    case "profile": {
      const content = profileShortCode(primary.profile);
      return content ? { content, title: primary.profile, kind: mode } : null;
    }
    case "sandbox":
      return primary.is_sandboxed ? { content: "sb", title: "Sandboxed", kind: mode } : null;
    case "branch":
      return branchRowTag(workspace);
  }
}

function profileShortCode(profile: string): string {
  const segments = profile.split(/[-_]/).filter(Boolean);
  const code =
    segments.length === 0
      ? ""
      : segments.length === 1
        ? Array.from(segments[0]!).slice(0, 3).join("")
        : segments
            .map((segment) => Array.from(segment)[0])
            .filter((char): char is string => !!char)
            .slice(0, PROFILE_TAG_WIDTH)
            .join("");
  return code.toLowerCase();
}

function branchRowTag(workspace: Workspace): SessionRowTag | null {
  const primary = workspace.sessions[0];
  if (!primary) return null;
  const repos = primary.workspace_repos ?? [];
  if (repos.length > 1) {
    const branch = workspace.branch ?? commonWorkspaceBranch(repos);
    if (!branch) return null;
    const content = workspaceBranchTagContent(branch, repos.length);
    return content ? { content, title: `${branch} across ${repos.length} repos`, kind: "branch" } : null;
  }

  const branch = workspace.branch ?? primary.branch;
  if (!branch) return null;
  const content = branchTagContent(branch, BRANCH_TAG_WIDTH);
  return content ? { content, title: branch, kind: "branch" } : null;
}

function commonWorkspaceBranch(repos: WorkspaceRepoSummary[]): string | null {
  const first = repos[0]?.branch;
  if (!first) return null;
  return repos.every((repo) => repo.branch === first) ? first : null;
}

function workspaceBranchTagContent(branch: string, repoCount: number): string | null {
  const suffix = `+${repoCount}`;
  const suffixWidth = Array.from(suffix).length;
  if (suffixWidth >= BRANCH_TAG_WIDTH) {
    return Array.from(suffix).slice(0, BRANCH_TAG_WIDTH).join("");
  }

  const branchWidth = BRANCH_TAG_WIDTH - suffixWidth;
  const content = branchTagContent(branch, branchWidth);
  return content ? `${content}${suffix}` : null;
}

function branchTagContent(branch: string, maxWidth: number): string | null {
  const last = branch.split("/").pop() ?? "";
  const content = Array.from(last).slice(0, maxWidth).join("");
  return content ? content : null;
}
