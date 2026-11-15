import { useEffect, useState } from "react";
import { createProject, fetchProjects } from "../../../lib/api";
import type { ProjectInfo } from "../../../lib/types";
import { DirectoryBrowser } from "../../DirectoryBrowser";
import { CloneRepoForm } from "../../session-wizard/steps/CloneRepoForm";
import { GhostButton, PANEL, Section } from "../ui";

interface Props {
  profile: string;
  onAdded: () => void;
}

export function ProjectStep({ profile, onAdded }: Props) {
  const [projects, setProjects] = useState<ProjectInfo[] | null>(null);
  const [adding, setAdding] = useState(false);
  const [tab, setTab] = useState<"browse" | "clone">("browse");
  const [error, setError] = useState<string | null>(null);

  const [loads, setLoads] = useState(0);

  useEffect(() => {
    let live = true;
    void fetchProjects({ profile: profile || undefined }).then((result) => {
      if (live) setProjects(result ?? []);
    });
    return () => {
      live = false;
    };
  }, [profile, loads]);

  const add = async (path: string) => {
    setError(null);
    const result = await createProject({ path, scope: "global", profile });
    if (!result.ok) {
      setError(result.error ?? "Couldn't add the Project.");
      return;
    }
    setAdding(false);
    setLoads((n) => n + 1);
    onAdded();
  };

  if (projects === null) return <p className="text-sm text-text-dim">Loading Projects…</p>;

  if (projects.length > 0 && !adding) {
    return (
      <Section title="Your Projects" action={<GhostButton onClick={() => setAdding(true)}>Add another</GhostButton>}>
        <div className={PANEL}>
          {projects.map((p) => (
            <div key={`${p.scope}:${p.name}`} className="px-4 py-2.5 flex items-center gap-4">
              <span className="text-sm text-text-primary">{p.name}</span>
              <span className="font-mono text-xs text-text-dim truncate">{p.path}</span>
            </div>
          ))}
        </div>
      </Section>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      <div role="tablist" className="flex gap-1 border-b border-surface-700">
        {(["browse", "clone"] as const).map((id) => (
          <button
            key={id}
            role="tab"
            type="button"
            aria-selected={tab === id}
            onClick={() => setTab(id)}
            className={`h-9 px-3 text-sm cursor-pointer border-b-2 -mb-px ${tab === id ? "border-brand-600 text-text-primary" : "border-transparent text-text-dim hover:text-text-secondary"}`}
          >
            {id === "browse" ? "Browse" : "Clone"}
          </button>
        ))}
      </div>
      {tab === "browse" ? (
        <DirectoryBrowser onSelect={(path) => void add(path)} />
      ) : (
        <CloneRepoForm onCloned={(path) => void add(path)} />
      )}
      {error && <p className="text-xs text-status-error">{error}</p>}
    </div>
  );
}
