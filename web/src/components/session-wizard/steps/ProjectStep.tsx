import { useState } from "react";
import type { AgentInfo, ClaudeSessionSummary } from "../../../lib/types";
import { DirectoryBrowser } from "../../DirectoryBrowser";
import { ExtraReposPicker } from "./ExtraReposPicker";
import { ClaudeSessionPicker } from "./ClaudeSessionPicker";
import { ProjectSearchList } from "./ProjectSearchList";
import { useProjectPicker } from "./projectPicker";
import { CloneRepoForm } from "./CloneRepoForm";
import { ToggleRow } from "./Toggle";
import type { WizardData } from "../wizardReducer";

type Tab = "recent" | "browse" | "clone" | "import";

interface Props {
  data: WizardData;
  onChange: (field: string, value: unknown) => void;
  initialTab?: Tab;
  /** Only used to gate the Claude import tab. */
  agents?: AgentInfo[];
  /** Called on every path selection with the saved project's worktree override, or `undefined`
   *  when the path is unregistered or has none. */
  onSelectSavedProject?: (override: boolean | undefined) => void;
}

export function ProjectStep({ data, onChange, initialTab, agents = [], onSelectSavedProject }: Props) {
  // Until a tab is picked, show Recent while loading, when there are picks, or
  // when a remembered path is set (so its selection shows); else Browse.
  const [manualTab, setManualTab] = useState<Tab | null>(initialTab ?? null);
  const { loading, saved, query, setQuery, filteredSaved, filteredRecent, hasPicks } = useProjectPicker();
  const activeTab: Tab = manualTab ?? (!loading && !hasPicks && !data.path ? "browse" : "recent");

  // Show the "Selected project" box only when no saved or recent row highlights the path.
  const normalizePath = (p: string) => p.replace(/\/+$/, "") || "/";
  const selectedPath = data.path ? normalizePath(data.path) : "";
  const selectedPathHasRow =
    !!selectedPath &&
    (filteredSaved.some((s) => normalizePath(s.path) === selectedPath) ||
      filteredRecent.some((r) => normalizePath(r.path) === selectedPath));

  // Match against the full saved list: a registered project can fall outside the search filter.
  const selectPath = (path: string) => {
    onChange("path", path);
    const matched = saved.find((p) => normalizePath(p.path) === normalizePath(path));
    onSelectSavedProject?.(matched?.overrides?.worktree_enabled);
  };

  const selectAndShowRecent = (path: string) => {
    selectPath(path);
    setManualTab("recent");
  };

  // Importing resumes via claude-agent-acp, so require both it and the claude CLI.
  const claudeImportAvailable = agents.some((a) => a.name === "claude" && a.installed && a.acp_installed);

  const tabs: { id: Tab; label: string }[] = [
    ...(hasPicks ? [{ id: "recent" as Tab, label: "Recent" }] : []),
    { id: "browse", label: "Browse" },
    { id: "clone", label: "Clone URL" },
    ...(claudeImportAvailable ? [{ id: "import" as Tab, label: "Import from Claude" }] : []),
  ];

  // The on-disk session id only resolves in its recorded cwd, so worktree and scratch are cleared.
  const handleImportSelect = (s: ClaudeSessionSummary) => {
    onChange("scratch", false);
    onChange("path", s.cwd);
    onChange("tool", "claude");
    onChange("useStructuredView", true);
    onChange("useWorktree", false);
    onChange("attachExisting", false);
    onChange("importAcpSessionId", s.session_id);
    if (s.title) onChange("title", s.title.slice(0, 60));
  };

  return (
    <div>
      <h2 className="text-lg font-semibold text-text-primary mb-1">Project folder</h2>
      <p className="text-sm text-text-muted mb-4">Pick a recent project, browse for one, or clone from a URL.</p>

      <ToggleRow
        className="cursor-pointer mb-4"
        title="Skip project folder"
        description="Run the agent in a fresh scratch directory under your AoE app data folder. The folder is removed when you delete the session."
        checked={data.scratch}
        onChange={(v) => onChange("scratch", v)}
        switchLabel="Skip project folder"
      />

      {data.scratch && (
        <div className="px-3 py-2.5 bg-surface-900 border border-brand-600/30 rounded-md">
          <p className="text-[10px] font-mono uppercase tracking-wider text-text-dim mb-1">Scratch session</p>
          <p className="text-sm text-text-primary">
            A fresh scratch directory under your AoE app data folder is created when you launch this session.
          </p>
        </div>
      )}

      {!data.scratch && (
        <>
          {!loading && (
            <div className="flex gap-1 mb-4 border-b border-surface-700/30">
              {tabs.map((tab) => (
                <button
                  key={tab.id}
                  type="button"
                  onClick={() => setManualTab(tab.id)}
                  className={`px-3 py-2 text-sm cursor-pointer transition-colors border-b-2 -mb-px ${
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
              isSelected={(path) => data.path === path}
              onSelect={(path) => selectPath(path)}
              emptyMessage="No projects match that search. Try the Browse tab."
            />
          )}

          {!loading && activeTab === "browse" && <DirectoryBrowser onSelect={selectAndShowRecent} />}

          {!loading && activeTab === "import" && claudeImportAvailable && (
            <ClaudeSessionPicker onSelect={handleImportSelect} selectedSessionId={data.importAcpSessionId} />
          )}

          {!loading && activeTab === "clone" && <CloneRepoForm onCloned={selectAndShowRecent} />}

          {data.path && activeTab !== "browse" && !selectedPathHasRow && (
            <div className="mt-4 px-3 py-2 bg-surface-900 border border-brand-600/30 rounded-md">
              <p className="text-[10px] font-mono uppercase tracking-wider text-text-dim mb-1">Selected project</p>
              <p className="text-sm font-mono text-text-primary truncate">{data.path}</p>
            </div>
          )}

          {data.path && activeTab !== "browse" && (
            <div className="mt-5 pt-4 border-t border-surface-700/30">
              <ExtraReposPicker
                primaryPath={data.path}
                selectedPaths={data.extraRepoPaths}
                onChange={(paths) => onChange("extraRepoPaths", paths)}
                repoBases={data.repoBases}
                onRepoBasesChange={(bases) => onChange("repoBases", bases)}
                basesEnabled={data.useWorktree && !data.attachExisting}
              />
            </div>
          )}
        </>
      )}
    </div>
  );
}
