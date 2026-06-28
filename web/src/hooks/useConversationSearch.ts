import { useEffect, useState } from "react";
import { searchConversations, type ConversationSearchHit } from "../lib/api";

const DEBOUNCE_MS = 200;
const MIN_CHARS = 2;

interface SearchState {
  // The query these results belong to, so a result that resolves after the
  // query changed is suppressed rather than shown under the new query.
  query: string;
  results: ConversationSearchHit[];
  loading: boolean;
}

interface SearchResult {
  results: ConversationSearchHit[];
  loading: boolean;
}

// Debounced full-text search over session conversation content (#2515).
// Returns `loading` while a request is in flight so the palette can show a
// spinner instead of a premature "No matches". An AbortController drops a
// stale response when the query changes, so out-of-order resolution never
// shows results for an old query.
export function useConversationSearch(query: string): SearchResult {
  const [state, setState] = useState<SearchState>({ query: "", results: [], loading: false });
  const normalized = query.trim();
  const enabled = normalized.length >= MIN_CHARS;

  // All setState happens inside async callbacks (the debounce timer and the
  // fetch resolution), never synchronously in the effect body, mirroring the
  // async-only discipline in useSessions so this stays a real external-sync
  // effect rather than a render-cascade. The too-short case is derived at
  // return time, so no effect run is needed to clear results.
  useEffect(() => {
    if (!enabled) return;
    const controller = new AbortController();
    const timer = setTimeout(() => {
      setState({ query: normalized, results: [], loading: true });
      void searchConversations(normalized, controller.signal).then((results) => {
        if (controller.signal.aborted) return;
        setState({ query: normalized, results, loading: false });
      });
    }, DEBOUNCE_MS);

    return () => {
      clearTimeout(timer);
      controller.abort();
    };
  }, [normalized, enabled]);

  // Suppress results that belong to a previous query (the new query's fetch
  // has not settled yet), so stale hits never stay selectable in the palette.
  if (!enabled) return { results: [], loading: false };
  return state.query === normalized
    ? { results: state.results, loading: state.loading }
    : { results: [], loading: state.loading };
}
