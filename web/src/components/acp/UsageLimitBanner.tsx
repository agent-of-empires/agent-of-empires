import { useEffect, useState } from "react";
import { Link } from "react-router-dom";

import { type RespawnState, useRespawnSession } from "../../hooks/useRespawnSession";
import type { AcpState } from "../../lib/acpTypes";
import { agentDisplayName } from "../../lib/agentDisplayName";
import { fetchAgents } from "../../lib/api";
import { formatDurationSecondsShort } from "../sidebar/format";
import { SwitchAgentModal } from "./SwitchAgentModal";

/** The agent's rate-limit wording without transport prefixes or the trailing
 *  `{"errorKind":...}` fingerprint the connection-end path appends. */
function rateLimitWording(status: string): string {
  return status
    .replace(/[\s:]*\{[\s\S]*\}\s*$/, "")
    .replace(/^(?:ACP connection failed:\s*)?(?:Internal error:?\s*)?/, "")
    .trim();
}

const BUTTON =
  "min-h-8 shrink-0 rounded-md px-3 py-1.5 text-xs font-medium disabled:cursor-not-allowed disabled:opacity-60";

/** Shown above the composer while the Agent's usage limit pauses the Session. */
export function UsageLimitBanner({
  agent,
  rateLimit,
  autoResume,
  retriesExhausted,
  resumeState,
  resumeError,
  onResume,
  onSwitchAgent,
  settingsHref,
}: {
  agent: string | null;
  rateLimit: NonNullable<AcpState["rateLimit"]>;
  /** Unknown when undefined: then nothing is said about auto-resume. */
  autoResume: boolean | undefined;
  retriesExhausted: boolean;
  resumeState: RespawnState;
  resumeError: string | null;
  onResume: () => void;
  /** Omitted when no other installed Agent could take over. */
  onSwitchAgent?: () => void;
  /** Where the auto-resume toggle lives; null where Settings can't be edited. */
  settingsHref: string | null;
}) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const tick = setInterval(() => setNow(Date.now()), 60_000);
    return () => clearInterval(tick);
  }, []);

  const name = agent ? agentDisplayName(agent) : null;
  const parsed = rateLimit.resets_at ? new Date(rateLimit.resets_at) : null;
  const reset = parsed && !Number.isNaN(parsed.getTime()) ? parsed : null;
  const lifted = reset !== null && now >= reset.getTime();
  const time = reset?.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  const resumeNow = lifted && autoResume === false;
  const busy = resumeState === "retrying" || resumeState === "ok";
  // Without a parseable reset, a non-Claude Agent's own words beat a guess.
  const own = agent === "claude" ? "" : rateLimitWording(rateLimit.status);

  return (
    <div
      data-testid="usage-limit-banner"
      className="border-t border-surface-700/60 bg-surface-850 px-4 py-3 text-text-primary"
    >
      <div className="flex flex-wrap items-start gap-x-4 gap-y-2">
        <div className="min-w-0 flex-1 basis-56">
          <p role="status" className="text-sm font-semibold">
            {name ?? "Agent"} usage limit {lifted ? "lifted" : "reached"}
          </p>
          <p className="mt-0.5 text-xs text-text-secondary" data-testid="usage-limit-reset">
            {reset
              ? lifted
                ? `The limit reset at ${time}.`
                : `Resets ${time} · in about ${formatDurationSecondsShort(
                    Math.max(60, Math.round((reset.getTime() - now) / 60_000) * 60),
                  )}`
              : own
                ? `${name ?? "The Agent"} said: ${own}`
                : "The Agent didn't say when the limit resets."}
          </p>
          {retriesExhausted ? (
            <p className="mt-1 text-xs text-text-secondary">
              Auto-resume stopped: the same prompt was re-sent too many times without getting through. Try again or send
              a new prompt.
            </p>
          ) : autoResume === true ? (
            <p className="mt-1 text-xs text-text-secondary">Picks up automatically when the limit resets.</p>
          ) : autoResume === false ? (
            <p className="mt-1 text-xs text-text-secondary">
              Auto-resume is off.
              {settingsHref && (
                <>
                  {" "}
                  <Link to={settingsHref} className="font-medium text-brand-400 underline-offset-2 hover:underline">
                    Turn on
                  </Link>
                </>
              )}
            </p>
          ) : null}
          {!resumeNow && !busy && (
            <p className="mt-1 text-xs text-text-dim">
              Added more usage? Try again now.{reset && !lifted ? ` Otherwise the limit lifts at ${time}.` : ""}
            </p>
          )}
        </div>
        <div className="flex flex-wrap items-center gap-2">
          {onSwitchAgent && (
            <button
              type="button"
              onClick={onSwitchAgent}
              className={`${BUTTON} border border-surface-700 bg-surface-800 text-text-primary hover:bg-surface-700`}
            >
              Switch Agent
            </button>
          )}
          <button
            type="button"
            data-testid="usage-limit-resume"
            onClick={onResume}
            disabled={busy}
            className={
              resumeNow
                ? `${BUTTON} bg-brand-600 text-text-on-brand hover:ring-2 hover:ring-brand-600/50`
                : `${BUTTON} border border-surface-700 bg-surface-800 text-text-primary hover:bg-surface-700`
            }
          >
            {resumeState === "retrying"
              ? "Resuming…"
              : resumeState === "ok"
                ? "Resume requested"
                : resumeNow
                  ? "Resume now"
                  : "Try again"}
          </button>
        </div>
      </div>
      {resumeState === "ok" && <p className="mt-2 text-xs text-text-dim">New events should start streaming shortly.</p>}
      {resumeState === "failed" && resumeError && (
        <p className="mt-2 text-xs text-status-error-text">Resume failed: {resumeError}</p>
      )}
    </div>
  );
}

/** Other Agents a limited Session could continue in; empty until loaded. */
function useSwitchTargets(current: string | null): string[] {
  const [targets, setTargets] = useState<string[]>([]);
  useEffect(() => {
    let cancelled = false;
    fetchAgents()
      .then((agents) => {
        if (cancelled) return;
        setTargets(
          agents
            .filter((a) => a.acp_capable && a.installed && a.acp_installed && a.acp_allowed !== false)
            .map((a) => a.name)
            .filter((name) => name !== current),
        );
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [current]);
  return targets;
}

/** The usage-limit banner wired to resume and to the Agent handoff. */
export function UsageLimitSection({
  sessionId,
  agent,
  rateLimit,
  autoResume,
  retriesExhausted,
  settingsHref,
  onPrefill,
}: {
  sessionId: string;
  agent: string | null;
  rateLimit: NonNullable<AcpState["rateLimit"]>;
  autoResume: boolean | undefined;
  retriesExhausted: boolean;
  settingsHref: string | null;
  onPrefill: (text: string) => void;
}) {
  // A limit with no reported reset still needs a key to scope resume status by.
  const resume = useRespawnSession(sessionId, rateLimit.resets_at ?? "unknown");
  const targets = useSwitchTargets(agent);
  const [switching, setSwitching] = useState(false);
  return (
    <>
      <UsageLimitBanner
        agent={agent}
        rateLimit={rateLimit}
        autoResume={autoResume}
        retriesExhausted={retriesExhausted}
        resumeState={resume.state}
        resumeError={resume.error}
        onResume={() => void resume.respawn()}
        onSwitchAgent={targets.length > 0 ? () => setSwitching(true) : undefined}
        settingsHref={settingsHref}
      />
      <SwitchAgentModal
        open={switching}
        sessionId={sessionId}
        currentAgent={agent}
        eligible={targets}
        onClose={() => setSwitching(false)}
        onPrefill={onPrefill}
        trigger="rate_limit"
      />
    </>
  );
}
