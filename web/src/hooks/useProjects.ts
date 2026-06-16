import { useCallback, useEffect, useState } from "react";
import { fetchProjects } from "../lib/api";
import type { ProjectInfo } from "../lib/types";

// Registered projects (the pin registry) drive the sidebar's empty-project
// headers and the pin/unpin controls. Unlike sessions (polled every 3s),
// the registry only changes on an explicit pin/unpin, so this fetches once
// on mount, exposes a `refresh()` the mutation handlers call after a
// pin/unpin, and re-fetches on window focus / tab visibility so a pin made
// in the TUI (or another tab) shows up when the user returns here, without
// a constant poll. See #2047.
export function useProjects(): {
  projects: ProjectInfo[];
  refresh: () => Promise<void>;
} {
  const [projects, setProjects] = useState<ProjectInfo[]>([]);

  const refresh = useCallback(async () => {
    setProjects(await fetchProjects());
  }, []);

  useEffect(() => {
    void fetchProjects().then(setProjects);
    const onFocus = () => {
      if (document.visibilityState === "visible") void fetchProjects().then(setProjects);
    };
    window.addEventListener("focus", onFocus);
    document.addEventListener("visibilitychange", onFocus);
    return () => {
      window.removeEventListener("focus", onFocus);
      document.removeEventListener("visibilitychange", onFocus);
    };
  }, [refresh]);

  return { projects, refresh };
}
