import { useState } from "react";
import { putSuggestionAutoStart } from "../../lib/api";
import type { SuggestionProvider, SuggestionSource } from "../../lib/types";
import { SuggestionSwitch } from "./suggestionsShared";

const CAPS = [1, 2, 3, 5, 10];
const DEFAULT_CAP = 3;

const BUTTON =
  "inline-flex h-8 items-center rounded-md border border-surface-700 bg-surface-800 px-3 text-xs font-medium text-text-primary hover:bg-surface-700/50 disabled:opacity-50";
const GHOST =
  "inline-flex h-8 items-center rounded-md px-3 text-xs font-medium text-text-secondary hover:bg-surface-800 disabled:opacity-50";

interface Props {
  provider: SuggestionProvider;
  source: SuggestionSource;
  /** The saved preset's yolo mode, which auto started Sessions run with. */
  yolo: boolean;
  disabled: boolean;
  /** The setting changed on the server; reload the snapshot. */
  onChanged: () => Promise<void>;
}

/** The auto start switch of one kind. Turning it on asks first: nobody reads what it starts. */
export function SuggestionAutoStart({ provider, source, yolo, disabled, onChanged }: Props) {
  const current = source.auto_start;
  const [confirming, setConfirming] = useState(false);
  const [cap, setCap] = useState(current?.cap ?? DEFAULT_CAP);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const off = disabled || busy;
  const capOptions = CAPS.includes(cap) ? CAPS : [...CAPS, cap].sort((a, b) => a - b);

  const save = async (enabled: boolean, nextCap: number) => {
    setBusy(true);
    setError(null);
    const result = await putSuggestionAutoStart(provider.id, source.criterion, enabled, nextCap);
    if (!result.ok) setError(result.error);
    else {
      setConfirming(false);
      await onChanged();
    }
    setBusy(false);
  };

  return (
    <section aria-label="Auto start" className="border-t border-surface-700/40 pt-3">
      <div className="flex items-center justify-between gap-4">
        <div className="min-w-0">
          <div className="text-sm text-text-primary">Auto start</div>
          <div className="mt-0.5 text-xs text-text-dim">
            Start new {source.label.toLowerCase()} suggestions without asking. Off by default.
          </div>
        </div>
        <SuggestionSwitch
          checked={!!current || confirming}
          disabled={off}
          label="Auto start"
          onChange={(on) => (on ? setConfirming(true) : current ? void save(false, current.cap) : setConfirming(false))}
        />
      </div>
      {error && (
        <p role="alert" className="mt-2 text-xs text-status-error">
          {error}
        </p>
      )}
      {current && !confirming && (
        <label className="mt-2 flex items-center gap-2 text-xs text-text-secondary">
          At most
          <select
            aria-label="Auto start limit per refresh"
            value={current.cap}
            disabled={off}
            onChange={(e) => void save(true, Number(e.target.value))}
            className="h-8 rounded-md border border-surface-700 bg-surface-800 px-2 text-xs text-text-primary disabled:opacity-50"
          >
            {capOptions.map((n) => (
              <option key={n} value={n}>
                {n}
              </option>
            ))}
          </select>
          per refresh. Each suggestion starts once, even if you dismiss or stop its Session.
        </label>
      )}
      {confirming && (
        <div
          role="alertdialog"
          aria-label="Turn on auto start"
          className="mt-2 rounded-md border border-status-warning/40 bg-status-warning/10 p-3 text-xs text-text-primary"
        >
          <p className="font-medium">
            Auto start launches an agent on text written by others (issue and PR content) with no human check.
          </p>
          {yolo && <p className="mt-1">This kind runs in YOLO mode, so the agent runs without approval prompts.</p>}
          <p className="mt-1 text-text-secondary">
            It starts from the next refresh, with this kind&apos;s preset. A suggestion whose repo is not linked to one
            Project, or whose Project&apos;s hooks need a review, stays a normal suggestion with a note.
          </p>
          <label className="mt-2 flex items-center gap-2">
            At most
            <select
              aria-label="Auto start limit per refresh"
              value={cap}
              disabled={off}
              onChange={(e) => setCap(Number(e.target.value))}
              className="h-8 rounded-md border border-surface-700 bg-surface-800 px-2 text-xs text-text-primary disabled:opacity-50"
            >
              {capOptions.map((n) => (
                <option key={n} value={n}>
                  {n}
                </option>
              ))}
            </select>
            per refresh
          </label>
          <div className="mt-3 flex gap-2">
            <button type="button" className={BUTTON} disabled={off} onClick={() => void save(true, cap)}>
              Turn on auto start
            </button>
            <button type="button" className={GHOST} disabled={busy} onClick={() => setConfirming(false)}>
              Cancel
            </button>
          </div>
        </div>
      )}
    </section>
  );
}
