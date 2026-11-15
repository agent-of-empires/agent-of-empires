import { useEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from "react";
import { fetchAccountTargets, switchSessionAccount } from "../../lib/api";
import type { AuthStatus } from "../../lib/acpTypes";

interface Props {
  open: boolean;
  sessionId: string;
  /** The tool (account) the session runs as now. */
  currentTool: string;
  /** What the agent last reported; only its label and plan are shown. */
  authStatus: AuthStatus | null;
  /** Names already fetched by the caller; loaded here when omitted. */
  targets?: readonly string[];
  /** The account an automatic failover is moving to; confirming is blocked meanwhile. */
  failoverTo?: string;
  onClose: () => void;
}

/** Restarts the session as another account of the same agent, keeping the conversation. */
export function SwitchAccountModal({ open, ...rest }: Props) {
  // Mounted per opening, so each one starts with fresh state.
  return open ? <SwitchAccountDialog {...rest} /> : null;
}

function SwitchAccountDialog({
  sessionId,
  currentTool,
  authStatus,
  targets: given,
  failoverTo,
  onClose,
}: Omit<Props, "open">) {
  const [loaded, setLoaded] = useState<string[] | null | undefined>(undefined);
  const [choice, setChoice] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const confirmRef = useRef<HTMLButtonElement>(null);
  const cancelRef = useRef<HTMLButtonElement>(null);
  const dialogRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (given) return;
    let cancelled = false;
    void fetchAccountTargets(sessionId).then((names) => {
      if (!cancelled) setLoaded(names);
    });
    return () => {
      cancelled = true;
    };
  }, [given, sessionId]);

  const targets = given ?? loaded;
  const selected = choice ?? targets?.[0] ?? null;

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !submitting) onClose();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [submitting, onClose]);

  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    return () => previous?.focus?.();
  }, []);

  // Confirm is disabled until the names load, so focus Cancel meanwhile and move on once it can take it.
  const ready = !!selected && !failoverTo;
  useEffect(() => {
    const frame = requestAnimationFrame(() => (ready ? confirmRef : cancelRef).current?.focus());
    return () => cancelAnimationFrame(frame);
  }, [ready]);

  // Tab and Shift+Tab wrap instead of reaching the page behind the dialog.
  const trapTab = (e: ReactKeyboardEvent) => {
    if (e.key !== "Tab") return;
    if (submitting) {
      e.preventDefault();
      return;
    }
    // The radio group is one tab stop, its checked radio, so that is the first stop, not the first radio.
    const items = Array.from(
      dialogRef.current?.querySelectorAll<HTMLElement>("button:not([disabled]), input:not([disabled])") ?? [],
    ).filter((el) => !(el instanceof HTMLInputElement && el.type === "radio" && !el.checked));
    const first = items[0];
    const last = items[items.length - 1];
    const wrapTo = e.shiftKey ? document.activeElement === first && last : document.activeElement === last && first;
    if (wrapTo) {
      e.preventDefault();
      wrapTo.focus();
    }
  };

  const confirm = async () => {
    if (!selected || failoverTo) return;
    setSubmitting(true);
    setError(null);
    // Submitting disables every control, which would drop focus to the page behind, so park it here.
    dialogRef.current?.focus();
    const result = await switchSessionAccount(sessionId, selected);
    setSubmitting(false);
    if (result.ok) onClose();
    else {
      setError(result.message ?? "Switching account failed.");
      requestAnimationFrame(() => confirmRef.current?.focus());
    }
  };

  const identity = [authStatus?.label, authStatus?.account?.plan].filter((d): d is string => !!d);
  const loading = targets === undefined;

  return (
    <div
      ref={dialogRef}
      onKeyDown={trapTab}
      tabIndex={-1}
      role="dialog"
      aria-modal="true"
      aria-labelledby="switch-account-title"
      data-testid="switch-account-modal"
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 px-4"
      onClick={(e) => {
        if (e.target === e.currentTarget && !submitting) onClose();
      }}
    >
      <div className="w-full max-w-lg rounded-lg border border-surface-700 bg-surface-900 p-5 text-text-primary shadow-xl">
        <h2 id="switch-account-title" className="text-base font-semibold">
          Switch account
        </h2>
        <p className="mt-1 text-xs text-text-muted">
          Currently <span className="font-medium text-text-primary">{currentTool}</span>
          {identity.length > 0 && <> ({[...new Set(identity)].join(" · ")})</>}. The conversation continues on the
          account you pick.
        </p>
        <ul className="mt-2 list-disc space-y-1 pl-4 text-xs text-text-muted">
          <li>The transcript is copied into the other account&apos;s store; the original stays where it is.</li>
          <li>The next turn runs uncached on the new account, so it can take longer and use more of its limit.</li>
        </ul>

        {loading ? (
          <div className="mt-4 text-xs text-text-muted">Loading accounts...</div>
        ) : !targets || targets.length === 0 ? (
          <div className="mt-4 text-xs text-text-muted" data-testid="switch-account-empty">
            {targets === null
              ? "Couldn't list the accounts. Reload to try again."
              : "No other account can take this conversation over. Declare another tool for the same agent with its own agent_config_dir in the aoe config."}
          </div>
        ) : (
          <ul className="mt-4 max-h-64 space-y-1 overflow-y-auto">
            {targets.map((name) => (
              <li key={name}>
                <label
                  className={`flex cursor-pointer items-center gap-3 rounded border px-3 py-2 text-sm transition-colors ${
                    selected === name ? "border-brand-500 bg-brand-900/30" : "border-surface-700 hover:bg-surface-800"
                  }`}
                >
                  <input
                    type="radio"
                    name="account-target"
                    value={name}
                    checked={selected === name}
                    onChange={() => setChoice(name)}
                    disabled={submitting}
                  />
                  {name}
                </label>
              </li>
            ))}
          </ul>
        )}

        {failoverTo && (
          <div className="mt-3 text-xs text-text-muted" data-testid="switch-account-failover">
            Already switching to {failoverTo}. Wait for it to finish before choosing an account.
          </div>
        )}

        {error && (
          <div className="mt-3 text-xs text-status-error" role="alert" data-testid="switch-account-error">
            {error}
          </div>
        )}

        <div className="mt-5 flex justify-end gap-2">
          <button
            ref={cancelRef}
            type="button"
            onClick={onClose}
            disabled={submitting}
            className="rounded border border-surface-700 px-3 py-1 text-xs font-medium hover:bg-surface-800 disabled:cursor-not-allowed disabled:opacity-60"
          >
            Cancel
          </button>
          <button
            ref={confirmRef}
            type="button"
            data-testid="switch-account-confirm"
            onClick={() => void confirm()}
            disabled={!selected || submitting || !!failoverTo}
            className="rounded border border-brand-700 bg-brand-900/40 px-3 py-1 text-xs font-medium text-brand-100 hover:bg-brand-900/60 disabled:cursor-not-allowed disabled:opacity-60"
          >
            {submitting ? "Switching..." : `Switch to ${selected ?? ""}`}
          </button>
        </div>
      </div>
    </div>
  );
}
