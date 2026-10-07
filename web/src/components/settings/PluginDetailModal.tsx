import { useEffect, useState } from "react";

import { fetchPluginDetails, type PluginDetail } from "../../lib/api";
import { PluginIdentityIcon } from "./PluginIdentityIcon";
import { ModalSection, PluginModal } from "./PluginModal";
import { uniqueSlots } from "./uniqueSlots";

interface Fallback {
  version?: string;
  description?: string;
  capabilities?: string[];
  ui_contributions?: { slot: string; id: string }[];
  icon?: string | null;
  icon_asset_url?: string | null;
}

interface Props {
  /** `gh:owner/repo` fetches the live manifest and releases; anything else shows the fallback. */
  source: string;
  title: string;
  /** Known fields shown while fetching and for non-GitHub sources. */
  fallback?: Fallback;
  installCommand?: string;
  onClose: () => void;
}

/** One plugin's screenshots, version, access and release list. Remount per source. */
export function PluginDetailModal({ source, title, fallback, installCommand, onClose }: Props) {
  const isGithub = source.startsWith("gh:");
  const [detail, setDetail] = useState<PluginDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [zoomed, setZoomed] = useState<{ src: string; alt: string } | null>(null);
  const loading = isGithub && !detail && !error;

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      if (zoomed) setZoomed(null);
      else onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose, zoomed]);

  useEffect(() => {
    if (!isGithub) return;
    let cancelled = false;
    void fetchPluginDetails(source).then((res) => {
      if (cancelled) return;
      if (res.kind === "ok") setDetail(res.detail);
      else setError(res.message);
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
  const screenshots = manifest?.screenshots ?? [];

  return (
    <PluginModal
      testId="plugin-detail-modal"
      ariaLabel={`${title} details`}
      closeTestId="plugin-detail-close"
      onClose={onClose}
      closeOnEscape={false}
      header={
        <div className="flex items-start gap-2">
          <PluginIdentityIcon
            icon={manifest?.icon ?? fallback?.icon ?? null}
            iconAssetUrl={manifest?.icon_asset_url ?? fallback?.icon_asset_url ?? null}
            className="mt-0.5 size-5"
            testId="plugin-detail-icon"
          />
          <div>
            <h2 className="font-semibold">{title}</h2>
            {version && <p className="text-xs text-text-dim">v{version}</p>}
            <p className="text-[11px] text-text-dim">{source}</p>
          </div>
        </div>
      }
      overlay={
        zoomed && (
          <div
            className="fixed inset-0 z-[60] flex items-center justify-center bg-black/80 p-4"
            role="dialog"
            aria-modal="true"
            aria-label={`${zoomed.alt || "Screenshot"} full size`}
            onClick={(e) => {
              // Keep the click from closing the modal; only the lightbox backdrop dismisses.
              e.stopPropagation();
              if (e.target === e.currentTarget) setZoomed(null);
            }}
            data-testid="plugin-detail-lightbox"
          >
            <img src={zoomed.src} alt={zoomed.alt} className="max-h-full max-w-full object-contain" />
          </div>
        )
      }
    >
      {loading && <p className="text-xs text-text-dim">Loading details…</p>}
      {error && (
        <p className="text-xs text-status-error" data-testid="plugin-detail-error">
          {error}
        </p>
      )}

      {screenshots.length > 0 && (
        <div className="mb-3 grid gap-3" data-testid="plugin-detail-screenshots">
          {screenshots.map((shot) => (
            <figure key={shot.src} className="overflow-hidden rounded border border-surface-700 bg-surface-950">
              <button
                type="button"
                className="block w-full cursor-zoom-in"
                onClick={() => setZoomed({ src: shot.src, alt: shot.alt })}
                aria-label={`View ${shot.alt || "screenshot"} full size`}
              >
                <img
                  src={shot.src}
                  alt={shot.alt}
                  loading="lazy"
                  decoding="async"
                  className="max-h-72 w-full object-contain"
                  onError={(e) => {
                    e.currentTarget.closest("figure")?.classList.add("hidden");
                  }}
                />
              </button>
              {shot.caption && <figcaption className="px-2 py-1 text-[11px] text-text-dim">{shot.caption}</figcaption>}
            </figure>
          ))}
        </div>
      )}

      {description && <p className="mb-3 text-text-dim">{description}</p>}
      {capabilities.length > 0 && <ModalSection label="Capabilities">{capabilities.join(", ")}</ModalSection>}
      {ui.length > 0 && <ModalSection label="UI slots">{uniqueSlots(ui)}</ModalSection>}

      {manifest?.api_version != null && (
        <p className="mb-3 text-[11px] text-text-dim">Manifest api_version: {manifest.api_version}</p>
      )}
      {detail?.manifest_error && (
        <p className="mb-3 text-[11px] text-status-warning">Manifest: {detail.manifest_error}</p>
      )}

      {isGithub && (
        <ModalSection label="Available versions" testId="plugin-detail-versions">
          {detail && detail.release_tags.length > 0 ? (
            <ul className="flex flex-wrap gap-1">
              {detail.release_tags.map((tag) => (
                <li key={tag} className="rounded bg-surface-800 px-1.5 py-0.5 text-[11px] text-text-dim">
                  {tag}
                </li>
              ))}
            </ul>
          ) : (
            // A failed fetch must not read as zero releases.
            !loading && !error && <p className="text-xs text-text-dim">No published releases.</p>
          )}
        </ModalSection>
      )}

      {installCommand && (
        <p className="text-[11px] text-text-dim">
          Install in a terminal: <code>{installCommand}</code>
        </p>
      )}
    </PluginModal>
  );
}
