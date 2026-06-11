// Inline passphrase prompt that pops when a sensitive request returns
// `403 elevation_required` (terminal attach, structured view command exec,
// approval resolution, file writes). Calls `POST /api/login/elevate`
// to open a fresh 15-minute window; the original action can be
// retried by the user once the modal closes. See #1131.

import { useEffect, useRef, useState } from "react";

import { elevateLogin } from "../lib/api";
import { ELEVATION_REQUIRED_EVENT } from "../lib/fetchInterceptor";

export function ElevationPrompt() {
  const [open, setOpen] = useState(false);
  const [passphrase, setPassphrase] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    const onElevationRequired = () => {
      setOpen(true);
      setPassphrase("");
      setError(null);
      setLoading(false);
    };
    window.addEventListener(ELEVATION_REQUIRED_EVENT, onElevationRequired);
    return () => window.removeEventListener(ELEVATION_REQUIRED_EVENT, onElevationRequired);
  }, []);

  const close = () => setOpen(false);

  const handleSubmit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (loading || !passphrase.trim()) return;
    setLoading(true);
    setError(null);
    const result = await elevateLogin(passphrase);
    if (result.ok) {
      setLoading(false);
      setOpen(false);
      return;
    }
    setError(result.error ?? "Could not confirm passphrase");
    setLoading(false);
    inputRef.current?.focus();
  };

  if (!open) return null;

  return (
    <div
      role="dialog"
      aria-modal="true"
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm p-4 safe-area-inset"
      onClick={(e) => {
        if (e.target === e.currentTarget) close();
      }}
    >
      <form
        onSubmit={handleSubmit}
        className="w-full max-w-sm rounded-xl border border-surface-700/40 bg-surface-800 p-6 shadow-xl"
      >
        <div className="mb-3">
          <div className="text-sm font-medium text-text-primary">Confirm passphrase</div>
          <div className="mt-1 text-xs text-text-muted">
            This action requires re-entering your passphrase. The confirmation stays valid for 15 minutes; you will not
            be prompted again during that window.
          </div>
        </div>
        <input
          ref={inputRef}
          autoFocus
          type="password"
          autoComplete="current-password"
          value={passphrase}
          onChange={(e) => setPassphrase(e.target.value)}
          disabled={loading}
          placeholder="Enter passphrase"
          className="w-full rounded-lg border border-surface-700/60 bg-surface-900 px-3 py-2.5 text-sm text-text-primary placeholder:text-text-dim focus:outline-none focus:ring-2 focus:ring-brand-600 disabled:opacity-50"
        />
        {error && <p className="mt-3 text-xs text-status-error">{error}</p>}
        <div className="mt-4 flex items-center justify-end gap-2">
          <button
            type="button"
            onClick={close}
            disabled={loading}
            className="rounded-lg border border-surface-700/60 bg-transparent px-3 py-1.5 text-xs text-text-secondary hover:bg-surface-700/40 disabled:opacity-50"
          >
            Cancel
          </button>
          <button
            type="submit"
            disabled={loading || !passphrase.trim()}
            className="rounded-lg bg-brand-600 px-3 py-1.5 text-xs font-medium text-white hover:bg-brand-700 disabled:opacity-50"
          >
            {loading ? "Confirming..." : "Confirm"}
          </button>
        </div>
      </form>
    </div>
  );
}
