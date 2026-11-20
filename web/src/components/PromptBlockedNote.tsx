import { useState } from "react";
import { updateSettings } from "../lib/api";
import type { StartedSession } from "../lib/types";

const FOLDER = "agent_folder_not_trusted";

/** The server's message names config keys; the codes it knows get one plain sentence instead. */
const PLAIN: Record<string, string> = {
  [FOLDER]: "Claude Code is asking whether to trust this folder, so the prompt was not typed.",
  agent_bypass_not_accepted:
    "Claude Code is waiting for Bypass Permissions to be accepted once. Open the Session and accept it.",
};

const LINK = "cursor-pointer text-brand-500 hover:underline disabled:opacity-50";

/**
 * Why a Session's initial prompt was not typed. For an untrusted folder it offers to turn on
 * `pre_trust_agent_folders` after saying what that also allows, then Try again when given.
 */
export function PromptBlockedNote({
  label,
  code,
  message,
  onTryAgain,
  disabled = false,
}: {
  label: string;
  code?: string | null;
  message: string;
  /** Starts a fresh Session; without it the note says how to go on in this one. */
  onTryAgain?: () => void;
  disabled?: boolean;
}) {
  const [step, setStep] = useState<"idle" | "confirm" | "saving" | "on" | "failed">("idle");
  const turnOn = async () => {
    setStep("saving");
    const outcome = await updateSettings({ session: { pre_trust_agent_folders: true } });
    setStep(outcome === "committed" ? "on" : "failed");
  };
  const actions = code === FOLDER && !disabled;
  return (
    <div className="text-xs text-status-warning">
      <p role="alert" className="break-words">
        {label}: {(code && PLAIN[code]) || message}
      </p>
      {actions && (step === "idle" || step === "failed") && (
        <p>
          {step === "failed" && <span>Couldn&apos;t turn on pre-trust. </span>}
          <button type="button" className={LINK} onClick={() => setStep("confirm")}>
            Turn on pre-trust
          </button>
        </p>
      )}
      {actions && (step === "confirm" || step === "saving") && (
        <p className="text-text-secondary">
          Claude Code will trust each folder AoE starts it in. That also runs the repo&apos;s own .claude/settings.json
          hooks.{" "}
          <button type="button" className={LINK} disabled={step === "saving"} onClick={() => void turnOn()}>
            Turn on
          </button>{" "}
          <button type="button" className={LINK} disabled={step === "saving"} onClick={() => setStep("idle")}>
            Cancel
          </button>
        </p>
      )}
      {actions &&
        step === "on" &&
        (onTryAgain ? (
          <p className="text-text-secondary">
            Pre-trust is on.{" "}
            <button type="button" className={LINK} onClick={onTryAgain}>
              Try again
            </button>{" "}
            moves this Session to Trash and starts a new one.
          </p>
        ) : (
          <p className="text-text-secondary">
            Pre-trust is on: new Sessions start trusted. Trust this folder in the terminal to go on.
          </p>
        ))}
    </div>
  );
}

/** A started suggestion's row note: nothing unless its Session's prompt was not typed. */
export function StartedPromptNote({
  session,
  onTryAgain,
  disabled,
}: {
  session?: StartedSession;
  onTryAgain: () => void;
  disabled: boolean;
}) {
  if (!session?.initial_prompt_error) return null;
  return (
    <div className="mt-0.5">
      <PromptBlockedNote
        label="Prompt not sent"
        code={session.initial_prompt_error_code}
        message={session.initial_prompt_error}
        onTryAgain={onTryAgain}
        disabled={disabled}
      />
    </div>
  );
}
