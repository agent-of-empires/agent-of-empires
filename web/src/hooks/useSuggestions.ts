import { useCallback, useEffect, useRef, useState } from "react";
import {
  dismissSuggestion,
  fetchSuggestions,
  prepareSuggestion,
  refreshSuggestions,
  type SuggestionResult,
} from "../lib/api";
import { useServerDown } from "../lib/connectionState";
import { STALE_NOTICE, toWizardPrefill } from "../lib/suggestionPrefill";
import type { SuggestionItem, SuggestionsSnapshot } from "../lib/types";
import type { WizardPrefill } from "../components/session-wizard/SessionWizard";

const REFRESH_POLL_MS = 2000;
const UNDO_MS = 10_000;

export const suggestionKey = (item: SuggestionItem) => `${item.provider_id}:${item.id}`;

/** Snapshot plus the mutations both suggestion views share. Every mutation reloads the snapshot. */
export function useSuggestions(onPrepare: (prefill: WizardPrefill) => void) {
  const offline = useServerDown();
  const [snapshot, setSnapshot] = useState<SuggestionsSnapshot | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadFailed, setLoadFailed] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [failures, setFailures] = useState<Record<string, string>>({});
  const mounted = useRef(true);
  const loadSeq = useRef(0);
  const failed = useRef(false);

  const apply = useCallback((data: SuggestionsSnapshot | null, seq: number) => {
    if (!mounted.current || seq !== loadSeq.current) return;
    if (data) setSnapshot(data);
    failed.current = !data;
    setLoadFailed(!data);
    setLoading(false);
  }, []);

  const load = useCallback(async () => {
    const seq = ++loadSeq.current;
    apply(await fetchSuggestions(), seq);
  }, [apply]);

  // While a provider refreshes, follow it until it answers.
  const refreshingProvider = snapshot?.providers.some((p) => p.refreshing) ?? false;
  useEffect(() => {
    if (!refreshingProvider) return;
    const timer = setTimeout(() => void load(), REFRESH_POLL_MS);
    return () => clearTimeout(timer);
  }, [refreshingProvider, snapshot, load]);

  useEffect(() => {
    mounted.current = true;
    void load();
    return () => {
      mounted.current = false;
    };
  }, [load]);

  // A fetch that failed while disconnected is retried on reconnect.
  useEffect(() => {
    if (!offline && failed.current) void load();
  }, [offline, load]);

  // Runs a mutation, then reloads; a stale item gets a notice instead of a raw error.
  const act = async (key: string, run: () => Promise<SuggestionResult<unknown>>): Promise<boolean> => {
    setBusy(key);
    setNotice(null);
    const result = await run();
    if (!mounted.current) return false;
    if (!result.ok) setNotice(result.stale ? STALE_NOTICE : result.error);
    await load();
    if (mounted.current) setBusy(null);
    return result.ok;
  };

  const refresh = async () => {
    setRefreshing(true);
    setNotice(null);
    const result = await refreshSuggestions();
    if (!mounted.current) return;
    if (!result.ok) setNotice(result.error);
    await load();
    if (mounted.current) setRefreshing(false);
  };

  // `another` starts a further Session without claiming the suggestion's link.
  const start = async (item: SuggestionItem, another = false) => {
    const key = suggestionKey(item);
    setBusy(key);
    setNotice(null);
    const result = await prepareSuggestion(item.provider_id, item.id);
    if (!mounted.current) return;
    if (result.ok) {
      setBusy(null);
      setFailures(({ [key]: _gone, ...rest }) => rest);
      const label = snapshot?.providers.find((p) => p.id === item.provider_id)?.name ?? item.provider_id;
      onPrepare(
        toWizardPrefill(
          result.data,
          { label, title: item.title, url: item.source_url },
          another ? undefined : { provider_id: item.provider_id, id: item.id },
        ),
      );
      return;
    }
    // A stale item is explained by the reload; any other failure stays on its row.
    if (result.stale) setNotice(STALE_NOTICE);
    else setFailures((f) => ({ ...f, [key]: result.error }));
    await load();
    if (mounted.current) setBusy(null);
  };

  return { snapshot, loading, loadFailed, refreshing, busy, notice, failures, offline, load, act, refresh, start };
}

/** Dismiss that can be undone for 10 s; `act` is the one from {@link useSuggestions}. */
export function useDismissUndo(act: (key: string, run: () => Promise<SuggestionResult<unknown>>) => Promise<boolean>) {
  const [undo, setUndo] = useState<SuggestionItem | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
  useEffect(() => () => clearTimeout(timer.current), []);

  const dismiss = async (item: SuggestionItem, dismissed: boolean) => {
    const ok = await act(suggestionKey(item), () => dismissSuggestion(item.provider_id, item.id, dismissed));
    if (!ok) return;
    clearTimeout(timer.current);
    if (dismissed) {
      setUndo(item);
      timer.current = setTimeout(() => setUndo(null), UNDO_MS);
    } else {
      setUndo(null);
    }
  };
  return { undo, dismiss, clearUndo: () => setUndo(null) };
}
