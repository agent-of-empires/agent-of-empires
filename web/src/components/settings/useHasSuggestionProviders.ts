import { useEffect, useState } from "react";
import { fetchSuggestions } from "../../lib/api";

/** Whether the tab shows: at least one provider plugin is active. */
export function useHasSuggestionProviders(): boolean {
  const [has, setHas] = useState(false);
  useEffect(() => {
    let live = true;
    void fetchSuggestions().then((data) => {
      if (live) setHas((data?.providers.length ?? 0) > 0);
    });
    return () => {
      live = false;
    };
  }, []);
  return has;
}
