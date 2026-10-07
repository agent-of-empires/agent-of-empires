import { useCallback, useMemo, useState } from "react";
import type { Workspace } from "../../lib/types";
import { type ActiveFacet, pluginFacetSpecs, sessionMatchesFacets } from "../../lib/pluginUi";
import type { usePluginUiEntries } from "../../lib/pluginUiContext";

const facetKey = (pluginId: string, entryId: string) => `${pluginId}\u0000${entryId}`;

/** Ephemeral plugin facet selection; a selection for a facet missing from the snapshot is ignored, not dropped. */
export function useFacetFilter(entries: ReturnType<typeof usePluginUiEntries>) {
  const facetSpecs = useMemo(() => pluginFacetSpecs(entries), [entries]);
  const [selection, setSelection] = useState<Map<string, Set<string>>>(new Map());

  const toggleValue = useCallback((pluginId: string, entryId: string, value: string) => {
    const key = facetKey(pluginId, entryId);
    setSelection((prev) => {
      const next = new Map(prev);
      const values = new Set(next.get(key));
      if (values.has(value)) values.delete(value);
      else values.add(value);
      if (values.size === 0) next.delete(key);
      else next.set(key, values);
      return next;
    });
  }, []);

  const activeFacets = useMemo<ActiveFacet[]>(
    () =>
      facetSpecs.flatMap((f) => {
        const values = selection.get(facetKey(f.pluginId, f.entryId));
        return values && values.size > 0 ? [{ pluginId: f.pluginId, column: f.column, values }] : [];
      }),
    [facetSpecs, selection],
  );

  const matchesFacets = useCallback(
    (ws: Workspace) =>
      activeFacets.length === 0 || ws.sessions.some((s) => sessionMatchesFacets(entries, s.id, activeFacets)),
    [activeFacets, entries],
  );

  const selectedValues = (pluginId: string, entryId: string) => selection.get(facetKey(pluginId, entryId));

  return { facetSpecs, activeFacets, toggleValue, matchesFacets, selectedValues };
}
