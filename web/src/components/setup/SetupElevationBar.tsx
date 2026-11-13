import { Lock } from "lucide-react";
import { useState } from "react";
import type { ElevationRequest } from "../../hooks/useInlineMachineSave";

/** Footer passphrase prompt for a machine-wide save; edits stay in place while it is open. */
export function SetupElevationBar({ request }: { request: ElevationRequest }) {
  const [passphrase, setPassphrase] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (busy || !passphrase.trim()) return;
    setBusy(true);
    setError(await request.submit(passphrase));
    setBusy(false);
  };

  return (
    <form
      onSubmit={submit}
      className="border-t border-brand-600/60 bg-surface-950 px-6 md:px-16 py-4 flex flex-col gap-2"
      aria-label="Enter the dashboard passphrase"
    >
      <div className="flex items-center gap-2 text-sm">
        <Lock size={14} className="text-brand-500" />
        <span className="font-semibold text-text-primary">Enter the dashboard passphrase to save</span>
        <span className="hidden md:inline text-text-dim">These settings change files on the host.</span>
      </div>
      <div className="flex flex-wrap items-center gap-3">
        <input
          autoFocus
          type="password"
          autoComplete="current-password"
          value={passphrase}
          onChange={(e) => setPassphrase(e.target.value)}
          disabled={busy}
          aria-label="Passphrase"
          className={`h-10 flex-1 min-w-48 max-w-sm rounded-md border bg-surface-900 px-3 text-sm text-text-primary focus:outline-none focus:ring-2 focus:ring-brand-600 ${error ? "border-status-error" : "border-surface-700"}`}
        />
        <div className="ml-auto flex gap-2">
          <button
            type="button"
            onClick={request.cancel}
            disabled={busy}
            className="h-10 px-4 rounded-md border border-surface-700 text-sm text-text-secondary hover:bg-surface-800 cursor-pointer"
          >
            Cancel
          </button>
          <button
            type="submit"
            disabled={busy || !passphrase.trim()}
            className="h-10 px-4 rounded-md bg-brand-600 text-sm font-medium text-white hover:bg-brand-700 disabled:opacity-50 cursor-pointer"
          >
            Unlock and save
          </button>
        </div>
      </div>
      {error && <p className="text-xs text-status-error">{error} Your edits are kept.</p>}
    </form>
  );
}
