import { useState } from "react";

import { fetchCityHallBundle } from "../../lib/api";
import { BUTTON_GHOST, BUTTON_PRIMARY, CARD } from "../../lib/surfaces";
import { CODE_BLOCK } from "../../lib/surfaces";
import { PanelMessage, SectionHeading } from "../panelParts";

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
    <div>
      <SectionHeading
        title="CityHall config bundle"
        description="One file describing how CityHall should set up every workspace: the settings on this install that differ from the defaults, plus your registered Projects, addressed by git remote so a workspace can clone them. Host-specific paths are left out, and no credentials are included. Paste it into CityHall's workspace configuration."
      />

      <div className={`${CARD} py-4 space-y-3`}>
        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            className={BUTTON_PRIMARY}
            disabled={busy}
            onClick={() => void generate()}
            data-testid="cityhall-export"
          >
            {busy ? "Generating…" : bundle ? "Regenerate" : "Generate bundle"}
          </button>
          {bundle && (
            <>
              <button type="button" className={BUTTON_GHOST} onClick={download}>
                Download cityhall.toml
              </button>
              <button type="button" className={BUTTON_GHOST} onClick={() => void copy()}>
                {copied ? "Copied" : "Copy"}
              </button>
            </>
          )}
          <span className="text-[12px] text-text-dim">
            or in a terminal: <code>aoe cityhall export --out cityhall.toml</code>
          </span>
        </div>

        {error && <PanelMessage tone="error">{error}</PanelMessage>}

        {bundle && <pre className={`${CODE_BLOCK} max-h-96`}>{bundle}</pre>}
      </div>
    </div>
  );
}
