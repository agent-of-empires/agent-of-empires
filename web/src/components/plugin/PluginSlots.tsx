// Host-rendered plugin UI slots. The host ships typed display state; no plugin
// code runs here. Pane bodies live in PluginPane; sort-key and filter-facet are
// rendered by the sidebar.

import { useRef, useState } from "react";

import { invokePluginAction, type PluginUiEntry } from "../../lib/api";
import { usePluginUiEntries, usePluginUiPoke } from "../../lib/pluginUiContext";
import {
  entryText,
  entryTone,
  globalEntries,
  lucideIcon,
  payloadStr,
  sessionEntries,
  toneClasses,
  toneTextClass,
} from "../../lib/pluginUi";
import { PluginPaneBody } from "./PluginPane";
import { BadgeChip, Spinner } from "./SlotChrome";
import { objectList, renderIcon } from "./slotPayload";

export interface ComposerActionSnapshot {
  text: string;
  selectionStart: number;
  selectionEnd: number;
}

const entryKey = (e: PluginUiEntry) => `${e.plugin_id}:${e.id}`;

function EntryBadge({ entry }: { entry: PluginUiEntry }) {
  return <BadgeChip item={entry.payload} slot={entry.slot} pluginId={entry.plugin_id} />;
}

export function PluginStatusBarSegments() {
  return globalEntries(usePluginUiEntries(), "status-bar").map((e) => <EntryBadge key={entryKey(e)} entry={e} />);
}

/** An entry is a single badge or an `items` list; `items: []` clears the row. */
export function PluginRowBadges({ sessionId }: { sessionId: string }) {
  return sessionEntries(usePluginUiEntries(), "row-badge", sessionId).map((e) => {
    const items = objectList(e.payload, "items");
    return items ? (
      items.map((it, i) => <BadgeChip key={`${entryKey(e)}:${i}`} item={it} slot="row-badge" pluginId={e.plugin_id} />)
    ) : (
      <EntryBadge key={entryKey(e)} entry={e} />
    );
  });
}

export function PluginRowColumn({ sessionId }: { sessionId: string }) {
  const entries = sessionEntries(usePluginUiEntries(), "row-column", sessionId);
  if (entries.length === 0) return null;
  return (
    <span className="ml-auto flex shrink-0 items-center gap-1.5">
      {entries.map((e) => {
        const text = entryText(e);
        if (!text) return null;
        return (
          <span
            key={entryKey(e)}
            className={`max-w-32 truncate font-mono text-[11px] ${toneTextClass(entryTone(e))}`}
            title={payloadStr(e, "tooltip") || text}
            data-plugin-slot="row-column"
            data-plugin-id={e.plugin_id}
          >
            {text}
          </span>
        );
      })}
    </span>
  );
}

/** Badges plus the right-anchored column on their own line under the session
 *  name, so a narrow sidebar cannot squeeze them; nothing when both are empty. */
export function PluginRowLine({ sessionId }: { sessionId: string }) {
  const entries = usePluginUiEntries();
  if (
    sessionEntries(entries, "row-badge", sessionId).length === 0 &&
    sessionEntries(entries, "row-column", sessionId).length === 0
  ) {
    return null;
  }
  return (
    <span className="mt-0.5 flex items-center gap-1.5">
      <span className="flex min-w-0 flex-wrap items-center gap-1.5">
        <PluginRowBadges sessionId={sessionId} />
      </span>
      <PluginRowColumn sessionId={sessionId} />
    </span>
  );
}

export function PluginCards() {
  const entries = globalEntries(usePluginUiEntries(), "card");
  if (entries.length === 0) return null;
  return (
    <div
      className="mt-4 w-full max-w-2xl grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-3"
      data-testid="plugin-cards"
    >
      {entries.map((e) => {
        const body = payloadStr(e, "body");
        return (
          <div
            key={entryKey(e)}
            className={`rounded-lg p-3 ring-1 ring-surface-700/60 ${toneClasses(entryTone(e))}`}
            data-plugin-id={e.plugin_id}
          >
            <div className="font-semibold text-sm">{payloadStr(e, "title")}</div>
            {body && <div className="mt-1 text-xs text-text-secondary whitespace-pre-wrap">{body}</div>}
          </div>
        );
      })}
    </div>
  );
}

/** Host-wide panes on the overview, sharing the session pane renderer. */
export function PluginHomePanes() {
  const entries = globalEntries(usePluginUiEntries(), "home-pane");
  if (entries.length === 0) return null;
  return (
    <div className="mt-4 flex w-full max-w-2xl flex-col gap-3" data-testid="plugin-home-panes">
      {entries.map((e) => {
        const title = payloadStr(e, "title");
        return (
          <div key={entryKey(e)} className="rounded-lg ring-1 ring-surface-700/60" data-plugin-id={e.plugin_id}>
            {title && <div className="border-b border-surface-700/60 px-3 py-2 font-semibold text-sm">{title}</div>}
            <PluginPaneBody entry={e} titleInChrome />
          </div>
        );
      })}
    </div>
  );
}

export function PluginDetailBadges({ sessionId }: { sessionId: string }) {
  const entries = sessionEntries(usePluginUiEntries(), "detail-badge", sessionId);
  if (entries.length === 0) return null;
  return (
    <div className="flex flex-wrap items-center gap-1.5" data-testid="plugin-detail-badges">
      {entries.map((e) => (
        <EntryBadge key={entryKey(e)} entry={e} />
      ))}
    </div>
  );
}

export function PluginComposerActions({
  sessionId,
  getSnapshot,
}: {
  sessionId: string;
  getSnapshot: () => ComposerActionSnapshot;
}) {
  return sessionEntries(usePluginUiEntries(), "composer-action", sessionId).map((entry) => (
    <ComposerActionButton key={entryKey(entry)} entry={entry} sessionId={sessionId} getSnapshot={getSnapshot} />
  ));
}

function ComposerActionButton({
  entry,
  sessionId,
  getSnapshot,
}: {
  entry: PluginUiEntry;
  sessionId: string;
  getSnapshot: () => ComposerActionSnapshot;
}) {
  const label = payloadStr(entry, "label");
  const method = payloadStr(entry, "method");
  const [posting, setPosting] = useState(false);
  const postingRef = useRef(false);
  const poke = usePluginUiPoke();
  if (!label || !method) return null;
  const disabled = entry.payload.disabled === true;
  const onClick = async () => {
    if (postingRef.current || disabled) return;
    postingRef.current = true;
    setPosting(true);
    try {
      const { text, selectionStart, selectionEnd } = getSnapshot();
      const accepted = await invokePluginAction(entry.plugin_id, method, sessionId, {
        composer: { text, selection_start: selectionStart, selection_end: selectionEnd },
      });
      if (accepted) poke();
    } finally {
      postingRef.current = false;
      setPosting(false);
    }
  };
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled || posting}
      title={payloadStr(entry, "tooltip") || label}
      aria-label={label}
      aria-busy={posting || undefined}
      data-testid="plugin-composer-action"
      className={`inline-flex h-8 items-center justify-center gap-1 rounded-md border border-surface-700 bg-surface-800 px-2.5 text-[12px] ${toneClasses(entryTone(entry))} hover:bg-surface-700 disabled:cursor-not-allowed disabled:opacity-60 transition-colors duration-100`}
    >
      {posting ? (
        <Spinner className="size-3.5" />
      ) : (
        renderIcon(lucideIcon(payloadStr(entry, "icon")), "size-3.5 shrink-0")
      )}
      <span className="max-w-24 truncate">{label}</span>
    </button>
  );
}
