import { useCallback, useEffect, useRef, useState } from "react";
import { fetchProfiles, fetchProjects } from "../lib/api";
import type { ProjectInfo } from "../lib/types";

interface ProjectRegistry {
  profile: string | null;
  projects: ProjectInfo[];
  ready: boolean;
}

// Rows retain their read profile; unresolved or stale reads never admit mutations.
export function useProjects(): ProjectRegistry & { refresh: () => Promise<void> } {
  const [registry, setRegistry] = useState<ProjectRegistry>({ profile: null, projects: [], ready: false });
  const generation = useRef(0);
  const load = useCallback(async () => {
    const request = ++generation.current;
    const profile = (await fetchProfiles()).find((profile) => profile.is_default)?.name;
    if (request !== generation.current || !profile) return;
    setRegistry((current) => (current.profile === profile ? current : { profile, projects: [], ready: false }));
    const projects = await fetchProjects({ profile });
    if (request !== generation.current || projects === null) return;
    setRegistry({ profile, projects, ready: true });
  }, []);
  const refresh = useCallback(() => {
    setRegistry((current) => ({ ...current, ready: false }));
    return load();
  }, [load]);

  useEffect(() => {
    const requests = generation;
    void load();
    const onFocus = () => {
      if (document.visibilityState === "visible") void refresh();
    };
    window.addEventListener("focus", onFocus);
    document.addEventListener("visibilitychange", onFocus);
    return () => {
      requests.current++;
      window.removeEventListener("focus", onFocus);
      document.removeEventListener("visibilitychange", onFocus);
    };
  }, [load, refresh]);

  return { ...registry, refresh };
}
