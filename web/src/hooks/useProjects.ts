import { useCallback, useEffect, useRef, useState } from "react";
import { fetchAbout, fetchProjects } from "../lib/api";
import type { ProjectInfo } from "../lib/types";

const LOAD_DEADLINE_MS = 15000;

interface ProjectRegistry {
  profile: string | null;
  projects: ProjectInfo[];
  ready: boolean;
}

interface ActiveRead {
  controller: AbortController;
  deadline: ReturnType<typeof setTimeout>;
  finish: () => void;
}

// Rows retain their read profile; unresolved or stale reads never admit mutations.
export function useProjects(): ProjectRegistry & { refresh: () => Promise<void> } {
  const [registry, setRegistry] = useState<ProjectRegistry>({ profile: null, projects: [], ready: false });
  const generation = useRef(0);
  const active = useRef<ActiveRead | null>(null);
  const load = useCallback((): Promise<void> => {
    const request = ++generation.current;
    if (active.current) {
      clearTimeout(active.current.deadline);
      active.current.controller.abort();
      active.current.finish();
    }
    const controller = new AbortController();
    return new Promise<void>((finish) => {
      const deadline = setTimeout(() => {
        if (request !== generation.current) return;
        generation.current++;
        controller.abort();
        active.current = null;
        finish();
      }, LOAD_DEADLINE_MS);
      active.current = { controller, deadline, finish };
      void (async () => {
        try {
          const profile = (await fetchAbout(controller.signal))?.profile;
          if (request !== generation.current || !profile) return;
          setRegistry((current) => (current.profile === profile ? current : { profile, projects: [], ready: false }));
          const projects = await fetchProjects({ profile }, controller.signal);
          if (request !== generation.current || projects === null) return;
          setRegistry({ profile, projects, ready: true });
        } finally {
          clearTimeout(deadline);
          if (request === generation.current) active.current = null;
          finish();
        }
      })();
    });
  }, []);

  const refresh = useCallback(() => {
    setRegistry((current) => ({ ...current, ready: false }));
    return load();
  }, [load]);

  useEffect(() => {
    const requests = generation;
    const reads = active;
    void load();
    const onFocus = () => {
      if (document.visibilityState === "visible") void refresh();
    };
    window.addEventListener("focus", onFocus);
    document.addEventListener("visibilitychange", onFocus);
    return () => {
      requests.current++;
      if (reads.current) {
        clearTimeout(reads.current.deadline);
        reads.current.controller.abort();
        reads.current.finish();
        reads.current = null;
      }
      window.removeEventListener("focus", onFocus);
      document.removeEventListener("visibilitychange", onFocus);
    };
  }, [load, refresh]);

  return { ...registry, refresh };
}
