import { useEffect, useState } from "react";
import { fetchAgents, putSuggestionPreset, resetSuggestionPreset } from "../../lib/api";
import type {
  AgentInfo,
  SuggestionItem,
  SuggestionPreset,
  SuggestionPresetEntry,
  SuggestionProvider,
  SuggestionSource,
} from "../../lib/types";
import { shortRef } from "../../lib/suggestionPlan";
import { expandTemplate } from "../../lib/suggestionTemplate";
import { TemplateEditor } from "./TemplateEditor";
import { SuggestionSwitch } from "./suggestionsShared";

/** A kind to open in the editor; `returnToSuggestions` swaps Back for a link to the suggestions view. */
export interface PresetFocus {
  providerId: string;
  criterion: string;
  returnToSuggestions?: boolean;
}

const BUTTON =
  "inline-flex h-8 items-center rounded-md border border-surface-700 bg-surface-800 px-3 text-xs font-medium text-text-primary hover:bg-surface-700/50 disabled:opacity-50";
const GHOST =
  "inline-flex h-8 items-center rounded-md px-3 text-xs font-medium text-text-secondary hover:bg-surface-800 disabled:opacity-50";

function Row({ label, hint, children }: { label: string; hint?: string; children: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-4 py-3">
      <div className="min-w-0">
        <div className="text-sm text-text-primary">{label}</div>
        {hint && <div className="mt-0.5 text-xs text-text-dim">{hint}</div>}
      </div>
      {children}
    </div>
  );
}

interface Props {
  provider: SuggestionProvider;
  source: SuggestionSource;
  entry: SuggestionPresetEntry | undefined;
  /** A current item of this kind, for the prompt preview. */
  item: SuggestionItem | undefined;
  disabled: boolean;
  onBack: () => void;
  /** The preset changed on the server; reload the snapshot. */
  onSaved: () => Promise<void>;
}

/** What Start uses for every suggestion of one kind: the New session dialog's rows, in its order. */
export function SuggestionPresetEditor({ provider, source, entry, item, disabled, onBack, onSaved }: Props) {
  const stored = entry?.preset;
  const [draft, setDraft] = useState<SuggestionPreset | null>(stored ?? null);
  const [agents, setAgents] = useState<AgentInfo[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let live = true;
    void fetchAgents().then((list) => {
      if (live && list) setAgents(list.filter((a) => a.installed || a.name === stored?.agent));
    });
    return () => {
      live = false;
    };
  }, [stored?.agent]);

  // The snapshot can arrive after the editor opens; the first value seeds the draft.
  if (draft === null && stored) setDraft(stored);
  if (!draft) {
    return (
      <p role="status" className="py-10 text-center text-sm text-text-dim">
        Loading…
      </p>
    );
  }
  const set = <K extends keyof SuggestionPreset>(key: K, value: SuggestionPreset[K]) =>
    setDraft((d) => (d ? { ...d, [key]: value } : d));
  const save = async () => {
    setBusy(true);
    setError(null);
    const result = await putSuggestionPreset(provider.id, source.criterion, draft);
    if (!result.ok) {
      setError(result.error);
      setBusy(false);
      return;
    }
    await onSaved();
    onBack();
  };
  const reset = async () => {
    setBusy(true);
    setError(null);
    const result = await resetSuggestionPreset(provider.id, source.criterion);
    if (!result.ok) {
      setError(result.error);
      setBusy(false);
      return;
    }
    await onSaved();
    onBack();
  };
  const preview = item
    ? draft.prompt === null
      ? item.draft.prompt
      : draft.prompt === ""
        ? "starts with an empty prompt, so you type the first message."
        : expandTemplate(draft.prompt, item.variables ?? {})
    : "no current suggestion to preview.";
  const off = disabled || busy;

  return (
    <div className="space-y-4">
      <button type="button" onClick={onBack} className="cursor-pointer text-sm text-brand-500">
        &larr; {source.label}
      </button>
      <div>
        <h2 className="text-base font-semibold text-text-primary">Edit &middot; {source.label}</h2>
        <p className="mt-1 text-xs text-text-dim">
          {provider.name}. What Start uses for every {source.label.toLowerCase()} suggestion.
        </p>
      </div>
      {error && (
        <div
          role="alert"
          className="rounded-md border border-status-error/40 bg-status-error/10 p-3 text-sm text-status-error"
        >
          {error}
        </div>
      )}
      <div className="divide-y divide-surface-700/40">
        <Row label="Agent">
          <select
            aria-label="Agent"
            value={draft.agent}
            disabled={off}
            onChange={(e) => set("agent", e.target.value)}
            className="h-8 rounded-md border border-surface-700 bg-surface-800 px-2 text-xs text-text-primary disabled:opacity-50"
          >
            {!agents.some((a) => a.name === draft.agent) && <option value={draft.agent}>{draft.agent}</option>}
            {agents.map((a) => (
              <option key={a.name} value={a.name}>
                {a.display_name}
              </option>
            ))}
          </select>
        </Row>
        <Row label="View">
          <div role="radiogroup" aria-label="View" className="flex gap-1">
            {(["structured", "terminal"] as const).map((v) => (
              <button
                key={v}
                type="button"
                role="radio"
                aria-checked={draft.view === v}
                disabled={off}
                onClick={() => set("view", v)}
                className={`${BUTTON} ${draft.view === v ? "border-brand-500 bg-brand-500/10" : ""}`}
              >
                {v === "structured" ? "Structured" : "Terminal"}
              </button>
            ))}
          </div>
        </Row>
        <Row label="Worktree" hint="A new branch for each Session.">
          <SuggestionSwitch
            checked={draft.worktree}
            disabled={off}
            label="Worktree"
            onChange={(v) => set("worktree", v)}
          />
        </Row>
        <Row label="Sandbox" hint="Run in a container.">
          <SuggestionSwitch
            checked={draft.sandbox}
            disabled={off}
            label="Sandbox"
            onChange={(v) => set("sandbox", v)}
          />
        </Row>
        <Row label="Yolo mode" hint="Skip permission prompts.">
          <SuggestionSwitch checked={draft.yolo} disabled={off} label="Yolo mode" onChange={(v) => set("yolo", v)} />
        </Row>
      </div>
      <div>
        <div className="mb-1 text-sm text-text-primary">Prompt</div>
        <TemplateEditor
          label={`${provider.name} ${source.criterion} prompt`}
          value={draft.prompt ?? ""}
          variables={source.variables}
          disabled={off}
          onChange={(v) => set("prompt", v)}
        />
        <p className="mt-2 text-xs text-text-dim">
          {draft.prompt === null
            ? `Uses ${provider.name}'s own prompt until you type one. `
            : "Words in {braces} are filled in from each suggestion. "}
          {draft.prompt !== null && (
            <button
              type="button"
              disabled={off}
              onClick={() => set("prompt", null)}
              className="cursor-pointer text-brand-500 hover:underline disabled:opacity-50"
            >
              Reset to {provider.name}&apos;s prompt
            </button>
          )}
        </p>
        <p className="mt-1 break-words font-mono text-xs text-text-muted">
          {item ? `For ${shortRef(item.source_ref)}: ` : "Preview: "}
          {preview}
        </p>
      </div>
      <div className="flex items-center gap-2">
        <button type="button" className={GHOST} disabled={off || !entry?.customized} onClick={() => void reset()}>
          Reset to {provider.name}&apos;s settings
        </button>
        <span className="flex-1" />
        <button type="button" className={GHOST} disabled={busy} onClick={onBack}>
          Cancel
        </button>
        <button type="button" className={BUTTON} disabled={off} onClick={() => void save()}>
          {busy ? "Saving…" : "Save"}
        </button>
      </div>
    </div>
  );
}
