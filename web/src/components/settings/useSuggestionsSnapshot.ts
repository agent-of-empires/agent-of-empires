import { useCallback, useEffect, useRef, useState } from "react";
import { fetchSuggestions } from "../../lib/api";
import type { SuggestionsSnapshot } from "../../lib/types";

/** One shared snapshot for the Suggestions tab gate and page; the newest `reload` wins. */
export function useSuggestionsSnapshot() {
  const [snapshot, setSnapshot] = useState<SuggestionsSnapshot | null>(null);
  const [failed, setFailed] = useState(false);
  const seq = useRef(0);
  const mounted = useRef(true);

  const reload = useCallback(async () => {
    const mine = ++seq.current;
    const data = await fetchSuggestions();
    if (!mounted.current || mine !== seq.current) return;
    if (data) setSnapshot(data);
    setFailed(!data);
  }, []);

  useEffect(() => {
    mounted.current = true;
    // Deferred so the first load does not set state inside the effect body.
    const first = setTimeout(() => void reload(), 0);
    return () => {
      mounted.current = false;
      clearTimeout(first);
    };
  }, [reload]);

  return { snapshot, failed, reload };
}
