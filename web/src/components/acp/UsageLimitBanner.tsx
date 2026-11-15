import { useEffect, useState, useSyncExternalStore } from "react";
import { Link } from "react-router-dom";

import { type RespawnState, useRespawnSession } from "../../hooks/useRespawnSession";
import type { AcpState, AuthStatus } from "../../lib/acpTypes";
import { useAgentDisplayNames } from "../../lib/agentDisplayName";
import { fetchAccountTargets, fetchAgents } from "../../lib/api";
import {
  clearPendingSwitchAccount,
  getPendingSwitchAccount,
  subscribePendingSwitchAccount,
} from "../../lib/switchAccountTrigger";
import { formatDurationSecondsShort } from "../sidebar/format";
import { SwitchAccountModal } from "./SwitchAccountModal";
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
  onSwitchAccount,
  failoverTo,
  switchListFailed = false,
  accountListFailed = false,
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
  /** Omitted when no other account could keep the conversation. */
  onSwitchAccount?: () => void;
  /** The account the daemon is already moving this Session to; replaces every manual action, which would race it. */
  failoverTo?: string;
  /** Why the Agents to switch to couldn't be listed. */
  switchListFailed?: boolean;
  /** Why the accounts to switch to couldn't be listed. */
  accountListFailed?: boolean;
  /** Where the auto-resume toggle lives; null where Settings can't be edited. */
  settingsHref: string | null;
}) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const tick = setInterval(() => setNow(Date.now()), 60_000);
    return () => clearInterval(tick);
  }, []);

  const agentName = useAgentDisplayNames();
  const name = agent ? agentName(agent) : null;
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
                : "No reset time was reported."}
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
          {failoverTo && (
            <p className="mt-1 text-xs text-text-secondary" data-testid="usage-limit-failover">
              Switching to {failoverTo}…
            </p>
          )}
          {!resumeNow && !busy && !failoverTo && (
            <p className="mt-1 text-xs text-text-dim">
              Added more usage? Try again now.{reset && !lifted ? ` Otherwise the limit lifts at ${time}.` : ""}
            </p>
          )}
        </div>
        <div className="flex flex-wrap items-center gap-2">
          {onSwitchAccount && !failoverTo && (
            <button
              type="button"
              data-testid="usage-limit-switch-account"
              onClick={onSwitchAccount}
              className={`${BUTTON} border border-surface-700 bg-surface-800 text-text-primary hover:bg-surface-700`}
            >
              Switch account
            </button>
          )}
          {onSwitchAgent && !failoverTo && (
            <button
              type="button"
              onClick={onSwitchAgent}
              className={`${BUTTON} border border-surface-700 bg-surface-800 text-text-primary hover:bg-surface-700`}
            >
              Switch Agent
            </button>
          )}
          {!failoverTo && (
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
          )}
        </div>
      </div>
      {switchListFailed && (
        <p className="mt-2 text-xs text-status-error-text">
          Couldn't list the other Agents, so Switch Agent isn't offered. Reload to try again.
        </p>
      )}
      {accountListFailed && (
        <p className="mt-2 text-xs text-status-error-text">
          Couldn't list the other accounts, so Switch account isn't offered. Reload to try again.
        </p>
      )}
      {resumeState === "ok" && <p className="mt-2 text-xs text-text-dim">New events should start streaming shortly.</p>}
      {resumeState === "failed" && resumeError && (
        <p className="mt-2 text-xs text-status-error-text">Resume failed: {resumeError}</p>
      )}
    </div>
  );
}

/** Other Agents a limited Session could continue in; empty until loaded, and read only while `limited`. */
function useSwitchTargets(current: string | null, limited: boolean): { targets: string[]; listFailed: boolean } {
  const [targets, setTargets] = useState<string[]>([]);
  const [listFailed, setListFailed] = useState(false);
  useEffect(() => {
    if (!limited) return;
    let cancelled = false;
    void fetchAgents().then((agents) => {
      if (cancelled) return;
      setListFailed(agents === null);
      setTargets(
        (agents ?? [])
          .filter((a) => a.acp_capable && a.installed && a.acp_installed && a.acp_allowed !== false)
          .map((a) => a.name)
          .filter((name) => name !== current),
      );
    });
    return () => {
      cancelled = true;
    };
  }, [current, limited]);
  return { targets, listFailed };
}

/** Accounts a limited Session could swap to; empty until loaded, and read only while `limited`. */
function useAccountTargets(sessionId: string, limited: boolean): { targets: string[]; listFailed: boolean } {
  const [targets, setTargets] = useState<string[]>([]);
  const [listFailed, setListFailed] = useState(false);
  useEffect(() => {
    if (!limited) return;
    let cancelled = false;
    void fetchAccountTargets(sessionId).then((names) => {
      if (cancelled) return;
      setListFailed(names === null);
      setTargets(names ?? []);
    });
    return () => {
      cancelled = true;
    };
  }, [sessionId, limited]);
  return { targets, listFailed };
}

/** The usage-limit banner wired to resume and to the Agent handoff. Mounted whether or not
 *  the Session is limited: a handoff clears the limit before its recap arrives, and its
 *  modal must stay until it settles. */
export function UsageLimitSection({
  sessionId,
  agent,
  rateLimit,
  autoResume,
  failoverTo,
  retriesExhausted,
  settingsHref,
  onPrefill,
  currentTool,
  authStatus,
}: {
  sessionId: string;
  currentTool: string;
  authStatus: AuthStatus | null;
  agent: string | null;
  rateLimit: AcpState["rateLimit"];
  autoResume: boolean | undefined;
  failoverTo?: string;
  retriesExhausted: boolean;
  settingsHref: string | null;
  onPrefill: (text: string) => void;
}) {
  // A limit with no reported reset still needs a key, and a cleared limit ends the incident.
  const resume = useRespawnSession(sessionId, rateLimit ? (rateLimit.resets_at ?? "unknown") : null);
  const { targets, listFailed } = useSwitchTargets(agent, rateLimit !== null);
  const { targets: accountTargets, listFailed: accountListFailed } = useAccountTargets(sessionId, rateLimit !== null);
  // Also opened from the sidebar row's "Switch account" item, limited or not.
  const [accountOpen, setAccountOpen] = useState(false);
  const pendingAccount = useSyncExternalStore(subscribePendingSwitchAccount, getPendingSwitchAccount, () => null);
  const closeAccount = () => {
    setAccountOpen(false);
    clearPendingSwitchAccount();
  };
  // Taken when the modal opens, so the switch's own Agent change doesn't reload it mid-handoff.
  const [handoff, setHandoff] = useState<{ from: string | null; eligible: string[] } | null>(null);
  return (
    <>
      {rateLimit && (
        <UsageLimitBanner
          agent={agent}
          rateLimit={rateLimit}
          autoResume={autoResume}
          failoverTo={failoverTo}
          retriesExhausted={retriesExhausted}
          resumeState={resume.state}
          resumeError={resume.error}
          onResume={() => void resume.respawn()}
          onSwitchAgent={targets.length > 0 ? () => setHandoff({ from: agent, eligible: targets }) : undefined}
          onSwitchAccount={accountTargets.length > 0 ? () => setAccountOpen(true) : undefined}
          switchListFailed={listFailed}
          accountListFailed={accountListFailed}
          settingsHref={settingsHref}
        />
      )}
      <SwitchAccountModal
        open={accountOpen || pendingAccount === sessionId}
        sessionId={sessionId}
        currentTool={currentTool}
        authStatus={authStatus}
        targets={accountOpen ? accountTargets : undefined}
        failoverTo={failoverTo}
        onClose={closeAccount}
      />

      <SwitchAgentModal
        open={handoff !== null}
        sessionId={sessionId}
        currentAgent={handoff?.from ?? agent}
        eligible={handoff?.eligible ?? targets}
        onClose={() => setHandoff(null)}
        onPrefill={onPrefill}
        trigger="rate_limit"
      />
    </>
  );
}
