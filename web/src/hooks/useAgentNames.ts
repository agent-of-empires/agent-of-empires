import { useEffect, useState } from "react";
import { fetchAgents } from "../lib/api";

/** Maps an agent id to how copy names it ("claude" becomes "Claude Code"); unknown ids show as is. */
export function useAgentNames(): (id: string) => string {
  const [names, setNames] = useState<Record<string, string>>({});
  useEffect(() => {
    let live = true;
    void fetchAgents().then((agents) => {
      if (live && agents) setNames(Object.fromEntries(agents.map((a) => [a.name, a.display_name])));
    });
    return () => {
      live = false;
    };
  }, []);
  return (id) => names[id] ?? id;
}
