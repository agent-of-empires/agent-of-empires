import { useEffect, useRef, useState } from "react";
import { fetchContextPrimer } from "../../lib/api";
import { BUTTON_WARNING_SM } from "../../lib/surfaces";
import { WarningStrip } from "../panelParts";

/** After a failed `session/load`, offers to prefill a recap of recent turns.
 *  Dismissal lives in the reducer so it survives session switches. */
interface Props {
  sessionId: string;
  available: { resetSeq: number; reason: string } | null;
  onInsertPrimer: (text: string) => void;
  onDismiss: () => void;
}

export function ContextPrimerBanner({ sessionId, available, onInsertPrimer, onDismiss }: Props) {
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const abortRef = useRef<AbortController | null>(null);

  // Reset transient state whenever a new reset incident lands.
  const [handledResetSeq, setHandledResetSeq] = useState<number | null>(null);
  const resetSeq = available?.resetSeq ?? null;
  if (resetSeq !== handledResetSeq) {
    setHandledResetSeq(resetSeq);
    setError(null);
    setLoading(false);
  }

  useEffect(() => {
    return () => {
      abortRef.current?.abort();
      abortRef.current = null;
    };
  }, [sessionId, available?.resetSeq]);

  if (!available) return null;

  const handleClick = async () => {
    abortRef.current?.abort();
    const controller = new AbortController();
    abortRef.current = controller;
    setLoading(true);
    setError(null);
    try {
      const resp = await fetchContextPrimer(sessionId, available.resetSeq, controller.signal);
      if (controller.signal.aborted) return;
      if (!resp || !resp.primer) {
        setError(resp ? "No prior transcript available to recap." : "Failed to fetch primer.");
        return;
      }
      onInsertPrimer(resp.primer);
      onDismiss();
    } catch (e) {
      if ((e as { name?: string }).name === "AbortError") return;
      setError("Network error fetching primer.");
    } finally {
      if (abortRef.current === controller) abortRef.current = null;
      if (!controller.signal.aborted) setLoading(false);
    }
  };

  return (
    <WarningStrip
      role="status"
      title="Agent lost its prior model context."
      onDismiss={onDismiss}
      dismissLabel="Dismiss context-reset banner"
      actions={
        <>
          {error && (
            <span className="text-status-error-text text-[12px] shrink-0" role="alert">
              {error}
            </span>
          )}
          <button type="button" onClick={handleClick} disabled={loading} className={`${BUTTON_WARNING_SM} shrink-0`}>
            {loading ? "Loading..." : "Resume with prior context"}
          </button>
        </>
      }
    >
      You can pre-fill the composer with a recap of the recent turns.
    </WarningStrip>
  );
}
