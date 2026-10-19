import { useCallback, useEffect, useState } from "react";
import { fetchPasskeys, passkeysSupported, registerPasskey, removePasskey, type PasskeyInfo } from "../lib/api";

export function PasskeySettings() {
  const [passkeys, setPasskeys] = useState<PasskeyInfo[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => setPasskeys(await fetchPasskeys()), []);

  useEffect(() => {
    // Deferred so the state update is not synchronous in the effect body.
    const first = setTimeout(load, 0);
    return () => clearTimeout(first);
  }, [load]);

  if (!passkeysSupported() || passkeys === null) return null;

  const onAdd = async () => {
    setBusy(true);
    setError(null);
    const result = await registerPasskey(navigator.platform || "This device");
    setBusy(false);
    if (result.ok) await load();
    else if (!result.cancelled) setError(result.error ?? "Could not add passkey");
  };

  const onRemove = async (id: string) => {
    setBusy(true);
    const ok = await removePasskey(id);
    setBusy(false);
    if (ok) await load();
  };

  return (
    <div className="mt-8">
      <h3 className="font-mono text-sm uppercase tracking-widest text-text-muted mb-4">Passkeys</h3>
      {passkeys.map((p) => (
        <div key={p.credential_id} className="flex items-center justify-between gap-3 py-3 border-b border-surface-700">
          <div className="min-w-0">
            <p className="font-body text-[13px] font-medium text-text-primary">{p.label}</p>
            <p className="font-body text-[11px] text-text-secondary">{p.origin}</p>
            <p className="font-body text-[11px] text-text-muted">
              {p.last_used_at_ms ? `last used ${new Date(p.last_used_at_ms).toLocaleString()}` : "never used"}
            </p>
          </div>
          <button
            type="button"
            onClick={() => onRemove(p.credential_id)}
            disabled={busy}
            className="shrink-0 font-mono text-[11px] uppercase tracking-wider text-status-error border border-status-error/40 rounded px-2 py-1 hover:bg-status-error/10 disabled:opacity-50"
          >
            Remove
          </button>
        </div>
      ))}
      {error && <p className="font-body text-[13px] text-status-error mt-2">{error}</p>}
      <button
        type="button"
        onClick={onAdd}
        disabled={busy}
        className="mt-4 font-mono text-[11px] uppercase tracking-wider text-text-secondary border border-surface-700 rounded px-3 py-1.5 hover:bg-surface-700/30 disabled:opacity-50"
      >
        {busy ? "Waiting..." : "Add a passkey for this device"}
      </button>
    </div>
  );
}
