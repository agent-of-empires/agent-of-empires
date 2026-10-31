import { useCallback, useEffect, useRef, useState } from "react";
import {
  createSession,
  fetchCreateBootId,
  fetchSuggestions,
  prepareSuggestion,
  refreshSuggestions,
  type SuggestionResult,
} from "../lib/api";
import { useServerDown } from "../lib/connectionState";
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

  const start = async (item: SuggestionItem) => {
    const key = suggestionKey(item);
    setBusy(key);
    setNotice(null);
    const result = await prepareSuggestion(item.provider_id, item.id);
    if (!mounted.current) return;
    if (result.ok) {
      const label = snapshot?.providers.find((p) => p.id === item.provider_id)?.name ?? item.provider_id;
      const prefill = toWizardPrefill(result.data, { label, title: item.title, url: item.source_url });
      const draft = result.data;
      // The pref is ignored when a host passes no `onStarted`. Only the user's own template
      // runs unreviewed; the provider's draft prompt always goes through the wizard.
      if (
        onStarted &&
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
    setNotice(result.stale ? STALE_NOTICE : result.error);
    await load();
    if (mounted.current) setBusy(null);
  };

  return { snapshot, loading, loadFailed, refreshing, busy, notice, offline, load, act, refresh, start };
}
