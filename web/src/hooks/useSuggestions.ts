import { useCallback, useEffect, useRef, useState } from "react";
import {
  dismissSuggestion,
  createSession,
  fetchCreateBootId,
  fetchSuggestions,
  prepareSuggestion,
  refreshSuggestions,
  type SuggestionResult,
} from "../lib/api";
import { useServerDown } from "../lib/connectionState";
import { onSuggestionsChanged } from "../lib/suggestionEvents";
import {
  newIdempotencyKey,
  peekPendingCreate,
  registerPendingCreate,
  releasePendingCreate,
  resolvePendingCreate,
} from "../lib/pendingCreates";
import { STALE_NOTICE, toWizardPrefill } from "../lib/suggestionPrefill";
import type { SessionResponse, SuggestionItem, SuggestionsSnapshot } from "../lib/types";
import type { WizardPrefill } from "../components/session-wizard/SessionWizard";

/** A start that failed, kept on its row so a retry repeats the same kind of start. */
export interface StartFailure {
  message: string;
  another: boolean;
}

const REFRESH_POLL_MS = 2000;
const UNDO_MS = 10_000;
const PENDING_NOTICE = "A session is still being created. Wait for it to finish.";

export const suggestionKey = (item: SuggestionItem) => `${item.provider_id}:${item.id}`;

/**
 * Snapshot plus the mutations both suggestion views share. Every mutation reloads the snapshot.
 * With "Start without review" on and a resolved, templated draft, `start` creates the session and
 * calls `onStarted`; anything unresolved or needing hook trust opens the wizard through `onPrepare`.
 */
export function useSuggestions(
  onPrepare: (prefill: WizardPrefill) => void,
  onStarted?: (session: SessionResponse) => void,
) {
  const offline = useServerDown();
  const [snapshot, setSnapshot] = useState<SuggestionsSnapshot | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadFailed, setLoadFailed] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [failures, setFailures] = useState<Record<string, StartFailure>>({});
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
  const refreshingProvider = snapshot?.providers.some((p) => p.refreshing) ?? false;
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

  // `another` starts a further Session without claiming the suggestion's link.
  const start = async (item: SuggestionItem, another = false) => {
    const key = suggestionKey(item);
    setBusy(key);
    setNotice(null);
    const result = await prepareSuggestion(item.provider_id, item.id);
    if (!mounted.current) return;
    if (result.ok) {
      setFailures(({ [key]: _gone, ...rest }) => rest);
      const label = snapshot?.providers.find((p) => p.id === item.provider_id)?.name ?? item.provider_id;
      const link = another ? undefined : { provider_id: item.provider_id, id: item.id };
      const prefill = toWizardPrefill(
        result.data,
        { label, title: item.title, url: item.source_url },
        item.provider_id,
        link,
      );
      const draft = result.data;
      // The pref is ignored when a host passes no `onStarted`. Only the user's own template
      // runs unreviewed; the provider's draft prompt always goes through the wizard.
      // "Start another" always opens the wizard, so it never starts unreviewed.
      if (
        onStarted &&
        !another &&
        snapshot?.prefs.start_without_review &&
        draft.project &&
        draft.templated &&
        draft.initial_prompt
      ) {
        if (peekPendingCreate()) {
          setNotice(PENDING_NOTICE);
          setBusy(null);
          return;
        }
        const body = {
          title: draft.title || undefined,
          path: draft.project.path,
          tool: draft.tool,
          yolo_mode: draft.yolo,
          sandbox: draft.sandbox,
          worktree_enabled: draft.worktree,
          initial_prompt: draft.initial_prompt,
          suggestion: link,
          idempotency_key: newIdempotencyKey(),
        };
        const origin = await fetchCreateBootId();
        registerPendingCreate({ body, tool: draft.tool, since: Date.now(), origin }, { claimed: true });
        const created = await createSession(body);
        // No definite answer: the app-level owner keeps reconciling under this key, and a
        // re-click is refused until it does.
        if (created.network || created.outcomeUnknown) releasePendingCreate(body.idempotency_key);
        else resolvePendingCreate(body.idempotency_key);
        if (created.ok && created.session) {
          // Both views navigate away, so the snapshot is not reloaded.
          onStarted(created.session);
          if (mounted.current) setBusy(null);
          return;
        }
        if (!mounted.current) return;
        if (created.network || created.outcomeUnknown || !(created.hooksNeedTrust || created.trustChanged)) {
          setNotice(created.error ?? "Could not start the session.");
          setBusy(null);
          return;
        }
      }
      setBusy(null);
      onPrepare(prefill);
      return;
    }
    // A stale item is explained by the reload; any other failure stays on its row.
    if (result.stale) setNotice(STALE_NOTICE);
    else setFailures((f) => ({ ...f, [key]: { message: result.error, another } }));
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
