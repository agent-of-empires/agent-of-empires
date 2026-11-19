import { useCallback, useEffect, useRef, useState } from "react";
import {
  dismissSuggestion,
  fetchSuggestions,
  prepareSuggestion,
  refreshSuggestions,
  startSuggestion,
  type StartHooksReview,
  type StartOptions,
  type SuggestionResult,
} from "../lib/api";
import { useServerDown } from "../lib/connectionState";
import { onSuggestionsChanged } from "../lib/suggestionEvents";
import { toastBus } from "../lib/toastBus";
import { STALE_NOTICE, toWizardPrefill } from "../lib/suggestionPrefill";
import type { SuggestionItem, SuggestionsSnapshot } from "../lib/types";
import type { WizardPrefill } from "../components/session-wizard/SessionWizard";

/** A start that failed, kept on its row: what went wrong, what to do, and how to retry. */
export interface StartFailure {
  reason: string;
  hint?: string;
  /** Try again would not help, so only Start with changes is offered. */
  retryable: boolean;
}

/** A Start paused on the repo's hooks, until the user approves or cancels. */
export interface HooksReview {
  item: SuggestionItem;
  review: StartHooksReview;
  options: StartOptions;
}

const REFRESH_POLL_MS = 2000;
const UNDO_MS = 10_000;
export const suggestionKey = (item: SuggestionItem) => `${item.provider_id}:${item.id}`;

/**
 * Snapshot plus the mutations both suggestion views share. Every mutation reloads the snapshot.
 * `start` runs the kind's preset on the server and toasts the Session; `startWithChanges` opens
 * the prefilled wizard instead.
 */
export function useSuggestions(
  onPrepare: (prefill: WizardPrefill) => void,
  onOpenSession?: (sessionId: string) => void,
  // A view that renders nothing for a snapshot has no reason to follow a refreshing provider.
  idleFor?: (snapshot: SuggestionsSnapshot) => boolean,
) {
  const offline = useServerDown();
  const [snapshot, setSnapshot] = useState<SuggestionsSnapshot | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadFailed, setLoadFailed] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [failures, setFailures] = useState<Record<string, StartFailure>>({});
  // Rows being started; the others stay usable meanwhile.
  const [starting, setStarting] = useState<string[]>([]);
  const [hooksReview, setHooksReview] = useState<HooksReview | null>(null);
  // Bumped after every load, success or not, so the refresh poll re-arms itself.
  const [loadTick, setLoadTick] = useState(0);
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
    if (mounted.current) setLoadTick((t) => t + 1);
  }, [apply]);

  // While a provider refreshes, follow it until it answers.
  const refreshingProvider = !!snapshot && !idleFor?.(snapshot) && snapshot.providers.some((p) => p.refreshing);
  useEffect(() => {
    if (!refreshingProvider) return;
    const timer = setTimeout(() => void load(), REFRESH_POLL_MS);
    return () => clearTimeout(timer);
  }, [refreshingProvider, loadTick, load]);

  useEffect(() => {
    mounted.current = true;
    void load();
    return () => {
      mounted.current = false;
    };
  }, [load]);

  useEffect(() => onSuggestionsChanged(() => void load()), [load]);

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

  const clearFailure = (key: string) => setFailures(({ [key]: _gone, ...rest }) => rest);

  // One-click start with the kind's preset. A paused hooks review resumes through `approveHooks`.
  const start = async (item: SuggestionItem, options: StartOptions = {}) => {
    const key = suggestionKey(item);
    setStarting((s) => (s.includes(key) ? s : [...s, key]));
    setNotice(null);
    const result = await startSuggestion(item.provider_id, item.id, options);
    setStarting((s) => s.filter((k) => k !== key));
    if (!mounted.current && result.kind !== "started") return;
    switch (result.kind) {
      case "started": {
        if (mounted.current) {
          clearFailure(key);
          setHooksReview(null);
        }
        const name = result.title || item.title;
        if (onOpenSession && toastBus.handler?.infoAction)
          toastBus.handler.infoAction(`Started \u201c${name}\u201d`, "Open", () => onOpenSession(result.sessionId));
        else toastBus.handler?.info(`Started \u201c${name}\u201d`);
        break;
      }
      case "hooks_need_trust":
        setHooksReview({ item, review: result.review, options });
        return;
      case "trust_changed":
        // Repeating without approval returns a fresh review.
        setHooksReview(null);
        await start(item, { ...options, approve_hooks: undefined, review_fingerprint: undefined });
        return;
      case "failed":
        setHooksReview(null);
        setFailures((f) => ({
          ...f,
          [key]: { reason: result.reason, hint: result.hint, retryable: result.retryable },
        }));
        break;
      case "no_project":
        // Nothing to link to without a choice, so the wizard's project picker takes over.
        await startWithChanges(item);
        return;
      case "stale":
        setHooksReview(null);
        setNotice(STALE_NOTICE);
        break;
      case "error":
        setHooksReview(null);
        setNotice(result.error);
        break;
      // Another start won the link: the reload turns the row into Open.
      case "already_started":
        setHooksReview(null);
        break;
    }
    await load();
  };

  const approveHooks = async () => {
    if (!hooksReview) return;
    const { item, review, options } = hooksReview;
    setHooksReview(null);
    await start(item, { ...options, approve_hooks: true, review_fingerprint: review.review_fingerprint });
  };

  // Opens the New session dialog with the kind's preset filled in. `another` starts a further
  // Session without claiming the suggestion's link.
  const startWithChanges = async (item: SuggestionItem, another = false) => {
    const key = suggestionKey(item);
    setBusy(key);
    setNotice(null);
    const result = await prepareSuggestion(item.provider_id, item.id);
    if (!mounted.current) return;
    if (result.ok) {
      const failure = failures[key];
      clearFailure(key);
      const provider = snapshot?.providers.find((p) => p.id === item.provider_id);
      const label = provider?.name ?? item.provider_id;
      const kindLabel = provider?.sources.find((src) => src.criterion === item.criterion)?.label;
      const link = another ? undefined : { provider_id: item.provider_id, id: item.id };
      setBusy(null);
      onPrepare(
        toWizardPrefill(
          result.data,
          {
            label,
            title: item.title,
            url: item.source_url,
            kindLabel,
            failure: failure && { reason: failure.reason, hint: failure.hint },
          },
          { providerId: item.provider_id, criterion: item.criterion },
          link,
        ),
      );
      return;
    }
    // A stale item is explained by the reload; any other failure stays on its row.
    if (result.stale) setNotice(STALE_NOTICE);
    else setFailures((f) => ({ ...f, [key]: { reason: result.error, retryable: false } }));
    await load();
    if (mounted.current) setBusy(null);
  };

  return {
    snapshot,
    loading,
    loadFailed,
    refreshing,
    busy,
    starting,
    notice,
    failures,
    offline,
    hooksReview,
    load,
    act,
    refresh,
    start,
    startWithChanges,
    approveHooks,
    cancelHooks: () => setHooksReview(null),
  };
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
