import { ShieldAlert } from "lucide-react";
import { useId, useRef, useState } from "react";

import { restartSession } from "../../lib/api";
import type { ShellMode } from "../PairedTerminal";
import { useOutsideDismiss } from "../useOutsideDismiss";
import { signInSteps } from "./signInSteps";
import { useCopy } from "./useCopy";

/** What the sign-in popover needs from the Structured view it sits in. */
export interface SignInContext {
  sessionId: string;
  /** Registry key of the Agent; picks its steps. */
  agent: string | null | undefined;
  agentName: string;
  sandboxed: boolean;
  /** Omitted where the paired terminal can't be used. */
  onOpenTerminal?: (mode: ShellMode) => void;
  canRestart: boolean;
}

const ACTION_CLASS =
  "inline-flex min-h-8 items-center rounded-md border border-surface-700 bg-surface-900 px-2.5 text-xs text-text-secondary transition-colors hover:bg-surface-800 hover:text-text-primary disabled:opacity-60";

function Code({ children }: { children: React.ReactNode }) {
  return <code className="rounded bg-surface-950 px-1 font-mono text-[12px]">{children}</code>;
}

/** The composer's Not logged in label: a button that opens the Agent's sign-in steps. */
export function SignInButton({
  label,
  explanation,
  signIn,
}: {
  label: string;
  explanation: string;
  signIn?: SignInContext;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLSpanElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const popoverId = useId();
  const descriptionId = useId();
  useOutsideDismiss(open, [ref], () => setOpen(false));
  const close = () => {
    setOpen(false);
    triggerRef.current?.focus();
  };
  return (
    <span
      ref={ref}
      className="relative inline-flex"
      onKeyDown={(e) => {
        if (e.key !== "Escape" || !open) return;
        // Keeps the composer's own Escape handling out of it.
        e.stopPropagation();
        close();
      }}
    >
      <button
        ref={triggerRef}
        type="button"
        data-testid="composer-auth-status"
        data-auth-kind="none"
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-controls={open ? popoverId : undefined}
        aria-describedby={descriptionId}
        onClick={() => setOpen((o) => !o)}
        className="inline-flex min-h-8 max-w-[12rem] items-center gap-1 rounded-md border border-status-error/50 px-2 text-[11px] text-status-error-text transition-colors hover:bg-surface-800"
      >
        <ShieldAlert className="size-3 shrink-0" aria-hidden />
        <span className="truncate">{label}</span>
      </button>
      <span id={descriptionId} className="sr-only">
        {explanation}
      </span>
      {open && <SignInPanel id={popoverId} signIn={signIn} onLeave={() => setOpen(false)} />}
    </span>
  );
}

function SignInPanel({ id, signIn, onLeave }: { id: string; signIn?: SignInContext; onLeave: () => void }) {
  const headingId = useId();
  const name = signIn?.agentName ?? "The Agent";
  const sandboxed = signIn?.sandboxed ?? false;
  const steps = signInSteps(signIn?.agent, name, sandboxed);
  const [copied, copy] = useCopy();
  const [restart, setRestart] = useState<{ state: "idle" | "restarting" | "failed"; message?: string }>({
    state: "idle",
  });
  const restartAgent = async (sessionId: string) => {
    setRestart({ state: "restarting" });
    const result = await restartSession(sessionId);
    if (result.ok) onLeave();
    else setRestart({ state: "failed", message: result.message });
  };
  return (
    <div
      id={id}
      role="dialog"
      aria-labelledby={headingId}
      data-testid="sign-in-popover"
      className="absolute bottom-full left-0 z-30 mb-2 w-80 max-w-[calc(100vw-2rem)] rounded-lg border border-surface-700 bg-surface-850 p-3 text-xs text-text-secondary shadow-xl"
    >
      <h2 id={headingId} className="text-sm font-medium text-text-primary">
        {name} isn't signed in
      </h2>
      {steps.command ? (
        <>
          <p className="mt-2">Run this in a {sandboxed ? "Container" : "host"} terminal:</p>
          <div className="mt-1 flex items-center gap-2 rounded-md border border-surface-700 bg-surface-950 py-1 pl-2 pr-1">
            <code
              data-testid="sign-in-command"
              className="min-w-0 flex-1 break-all font-mono text-[12px] text-text-primary"
            >
              {steps.command}
            </code>
            <button type="button" className={ACTION_CLASS} onClick={() => void copy(steps.command!)}>
              {copied ? "Copied" : "Copy command"}
            </button>
          </div>
          <p className="mt-2">
            Then restart the Agent so it reads the new sign-in. Or set <Code>{steps.apiKeyEnv}</Code> in the environment
            that runs <Code>aoe serve</Code> and restart <Code>aoe serve</Code>.
          </p>
        </>
      ) : (
        <p className="mt-2">
          Sign in to {name}, then restart the Agent. Or set its API key in the environment that runs{" "}
          <Code>aoe serve</Code> and restart <Code>aoe serve</Code>.
        </p>
      )}
      {(signIn?.onOpenTerminal || signIn?.canRestart) && (
        <div className="mt-3 flex flex-wrap gap-2">
          {signIn.onOpenTerminal && (
            <button
              type="button"
              className={ACTION_CLASS}
              onClick={() => {
                signIn.onOpenTerminal?.(sandboxed ? "container" : "host");
                onLeave();
              }}
            >
              Open Terminal
            </button>
          )}
          {signIn.canRestart && (
            <button
              type="button"
              className={ACTION_CLASS}
              disabled={restart.state === "restarting"}
              onClick={() => void restartAgent(signIn.sessionId)}
            >
              {restart.state === "restarting" ? "Restarting…" : "Restart Agent"}
            </button>
          )}
        </div>
      )}
      {restart.state === "failed" && (
        <p role="status" className="mt-2 text-status-error-text">
          Couldn't restart the Agent{restart.message ? `: ${restart.message}` : "."}
        </p>
      )}
    </div>
  );
}
