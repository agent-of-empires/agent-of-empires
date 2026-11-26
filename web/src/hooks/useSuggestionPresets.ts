import { useCallback, useEffect, useRef, useState } from "react";
import { fetchSuggestionPresets } from "../lib/api";
import { onSuggestionsChanged } from "../lib/suggestionEvents";
import type { SuggestionPresetEntry } from "../lib/types";

/** Each kind's preset, reloaded whenever suggestions change (a preset save notifies). */
export function useSuggestionPresets(): (providerId: string, criterion: string) => SuggestionPresetEntry | undefined {
  const [presets, setPresets] = useState<SuggestionPresetEntry[]>([]);
  const mounted = useRef(true);
  const load = useCallback(async () => {
    const list = await fetchSuggestionPresets();
    if (mounted.current && list) setPresets(list);
  }, []);
  useEffect(() => {
    mounted.current = true;
    void load();
    const off = onSuggestionsChanged(() => void load());
    return () => {
      mounted.current = false;
      off();
    };
  }, [load]);
  return (providerId, criterion) => presets.find((p) => p.provider_id === providerId && p.criterion === criterion);
}
