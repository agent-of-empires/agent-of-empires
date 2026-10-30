import { useCallback, useEffect, useRef, useState } from "react";
import { X } from "lucide-react";
import { dismissSuggestion, fetchSuggestions, prepareSuggestion, refreshSuggestions } from "../lib/api";
import type { SuggestionItem, SuggestionsSnapshot } from "../lib/types";
import { OFFLINE_TITLE, useServerDown } from "../lib/connectionState";
import { useIsWideViewport } from "../hooks/useIsWideViewport";
import { STALE_NOTICE, toWizardPrefill } from "../lib/suggestionPrefill";
import { SuggestionRow } from "./SuggestionRow";
import type { WizardPrefill } from "./session-wizard/SessionWizard";

interface Props {
  readOnly?: boolean;
  onPrepare: (prefill: WizardPrefill) => void;
  onViewAll: () => void;
}

const UNDO_MS = 10_000;
const NOTE = "border-t border-surface-700/40 px-3 py-2 text-xs text-text-dim first:border-t-0";

export function DashboardSuggestions({ readOnly = false, onPrepare, onViewAll }: Props) {
  const offline = useServerDown();
  const wide = useIsWideViewport();
  const [snapshot, setSnapshot] = useState<SuggestionsSnapshot | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [undo, setUndo] = useState<SuggestionItem | null>(null);
  const mounted = useRef(true);
  const undoTimer = useRef<ReturnType<typeof setTimeout>>(undefined);

  const load = useCallback(async () => {
    const data = await fetchSuggestions();
    if (mounted.current && data) setSnapshot(data);
  }, []);

  useEffect(() => {
    mounted.current = true;
    void load();
    return () => {
      mounted.current = false;
      clearTimeout(undoTimer.current);
    };
  }, [load]);

  if (!snapshot || snapshot.providers.length === 0) return null;

  const disabled = readOnly || offline;
  const active = snapshot.items.filter((item) => !item.dismissed);
  const rows = active.slice(0, wide ? 3 : 2);
  const nameOf = (id: string) => snapshot.providers.find((p) => p.id === id)?.name ?? id;
  const pending = snapshot.providers.filter((p) => p.status === "pending");

  const start = async (item: SuggestionItem) => {
    setBusy(`${item.provider_id}:${item.id}`);
    setNotice(null);
    const result = await prepareSuggestion(item.provider_id, item.id);
    if (!mounted.current) return;
    if (result.ok) {
      setBusy(null);
      onPrepare(
        toWizardPrefill(result.data, { label: nameOf(item.provider_id), title: item.title, url: item.source_url }),
      );
      return;
    }
    setNotice(result.stale ? STALE_NOTICE : result.error);
    await load();
    if (mounted.current) setBusy(null);
  };

  const dismiss = async (item: SuggestionItem, dismissed: boolean) => {
    setBusy(`${item.provider_id}:${item.id}`);
    setNotice(null);
    const result = await dismissSuggestion(item.provider_id, item.id, dismissed);
    if (!mounted.current) return;
    if (!result.ok) setNotice(result.stale ? STALE_NOTICE : result.error);
    clearTimeout(undoTimer.current);
    if (result.ok && dismissed) {
      setUndo(item);
      undoTimer.current = setTimeout(() => setUndo(null), UNDO_MS);
    } else {
      setUndo(null);
    }
    await load();
    if (mounted.current) setBusy(null);
  };

  const refresh = async () => {
    setBusy("refresh");
    const result = await refreshSuggestions();
    if (!mounted.current) return;
    if (!result.ok) setNotice(result.error);
    await load();
    if (mounted.current) setBusy(null);
  };

  return (
    <section className="mb-4 mt-3 w-full max-w-2xl" aria-labelledby="suggested-sessions-heading">
      <div className="mb-2 flex items-baseline justify-between px-1">
        <h2
          id="suggested-sessions-heading"
          className="text-[11px] font-mono uppercase tracking-[0.16em] text-text-muted"
        >
          Suggested sessions
        </h2>
        <button type="button" onClick={onViewAll} className="cursor-pointer text-xs text-brand-500 hover:underline">
          View all ({active.length})
        </button>
      </div>
      <ul className="overflow-hidden rounded-lg border border-surface-700/40 bg-surface-900">
        {rows.map((item) => (
          <SuggestionRow
            key={`${item.provider_id}:${item.id}`}
            title={item.title}
            source={nameOf(item.provider_id)}
            reason={item.reason}
            project={item.project}
            actions={
              <>
                <button
                  type="button"
                  onClick={() => void start(item)}
                  disabled={busy !== null || disabled}
                  title={readOnly ? "Read-only: can't create Sessions" : offline ? OFFLINE_TITLE : undefined}
                  className="inline-flex h-8 items-center rounded-md border border-surface-700 bg-surface-800 px-3 text-xs font-medium text-text-primary hover:bg-surface-700/50 disabled:opacity-50"
                >
                  Start
                </button>
                {!readOnly && (
                  <button
                    type="button"
                    aria-label="Dismiss"
                    title="Dismiss"
                    onClick={() => void dismiss(item, true)}
                    disabled={busy !== null || offline}
                    className="inline-flex h-8 w-8 items-center justify-center rounded-md text-text-secondary hover:bg-surface-700/50 disabled:opacity-50"
                  >
                    <X className="h-4 w-4" />
                  </button>
                )}
              </>
            }
          />
        ))}
        {rows.length === 0 && pending.length > 0 && (
          <li className={NOTE}>
            <span aria-hidden="true" className="font-mono text-brand-500">
              ⠙
            </span>{" "}
            Getting your first suggestions from {pending.map((p) => p.name).join(", ")}…
          </li>
        )}
        {rows.length === 0 && pending.length === 0 && (
          <li className={NOTE}>
            Nothing to suggest right now.{" "}
            {!disabled && (
              <button
                type="button"
                onClick={() => void refresh()}
                disabled={busy !== null}
                className="cursor-pointer text-brand-500 hover:underline disabled:opacity-50"
              >
                Refresh
              </button>
            )}
          </li>
        )}
        {readOnly && <li className={NOTE}>This dashboard is read-only. Start, Dismiss and Refresh are off.</li>}
        {offline && (
          <li className={NOTE}>Disconnected. Suggestions show from the last fetch; reconnect to start one.</li>
        )}
      </ul>
      {notice && (
        <p role="alert" className="mt-2 px-1 text-xs text-status-error">
          {notice}
        </p>
      )}
      {undo && (
        <div className="mt-2 w-max max-w-full rounded-md bg-surface-800 px-3 py-1.5 text-[13px] text-text-secondary">
          <span className="truncate">Dismissed &ldquo;{undo.title}&rdquo;. </span>
          <button
            type="button"
            onClick={() => void dismiss(undo, false)}
            disabled={busy !== null || disabled}
            className="cursor-pointer text-brand-500 hover:underline disabled:opacity-50"
          >
            Undo
          </button>
        </div>
      )}
    </section>
  );
}
