import { useCallback, useEffect, useState } from "react";
import { fetchProjects } from "../lib/api";
import type { ProjectInfo } from "../lib/types";
import { listen } from "./domEvents";

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
    return listen(onFocus, [window, "focus"], [document, "visibilitychange"]);
  }, [refresh]);

  return { projects, refresh };
}
