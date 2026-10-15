import { useEffect, useState } from "react";
import { mintPluginPageTicket, type PluginPage } from "../../lib/api";
import { usePluginPages } from "../../lib/pluginUiContext";

interface Props {
  pluginId: string;
  pageId: string;
  onBack: () => void;
}

/** A plugin page, full width. The frame gets an opaque origin: no dashboard storage, cookies, token or API. */
export function PluginPageView({ pluginId, pageId, onBack }: Props) {
  const pages = usePluginPages();
  const page = pages?.find((p) => p.plugin_id === pluginId && p.id === pageId);
  const [minted, setMinted] = useState<{ page: PluginPage; ok: boolean } | null>(null);

  // The frame load authenticates with the ticket this sets, so mint before rendering it.
  useEffect(() => {
    if (!page) return;
    let cancelled = false;
    void mintPluginPageTicket(page).then((ok) => {
      if (!cancelled) setMinted({ page, ok });
    });
    return () => {
      cancelled = true;
    };
  }, [page]);

  const ticket = page && minted?.page === page ? minted : null;
  let body = null;
  if (pages !== null && !page) {
    body = <Empty title="Page not found" detail={`No active plugin ${pluginId} declares a page ${pageId}.`} />;
  } else if (page && ticket?.ok === false) {
    body = <Empty title="This page could not be opened" detail="Reload the dashboard to try again." />;
  } else if (page && ticket?.ok) {
    body = (
      <iframe
        title={page.title}
        src={page.url}
        sandbox="allow-scripts"
        className="flex-1 w-full border-0 bg-surface-900"
        data-testid="plugin-page-frame"
      />
    );
  }

  return (
    <div className="flex-1 flex flex-col overflow-hidden bg-surface-900" data-testid="plugin-page-view">
      <div className="bg-surface-850 border-b border-surface-700 shrink-0 flex items-center gap-3 px-4 h-12">
        <button onClick={onBack} className="text-brand-500 cursor-pointer text-sm shrink-0">
          &larr; Back
        </button>
        <span className="text-xs font-mono text-text-bright truncate">{page?.title ?? pageId}</span>
      </div>
      {body}
    </div>
  );
}

function Empty({ title, detail }: { title: string; detail: string }) {
  return (
    <div className="px-4 py-10 text-center" data-testid="plugin-page-empty">
      <p className="text-sm font-medium text-text-secondary">{title}</p>
      <p className="mt-1 text-[13px] text-text-muted">{detail}</p>
    </div>
  );
}
