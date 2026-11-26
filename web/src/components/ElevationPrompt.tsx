// Inline passphrase prompt that pops when a sensitive request returns `403 elevation_required` (terminal attach,
// structured view command exec, approval resolution, file writes).

import { useEffect, useRef, useState } from "react";

import { elevateLogin } from "../lib/api";
import { ELEVATION_REQUIRED_EVENT } from "../lib/fetchInterceptor";
import { INPUT } from "../lib/surfaces";
import { CancelButton, ConfirmButton, Dialog } from "./Dialog";

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
    <Dialog
      id="elevation-prompt"
      title="Confirm passphrase"
      panelClassName="md:w-[384px]"
      describedBy="elevation-prompt-note"
      onDismiss={close}
      footer={
        <>
          <CancelButton onClick={close} disabled={loading} />
          <ConfirmButton type="submit" form="elevation-prompt-form" disabled={loading || !passphrase.trim()}>
            {loading ? "Confirming..." : "Confirm"}
          </ConfirmButton>
        </>
      }
    >
      <form id="elevation-prompt-form" onSubmit={handleSubmit}>
        <p id="elevation-prompt-note" className="mb-3 text-[13px] md:text-xs text-text-muted">
          This action requires re-entering your passphrase. The confirmation stays valid for 15 minutes; you will not be
          prompted again during that window.
        </p>
        <input
          ref={inputRef}
          autoFocus
          type="password"
          autoComplete="current-password"
          value={passphrase}
          onChange={(e) => setPassphrase(e.target.value)}
          disabled={loading}
          placeholder="Enter passphrase"
          aria-label="Passphrase"
          className={INPUT}
        />
        {error && <p className="mt-3 text-[13px] md:text-xs text-status-error">{error}</p>}
      </form>
    </Dialog>
  );
}
