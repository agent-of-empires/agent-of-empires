import { useEffect, useState } from "react";
import { searchConversations, type ConversationSearchHit } from "../lib/api";

const DEBOUNCE_MS = 200;
const MIN_CHARS = 2;

interface SearchState {
  query: string;
  results: ConversationSearchHit[];
  loading: boolean;
}

interface SearchResult {
  results: ConversationSearchHit[];
  loading: boolean;
}

export function useConversationSearch(query: string): SearchResult {
  const [state, setState] = useState<SearchState>({ query: "", results: [], loading: false });
  const normalized = query.trim();
  const enabled = normalized.length >= MIN_CHARS;

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

  if (!enabled) return { results: [], loading: false };
  return state.query === normalized
    ? { results: state.results, loading: state.loading }
    : { results: [], loading: state.loading };
}
