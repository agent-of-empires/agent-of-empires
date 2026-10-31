import { useState } from "react";
import type { AgentInfo, ImportableSession, ProjectOverrides } from "../../../lib/types";
import { DirectoryBrowser } from "../../DirectoryBrowser";
import { ImportSessionPicker } from "./ImportSessionPicker";
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
  /** Only used to offer agents on the import tab. */
  agents?: AgentInfo[];
  /** Called on every path selection with the saved project's overrides, `undefined` when the path is
   *  unregistered or has none, or `null` when the registry could not be loaded (unknown). */
  onSelectSavedProject?: (overrides: ProjectOverrides | undefined | null, path: string) => void;
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
  const { loading, error, retry, registryAvailable, saved, query, setQuery, filteredSaved, filteredRecent, hasPicks } =
    useProjectPicker(profile);
  const activeTab: Tab = manualTab ?? (!loading && !hasPicks ? "browse" : "recent");

  const normalizePath = (p: string) => p.replace(/\/+$/, "") || "/";

  const selectPath = (path: string) => {
    onChange("path", path);
    const matched = saved.find((project) => normalizePath(project.path) === normalizePath(path));
    // A failed registry request reports `null`: the overrides are unknown, which is not "none".
    onSelectSavedProject?.(registryAvailable ? matched?.overrides : null, path);
    onPicked?.();
  };

  // Import resumes through the built-in ACP adapter, so it needs both the CLI and the adapter.
  const importAgents = agents
    .filter((a) => a.kind === "builtin" && a.acp_capable && a.installed && a.acp_installed && a.acp_allowed !== false)
    .map((a) => a.name);

  const tabs: { id: Tab; label: string }[] = [
    ...(hasPicks ? [{ id: "recent" as Tab, label: "Recent" }] : []),
    { id: "browse", label: "Browse" },
    { id: "clone", label: "Clone URL" },
    ...(importAgents.length > 0 ? [{ id: "import" as Tab, label: "Import session" }] : []),
    { id: "scratch", label: "Scratch" },
  ];

  // Native imports resume in their host cwd, without worktrees, extra repos or a sandbox.
  const handleImportSelect = (s: ImportableSession, agent: string) => {
    onChange("scratch", false);
    onChange("path", s.cwd);
    onChange("tool", agent);
    onChange("useStructuredView", true);
    onChange("useWorktree", false);
    onChange("extraRepoPaths", []);
    onChange("repoBases", {});
    onChange("sandboxEnabled", false);
    onChange("attachExisting", false);
    onChange("importAcpSessionId", s.session_id);
    if (s.title) onChange("title", s.title.slice(0, 60));
    onChange("worktreeBranch", "");
    onChange("baseBranch", "");
    onPicked?.();
  };

  return (
    <div>
      {error && (
        <div
          role="alert"
          className="mb-4 rounded-md border border-status-warning/40 bg-surface-900 px-3 py-2 text-sm text-text-secondary"
        >
          Projects could not be loaded. Browse and Clone are still available.
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

      {!loading &&
        activeTab === "import" &&
        importAgents.length > 0 &&
        (profile ? (
          <ImportSessionPicker
            agents={importAgents}
            profile={profile}
            onSelect={handleImportSelect}
            selectedSessionId={data.importAcpSessionId}
          />
        ) : (
          <p className="text-sm text-text-muted">Resolve the server profile or select a profile to import a session.</p>
        ))}

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
