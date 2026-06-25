// Renderers for the host-rendered plugin UI slots (#2366). The host ships
// typed display state; these components draw it. No plugin code runs here.
// Each reads the shared snapshot via context and the pure selectors in
// `pluginUi.ts`. Slots shipped here: status-bar, row-badge, row-column, card,
// detail-panel, detail-badge. Notifications surface as toasts via the hook;
// sort-key and filter-facet are deferred (see #2366 follow-ups).

import { usePluginUiEntries } from "../../lib/pluginUiContext";
import { entryText, entryTone, globalEntries, payloadStr, sessionEntries, toneClasses } from "../../lib/pluginUi";
import type { PluginUiEntry } from "../../lib/api";

function Badge({ entry }: { entry: PluginUiEntry }) {
  const text = entryText(entry);
  if (!text) return null;
  const tooltip = payloadStr(entry, "tooltip");
  return (
    <span
      className={`font-mono text-[11px] px-1.5 py-0.5 rounded-full ${toneClasses(entryTone(entry))}`}
      title={tooltip || `${entry.plugin_id}`}
      data-plugin-slot={entry.slot}
      data-plugin-id={entry.plugin_id}
    >
      {text}
    </span>
  );
}

/** status-bar: global segments in the top bar's right zone. */
export function PluginStatusBarSegments() {
  const entries = globalEntries(usePluginUiEntries(), "status-bar");
  if (entries.length === 0) return null;
  return (
    <>
      {entries.map((e) => (
        <Badge key={`${e.plugin_id}:${e.id}`} entry={e} />
      ))}
    </>
  );
}

/** row-badge: per-session badges shown inline on a session row. */
export function PluginRowBadges({ sessionId }: { sessionId: string }) {
  const entries = sessionEntries(usePluginUiEntries(), "row-badge", sessionId);
  if (entries.length === 0) return null;
  return (
    <>
      {entries.map((e) => (
        <Badge key={`${e.plugin_id}:${e.id}`} entry={e} />
      ))}
    </>
  );
}

/** row-column: per-session text column, right-aligned on a session row. The
 *  payload may also carry sort/filter scalars; rendering those as interactive
 *  controls is the deferred sort-key/filter-facet work. */
export function PluginRowColumn({ sessionId }: { sessionId: string }) {
  const entries = sessionEntries(usePluginUiEntries(), "row-column", sessionId);
  if (entries.length === 0) return null;
  return (
    <span className="flex items-center gap-1.5">
      {entries.map((e) => {
        const text = entryText(e);
        if (!text) return null;
        return (
          <span
            key={`${e.plugin_id}:${e.id}`}
            className={`font-mono text-[11px] ${
              toneClasses(entryTone(e))
                .split(" ")
                .find((c) => c.startsWith("text-")) ?? "text-text-dim"
            }`}
            title={payloadStr(e, "tooltip") || e.plugin_id}
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

/** card: global cards on the dashboard overview. */
export function PluginCards() {
  const entries = globalEntries(usePluginUiEntries(), "card");
  if (entries.length === 0) return null;
  return (
    <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-3" data-testid="plugin-cards">
      {entries.map((e) => {
        const title = payloadStr(e, "title");
        const body = payloadStr(e, "body");
        return (
          <div
            key={`${e.plugin_id}:${e.id}`}
            className={`rounded-lg p-3 ring-1 ring-surface-700/60 ${toneClasses(entryTone(e))}`}
            data-plugin-id={e.plugin_id}
          >
            <div className="font-semibold text-sm">{title}</div>
            {body && <div className="mt-1 text-xs text-text-secondary whitespace-pre-wrap">{body}</div>}
          </div>
        );
      })}
    </div>
  );
}

/** detail-badge: per-session badges in the session detail panel. */
export function PluginDetailBadges({ sessionId }: { sessionId: string }) {
  const entries = sessionEntries(usePluginUiEntries(), "detail-badge", sessionId);
  if (entries.length === 0) return null;
  return (
    <div className="flex flex-wrap items-center gap-1.5" data-testid="plugin-detail-badges">
      {entries.map((e) => (
        <Badge key={`${e.plugin_id}:${e.id}`} entry={e} />
      ))}
    </div>
  );
}

/** detail-panel: per-session panels in the session detail view. */
export function PluginDetailPanels({ sessionId }: { sessionId: string }) {
  const entries = sessionEntries(usePluginUiEntries(), "detail-panel", sessionId);
  if (entries.length === 0) return null;
  return (
    <div className="flex flex-col gap-2" data-testid="plugin-detail-panels">
      {entries.map((e) => {
        const title = payloadStr(e, "title");
        const body = payloadStr(e, "body");
        return (
          <section
            key={`${e.plugin_id}:${e.id}`}
            className="rounded-lg p-3 ring-1 ring-surface-700/60 bg-surface-800/40"
            data-plugin-id={e.plugin_id}
          >
            {title && <div className="font-semibold text-sm text-text-primary">{title}</div>}
            <div className="mt-1 text-xs text-text-secondary whitespace-pre-wrap">{body}</div>
          </section>
        );
      })}
    </div>
  );
}
