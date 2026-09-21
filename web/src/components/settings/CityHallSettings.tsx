import { useState } from "react";

import { fetchCityHallBundle } from "../../lib/api";

/// Exports the CityHall config bundle. Export only: workspaces apply bundles at boot.
export function CityHallSettings() {
  const [bundle, setBundle] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [copied, setCopied] = useState(false);

  async function generate() {
    setBusy(true);
    setError(null);
    setCopied(false);
    try {
      setBundle(await fetchCityHallBundle());
    } catch (e) {
      setBundle(null);
      setError(e instanceof Error ? e.message : "Export failed");
    } finally {
      setBusy(false);
    }
  }

  function download() {
    if (!bundle) return;
    const url = URL.createObjectURL(new Blob([bundle], { type: "application/toml" }));
    const link = document.createElement("a");
    link.href = url;
    link.download = "cityhall.toml";
    link.click();
    URL.revokeObjectURL(url);
  }

  async function copy() {
    if (!bundle) return;
    try {
      await navigator.clipboard.writeText(bundle);
      setCopied(true);
    } catch {
      // Download remains available when clipboard access is denied.
    }
  }

  return (
    <div className="space-y-4">
      <div>
        <div className="text-[13px] text-text-secondary">CityHall config bundle</div>
        <p className="mt-1 text-[11px] text-text-muted">
          One file describing how CityHall should set up every workspace: the settings on this install that differ from
          the defaults, plus your saved projects, addressed by git remote so a workspace can clone them. Host-specific
          paths are left out, and no credentials are included. Paste it into CityHall's workspace configuration.
        </p>
      </div>

      <div className="flex flex-wrap items-center gap-2">
        <button
          type="button"
          className="h-8 cursor-pointer rounded-md bg-brand-600 px-3 text-xs font-medium text-white transition-colors duration-150 hover:bg-brand-500 disabled:opacity-50"
          disabled={busy}
          onClick={() => void generate()}
          data-testid="cityhall-export"
        >
          {busy ? "Generating…" : bundle ? "Regenerate" : "Generate bundle"}
        </button>
        {bundle && (
          <>
            <button
              type="button"
              className="h-8 cursor-pointer rounded-md px-3 text-xs text-text-secondary transition-colors duration-150 hover:bg-surface-800 hover:text-text-primary"
              onClick={download}
            >
              Download cityhall.toml
            </button>
            <button
              type="button"
              className="h-8 cursor-pointer rounded-md px-3 text-xs text-text-secondary transition-colors duration-150 hover:bg-surface-800 hover:text-text-primary"
              onClick={() => void copy()}
            >
              {copied ? "Copied" : "Copy"}
            </button>
          </>
        )}
        <span className="text-[11px] text-text-dim">
          or in a terminal: <code>aoe cityhall export --out cityhall.toml</code>
        </span>
      </div>

      {error && <p className="text-[11px] text-status-error">{error}</p>}

      {bundle && (
        <pre className="max-h-96 overflow-auto rounded border border-surface-700 bg-surface-900 p-3 text-[11px] leading-relaxed text-text-secondary">
          {bundle}
        </pre>
      )}
    </div>
  );
}
