import { useState } from "react";
import { RefreshCw } from "lucide-react";

import { fetchGatewayModels, type GatewayModelInfo } from "../../lib/api";
import { TextField } from "./FormFields";
import type { CustomWidgetProps } from "./customWidgets";

/** Model Gateway settings widget (`acp` section, custom widget bound to the
 *  `gateway_base_url` field). URL + credential + discovery path are one unit
 *  and are useless to configure blind, so the URL row carries a "Test
 *  discovery" affordance: it runs the daemon-side `GET /api/acp/models` (the
 *  exact call the session model picker uses) and reports what the gateway
 *  served. The api-key and discovery-path fields render as their own plain
 *  schema rows beside this one.
 *
 *  Credential handling: the key field holds a credential REFERENCE, not a
 *  secret in the nodeterm sense — `${env:VAR}` (resolved against the daemon's
 *  own environment) or `${secret:model-gateway-api-key}` (env
 *  `MODEL_GATEWAY_API_KEY`). Either is a non-secret pointer and is safe to
 *  type into the browser; the daemon resolves it and never echoes the
 *  resolved key back. A literal key pasted there is stored verbatim in
 *  config.toml (host-file permissions) and is sent only by the daemon itself,
 *  never logged and never echoed by any endpoint. The elevation wall (a 403
 *  `elevation_required` from PATCH /api/settings) pops the global passphrase
 *  prompt via the fetch interceptor, the same as any other elevated setting.
 */
export function ModelGatewayWidget({ descriptor, value, save }: CustomWidgetProps) {
  const [testing, setTesting] = useState(false);
  const [result, setResult] = useState<{ models: GatewayModelInfo[]; error?: string } | null>(null);

  const testDiscovery = async () => {
    setTesting(true);
    setResult(null);
    try {
      const res = await fetchGatewayModels();
      if (res === null) {
        setResult({ models: [], error: "Could not reach the daemon endpoint." });
      } else if (res.error) {
        setResult({ models: [], error: res.error });
      } else {
        setResult({ models: res.models ?? [] });
      }
    } catch {
      setResult({ models: [], error: "Discovery request failed." });
    } finally {
      setTesting(false);
    }
  };

  const url = typeof value === "string" ? value : "";

  return (
    <div
      data-testid="model-gateway-widget"
      className="rounded-lg border border-surface-700 bg-surface-850/40 p-3 space-y-3"
    >
      <TextField
        label={descriptor.label}
        description={descriptor.description}
        value={url}
        onChange={(v) => save(v)}
        placeholder="https://gateway.example.com"
        mono
      />
      <div className="flex items-center gap-2">
        <button
          type="button"
          onClick={() => void testDiscovery()}
          disabled={testing}
          data-testid="model-gateway-test-discovery"
          className="inline-flex items-center gap-1.5 rounded-md border border-surface-700 bg-surface-800 px-3 py-1.5 text-xs font-medium text-text-secondary transition-colors hover:border-brand-600/60 hover:text-text-primary disabled:opacity-50"
        >
          <RefreshCw className={"h-3.5 w-3.5" + (testing ? " animate-spin" : "")} />
          {testing ? "Testing…" : "Test discovery"}
        </button>
        {result && !result.error && (
          <span
            data-testid="model-gateway-discovery-ok"
            className="text-xs text-status-running"
          >
            {result.models.length === 0
              ? "Gateway reachable, but it served no models."
              : `${result.models.length} model${result.models.length === 1 ? "" : "s"} available`}
          </span>
        )}
        {result?.error && (
          <span data-testid="model-gateway-discovery-error" className="text-xs text-status-error">
            {result.error}
          </span>
        )}
      </div>
      {result && !result.error && result.models.length > 0 && (
        <ul data-testid="model-gateway-discovery-list" className="space-y-1 text-xs text-text-secondary">
          {result.models.map((m) => (
            <li key={m.id} className="flex items-baseline justify-between gap-2">
              <span className="truncate font-mono">{m.name || m.id}</span>
              {m.context_window != null && (
                <span className="shrink-0 text-text-dim">
                  {m.context_window >= 1_000_000
                    ? `${Math.round(m.context_window / 100_000) / 10}M ctx`
                    : `${Math.round(m.context_window / 1000)}K ctx`}
                </span>
              )}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
