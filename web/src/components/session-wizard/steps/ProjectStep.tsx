import { useState } from "react";
import type { AgentInfo, ClaudeSessionSummary } from "../../../lib/types";
import { DirectoryBrowser } from "../../DirectoryBrowser";
import { ClaudeSessionPicker } from "./ClaudeSessionPicker";
import { CloneRepoForm } from "./CloneRepoForm";
import { ProjectSearchList } from "./ProjectSearchList";
import { useProjectPicker } from "./projectPicker";
import type { WizardData } from "../wizardReducer";

type Tab = "recent" | "browse" | "clone" | "import" | "scratch";

interface Props {
  profile: string | undefined;
  data: WizardData;
  onChange: (field: string, value: unknown) => void;
  initialTab?: Tab;
  /** Built-in + custom agents, used only to gate the Claude import tab.
   *  Optional so render sites that never reach import (and tests) can omit it. */
  agents?: AgentInfo[];
  /** Reports the selected saved project's worktree override, if any. */
  onSelectSavedProject?: (override: boolean | undefined) => void;
  /** Called after any pick, so the wizard can return to its form. */
  onPicked?: () => void;
}

/** The wizard's project panel: recent and saved projects, browse, clone, import, or scratch. */
export function ProjectStep({
  data,
  profile,
  onChange,
  initialTab,
  agents = [],
  onSelectSavedProject,
  onPicked,
}: Props) {
  // Until a tab is picked, show Recent while loading or when there are picks, else Browse.
  const [manualTab, setManualTab] = useState<Tab | null>(initialTab ?? (data.scratch ? "scratch" : null));
  const { loading, error, retry, saved, query, setQuery, filteredSaved, filteredRecent, hasPicks } =
    useProjectPicker(profile);
  const activeTab: Tab = manualTab ?? (!loading && !hasPicks ? "browse" : "recent");

  const normalizePath = (p: string) => p.replace(/\/+$/, "") || "/";

  const selectPath = (path: string) => {
    onChange("path", path);
    const matched = saved.find((project) => normalizePath(project.path) === normalizePath(path));
    onSelectSavedProject?.(matched?.overrides?.worktree_enabled);
    onPicked?.();
  };

  // Claude import needs both the claude CLI and the claude-agent-acp adapter
  // resolvable on the host; gate the tab on both so it never shows when
  // either is missing. See #2276.
  const claudeImportAvailable = agents.some((a) => a.name === "claude" && a.installed && a.acp_installed);

  const tabs: { id: Tab; label: string }[] = [
    ...(hasPicks ? [{ id: "recent" as Tab, label: "Recent" }] : []),
    { id: "browse", label: "Browse" },
    { id: "clone", label: "Clone URL" },
    // Only offer the Claude import when claude and its ACP adapter are both
    // installed; importing resumes via claude-agent-acp, so without it the
    // tab can only ever fail at spawn. See #2276.
    ...(claudeImportAvailable ? [{ id: "import" as Tab, label: "Import from Claude" }] : []),
    { id: "scratch", label: "Scratch" },
  ];

  // #2276: importing an existing Claude Code session prefills the original
  // cwd and forces a structured-view claude session that resumes it. Worktree
  // and scratch are cleared: the on-disk session id only resolves in its
  // recorded cwd.
  const handleImportSelect = (s: ClaudeSessionSummary) => {
    onChange("scratch", false);
    onChange("path", s.cwd);
    onChange("tool", "claude");
    onChange("useStructuredView", true);
    onChange("useWorktree", false);
    onChange("attachExisting", false);
    onChange("importAcpSessionId", s.session_id);
    if (s.title) onChange("title", s.title.slice(0, 60));
    onPicked?.();
  };

  return (
    <div>
      {error && (
        <div
          role="alert"
          className="mb-4 rounded-md border border-status-warning/40 bg-surface-900 px-3 py-2 text-sm text-text-secondary"
        >
          Saved projects could not be loaded. Browse and Clone are still available.
          <button type="button" onClick={retry} className="ml-2 text-brand-400 hover:underline cursor-pointer">
            Retry
          </button>
        </div>
      )}
      {!loading && (
        <div className="flex gap-1 mb-4 border-b border-surface-700/30 overflow-x-auto">
          {tabs.map((tab) => (
            <button
              key={tab.id}
              type="button"
              onClick={() => setManualTab(tab.id)}
              className={`px-3 py-2 text-sm whitespace-nowrap cursor-pointer transition-colors border-b-2 -mb-px ${
                activeTab === tab.id
                  ? "border-brand-600 text-text-primary"
                  : "border-transparent text-text-dim hover:text-text-secondary"
              }`}
            >
              {tab.label}
            </button>
          ))}
        </div>
      )}

      {loading && (
        <div className="animate-pulse space-y-2">
          {[...Array(3)].map((_, i) => (
            <div key={i} className="h-[60px] bg-surface-900 border border-surface-700/40 rounded-md" />
          ))}
        </div>
      )}

      {!loading && activeTab === "recent" && hasPicks && (
        <ProjectSearchList
          query={query}
          onQueryChange={setQuery}
          filteredSaved={filteredSaved}
          filteredRecent={filteredRecent}
          isSelected={(path) => !data.scratch && data.path === path}
          onSelect={(path) => selectPath(path)}
          emptyMessage="No projects match that search. Try the Browse tab."
        />
      )}

      {!loading && activeTab === "browse" && <DirectoryBrowser onSelect={selectPath} />}

      {!loading && activeTab === "import" && claudeImportAvailable && (
        <ClaudeSessionPicker onSelect={handleImportSelect} selectedSessionId={data.importAcpSessionId} />
      )}

      {!loading && activeTab === "clone" && <CloneRepoForm onCloned={selectPath} />}

      {!loading && activeTab === "scratch" && (
        <div className="space-y-3">
          <p className="text-sm text-text-muted">
            Run the agent in a fresh scratch directory under your AoE app data folder. The folder is removed when you
            delete the session.
          </p>
          <button
            type="button"
            onClick={() => {
              onChange("scratch", true);
              onPicked?.();
            }}
            className="px-3 py-2 text-sm rounded-md border border-brand-600 text-text-primary hover:bg-surface-850 cursor-pointer"
          >
            {data.scratch ? "Keep scratch folder" : "Use a scratch folder"}
          </button>
        </div>
      )}
    </div>
  );
}
