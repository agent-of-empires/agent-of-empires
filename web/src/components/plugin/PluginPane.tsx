import type { PluginUiEntry } from "../../lib/api";
import { usePluginUiRefreshing } from "../../lib/pluginUiContext";
import { lucideIcon, payloadStr, toneTextClass, validTone } from "../../lib/pluginUi";
import { DetailBlock } from "./paneBlocks";
import { Spinner } from "./SlotChrome";
import { isObject, objectList, renderIcon, str, type Obj } from "./slotPayload";

/** The scrollable body of one plugin pane: a `blocks` list or the simple
 *  `{ title, body }` form, plus an optional pinned footer. */
export function PluginPaneBody({
  entry,
  titleInChrome = false,
}: {
  entry: PluginUiEntry;
  /** The caller already renders the payload title. */
  titleInChrome?: boolean;
}) {
  const blocks = objectList(entry.payload, "blocks");
  const title = titleInChrome ? undefined : payloadStr(entry, "title");
  const body = payloadStr(entry, "body");
  const footer = isObject(entry.payload.footer) ? entry.payload.footer : undefined;
  const refreshing = usePluginUiRefreshing();
  return (
    <div className="flex flex-1 min-h-0 flex-col" data-testid="plugin-pane-body" data-plugin-id={entry.plugin_id}>
      <div className="min-h-0 flex-1 overflow-auto p-3">
        {refreshing && (
          <div
            className="sticky top-0 z-10 mb-1.5 flex items-center justify-end gap-1 text-[10px] text-text-dim"
            data-testid="plugin-pane-refreshing"
          >
            <Spinner className="size-3" />
            Refreshing…
          </div>
        )}
        {blocks ? (
          <div className="flex flex-col gap-1.5">
            {blocks.map((b, i) => (
              <DetailBlock key={i} block={b} pluginId={entry.plugin_id} sessionId={entry.session_id} />
            ))}
          </div>
        ) : (
          <>
            {title && <div className="font-semibold text-sm text-text-primary">{title}</div>}
            {body && <div className="mt-1 text-xs text-text-secondary whitespace-pre-wrap">{body}</div>}
          </>
        )}
      </div>
      {footer && <PaneFooter footer={footer} />}
    </div>
  );
}

function PaneFooter({ footer }: { footer: Obj }) {
  const text = str(footer, "text");
  const value = str(footer, "value");
  if (!text && !value) return null;
  return (
    <div
      className="flex shrink-0 items-center gap-1.5 border-t border-surface-700/60 px-3 py-1.5 font-mono text-[10px] text-text-dim"
      data-testid="plugin-pane-footer"
    >
      {renderIcon(lucideIcon(str(footer, "icon")), "size-3 shrink-0")}
      {text && <span className="min-w-0 truncate">{text}</span>}
      {value && <span className={`ml-auto shrink-0 ${toneTextClass(validTone(footer.tone))}`}>{value}</span>}
    </div>
  );
}
