import { useEffect, useState } from "react";

import { fetchPluginDetails, type PluginDetail } from "../../lib/api";

interface Fallback {
  version?: string;
  description?: string;
  capabilities?: string[];
  ui_contributions?: { slot: string; id: string }[];
}

interface PluginDetailModalProps {
  /** gh:owner/repo or a local path. Remote detail (manifest + release tags) is
   *  only fetched for a gh source; a local plugin shows the fallback only. */
  source: string;
  /** Name shown in the header immediately, before any fetch resolves. */
  title: string;
  /** Already-known fields (an installed plugin's view) shown while the fetch is
   *  in flight and for a non-gh source. */
  fallback?: Fallback;
  /** Shown for a discovery result so the user can copy the install command. */
  installCommand?: string;
  onClose: () => void;
}

/// A modal showing one plugin's detail: version, description, capabilities, UI
/// slots, and the available release versions. Opened from a discovery result or
/// an installed-plugin row. For a gh source it fetches the live manifest +
/// release tags; for a local source it renders the passed-in fallback fields.
/// Screenshots and richer metadata are a future addition.
export function PluginDetailModal({ source, title, fallback, installCommand, onClose }: PluginDetailModalProps) {
  const isGithub = source.startsWith("gh:");
  const [detail, setDetail] = useState<PluginDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Derived, not stored: avoids a synchronous setState in the effect. The modal
  // is remounted per source (keyed at the call site), so this resets each open.
  const loading = isGithub && !detail && !error;

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  useEffect(() => {
    if (!isGithub) return;
    let cancelled = false;
    void fetchPluginDetails(source).then((res) => {
      if (cancelled) return;
      if (res.kind === "ok") {
        setDetail(res.detail);
      } else {
        setError(res.message);
      }
    });
    return () => {
      cancelled = true;
    };
  }, [source, isGithub]);

  const manifest = detail?.manifest ?? null;
  const version = manifest?.version ?? fallback?.version ?? null;
  const description = manifest?.description ?? fallback?.description ?? null;
  const capabilities = manifest?.capabilities ?? fallback?.capabilities ?? [];
  const ui = manifest?.ui_contributions ?? fallback?.ui_contributions ?? [];

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
      role="dialog"
      aria-modal="true"
      aria-label={`${title} details`}
      onClick={onClose}
      data-testid="plugin-detail-modal"
    >
      <div
        className="max-h-[80vh] w-full max-w-lg overflow-auto rounded border border-surface-700 bg-surface-900 p-4 text-sm"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="mb-3 flex items-start justify-between gap-3">
          <div>
            <h2 className="font-semibold">{title}</h2>
            {version && <p className="text-xs text-text-dim">v{version}</p>}
            <p className="text-[11px] text-text-dim">{source}</p>
          </div>
          <button
            type="button"
            className="rounded border border-surface-700 px-2 py-0.5 text-xs hover:bg-surface-800"
            onClick={onClose}
            data-testid="plugin-detail-close"
          >
            Close
          </button>
        </div>

        {loading && <p className="text-xs text-text-dim">Loading details…</p>}
        {error && (
          <p className="text-xs text-status-error" data-testid="plugin-detail-error">
            {error}
          </p>
        )}

        {description && <p className="mb-3 text-text-dim">{description}</p>}

        {capabilities.length > 0 && (
          <div className="mb-3">
            <p className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-text-dim">Capabilities</p>
            <p className="text-xs text-text-dim">{capabilities.join(", ")}</p>
          </div>
        )}

        {ui.length > 0 && (
          <div className="mb-3">
            <p className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-text-dim">UI slots</p>
            <p className="text-xs text-text-dim">{[...new Set(ui.map((u) => u.slot))].join(", ")}</p>
          </div>
        )}

        {manifest?.api_version != null && (
          <p className="mb-3 text-[11px] text-text-dim">Manifest api_version: {manifest.api_version}</p>
        )}

        {detail?.manifest_error && (
          <p className="mb-3 text-[11px] text-status-warning">Manifest: {detail.manifest_error}</p>
        )}

        {isGithub && (
          <div className="mb-3" data-testid="plugin-detail-versions">
            <p className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-text-dim">Available versions</p>
            {detail && detail.release_tags.length > 0 ? (
              <ul className="flex flex-wrap gap-1">
                {detail.release_tags.map((tag) => (
                  <li key={tag} className="rounded bg-surface-800 px-1.5 py-0.5 text-[11px] text-text-dim">
                    {tag}
                  </li>
                ))}
              </ul>
            ) : (
              // Only claim "no releases" after a successful fetch; a transport
              // error already shows above and must not read as zero releases.
              !loading && !error && <p className="text-xs text-text-dim">No published releases.</p>
            )}
          </div>
        )}

        {installCommand && (
          <p className="text-[11px] text-text-dim">
            Install in a terminal: <code>{installCommand}</code>
          </p>
        )}
      </div>
    </div>
  );
}
