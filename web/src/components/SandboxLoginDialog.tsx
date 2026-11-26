// Logs the sandboxes in from the dashboard: the sign-in link, then the code the login client shows after signing in.
// The code lives only in component state, is cleared on submit and close, and never reaches a URL, storage or log.

import { useCallback, useEffect, useRef, useState } from "react";
import { BUTTON_PRIMARY } from "../lib/surfaces";
import { createPortal } from "react-dom";

import {
  cancelSandboxLogin,
  getSandboxLoginOutcome,
  getSandboxLoginStatus,
  leaveSandboxLogin,
  startSandboxLogin,
  submitSandboxLoginCode,
  type SandboxLoginFailure,
  type SandboxLoginState,
} from "../lib/api";
import { writeClipboard } from "../lib/clipboard";
import { CancelButton, Dialog, Spinner } from "./Dialog";

type Phase =
  | { kind: "starting" }
  | { kind: "ready"; loginId: string; url: string; submitting: boolean; notice: string | null }
  /** The login ended (rejected code, timeout): only a new start gives a new link. */
  | { kind: "ended"; message: string }
  | { kind: "start_error"; failure: SandboxLoginFailure };

/** The dialog's own lease, chosen before its start so a lost answer cannot strand it. Works on plain HTTP too. */
function newLease(): string {
  return Array.from(globalThis.crypto.getRandomValues(new Uint8Array(16)), (b) => b.toString(16).padStart(2, "0")).join(
    "",
  );
}

/** The answer to a start never came, or the server failed: it may have joined the dialog all the same. */
function startMayHaveJoined(failure: SandboxLoginFailure): boolean {
  return failure.status === 0 || failure.status >= 500;
}

const ELEVATION_START_NOTICE = "Confirm your passphrase when asked, then press Log in again.";
const ELEVATION_SUBMIT_NOTICE = "Confirm your passphrase, then press Submit again.";
const TOO_LARGE_NOTICE = "That code is too long. Paste only the code the sign-in page shows.";
const NOT_FOUND_NOTICE = "That login ended. Start again for a new link.";
const OUTCOME_UNSETTLED_NOTICE = "Could not confirm the sign-in yet. Press Submit to check again.";

const isHttps = (url: string) => /^https:\/\//i.test(url);

const DIALOG_SELECTOR = '[data-testid="sandbox-login-dialog"]';

/** Another modal, such as the passphrase prompt, holds focus above this dialog and owns Tab and Escape. */
function otherModalHasFocus() {
  const modal = document.activeElement?.closest('[role="dialog"]');
  return !!modal && !modal.matches(DIALOG_SELECTOR);
}

/** Tab and Shift+Tab wrap inside the dialog instead of reaching the page behind it. */
function trapTab(event: KeyboardEvent) {
  const dialog = document.querySelector<HTMLElement>(DIALOG_SELECTOR);
  const items = dialog?.querySelectorAll<HTMLElement>("button:not([disabled]), a[href], input");
  if (!items || items.length === 0) return;
  const first = items[0];
  const last = items[items.length - 1];
  const active = document.activeElement;
  const inside = dialog?.contains(active) ?? false;
  const wrapTo = !inside ? first : event.shiftKey ? active === first && last : active === last && first;
  if (wrapTo) {
    event.preventDefault();
    wrapTo.focus();
  }
}

export function SandboxLoginDialog({
  force = false,
  tool,
  command,
  profile,
  onClose,
  onLoggedIn,
}: {
  /** Log in again although the credential is usable. */
  force?: boolean;
  /** The agent whose shared credential file to log in: a waiting session's tool. The server picks the profile's default when omitted. */
  tool?: string;
  /** The command the server's login-required error named for that session; the login runs for the file of that command. */
  command?: string;
  /** The profile the server's login-required error named for that session; the login runs in that profile's file. The daemon's profile when omitted. */
  profile?: string;
  onClose: () => void;
  onLoggedIn?: () => void;
}) {
  const [phase, setPhase] = useState<Phase>({ kind: "starting" });
  const [code, setCode] = useState("");
  const [copied, setCopied] = useState(false);
  const mountedRef = useRef(false);
  const closedRef = useRef(false);
  const startedRef = useRef(false);
  // Set when an answer to a code got lost, until a new login starts: the code may have landed.
  const networkFailedRef = useRef(false);
  const loginIdRef = useRef<string | null>(null);
  // Names this dialog's part of the login, which other dialogs for the same credential share.
  const [ownLease] = useState(newLease);
  const leaseRef = useRef(ownLease);
  // Set when a start's answer got lost: the server may hold the lease although no login id is known.
  const leaseStrandedRef = useRef(false);
  const submitPendingRef = useRef(false);
  const inputRef = useRef<HTMLInputElement>(null);

  // StrictMode's simulated unmount runs before `start` returns, so no login id exists to cancel yet.
  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      const loginId = loginIdRef.current;
      loginIdRef.current = null;
      // A cancel now would kill a code exchange that may be about to write the credential.
      if (loginId && !submitPendingRef.current) void cancelSandboxLogin(loginId, leaseRef.current);
      else if (!loginId && leaseStrandedRef.current) void leaveSandboxLogin(leaseRef.current);
    };
  }, []);

  const start = useCallback(
    async (forceLogin: boolean) => {
      setPhase({ kind: "starting" });
      setCode("");
      setCopied(false);
      networkFailedRef.current = false;
      const result = await startSandboxLogin(forceLogin, tool, command, profile, leaseRef.current);
      if (!mountedRef.current || closedRef.current) {
        if (result.kind === "started") void cancelSandboxLogin(result.loginId, result.lease);
        else if (startMayHaveJoined(result)) void leaveSandboxLogin(leaseRef.current);
        return;
      }
      if (result.kind === "started") {
        loginIdRef.current = result.loginId;
        leaseRef.current = result.lease ?? leaseRef.current;
        leaseStrandedRef.current = false;
        setPhase({ kind: "ready", loginId: result.loginId, url: result.url, submitting: false, notice: null });
      } else {
        loginIdRef.current = null;
        // A refused retry says nothing about an earlier start whose answer got lost.
        leaseStrandedRef.current ||= startMayHaveJoined(result);
        setPhase({ kind: "start_error", failure: result });
      }
    },
    [tool, command, profile],
  );

  useEffect(() => {
    if (startedRef.current) return;
    startedRef.current = true;
    void start(force);
  }, [start, force]);

  const submitting = phase.kind === "ready" && phase.submitting;

  const close = () => {
    closedRef.current = true;
    setCode("");
    const loginId = loginIdRef.current;
    loginIdRef.current = null;
    if (loginId) void cancelSandboxLogin(loginId, leaseRef.current);
    else if (leaseStrandedRef.current) void leaveSandboxLogin(leaseRef.current);
    onClose();
  };

  // Cancelling mid token exchange would end a login that may be about to succeed.
  const dismiss = () => {
    if (!submitting) close();
  };

  // The opener gets focus back when the dialog goes; focus moves in at once, before the link arrives.
  useEffect(() => {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    document.querySelector<HTMLElement>(`${DIALOG_SELECTOR} button:not([disabled])`)?.focus();
    return () => opener?.focus();
  }, []);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (otherModalHasFocus()) return;
      if (event.key === "Escape") dismiss();
      if (event.key === "Tab") trapTab(event);
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  });

  const finish = () => {
    loginIdRef.current = null;
    onLoggedIn?.();
    onClose();
  };

  const submit = async () => {
    if (phase.kind !== "ready" || phase.submitting) return;
    const value = code.trim();
    if (!value) return;
    const { loginId, url } = phase;
    const lease = leaseRef.current;
    setCode("");
    setPhase({ kind: "ready", loginId, url, submitting: true, notice: null });
    submitPendingRef.current = true;
    const result = await submitSandboxLoginCode(loginId, value);
    submitPendingRef.current = false;
    if (!mountedRef.current) {
      // The server settles an exchange itself; a refusal or a request that never arrived leaves
      // the login running, and the lease has to go.
      const stillRunning =
        result.kind !== "ok" &&
        (result.code === "invalid_code" ||
          result.code === "elevation_required" ||
          result.status === 413 ||
          result.status === 0);
      if (stillRunning) void cancelSandboxLogin(loginId, lease);
      return;
    }
    if (result.kind === "ok") {
      finish();
      return;
    }
    const notFound = result.code === "login_not_found";
    if (notFound && networkFailedRef.current) {
      // The first attempt may have landed although its answer got lost: the server knows what
      // this login's exchange came to, which a usable credential from before it cannot tell.
      const outcome = await getSandboxLoginOutcome(loginId);
      if (!mountedRef.current) return;
      if (outcome === "succeeded") {
        finish();
        return;
      }
      if (outcome === null || outcome === "pending") {
        // Not terminal: keep the login id and the lost answer, so Submit reads the outcome again.
        setCode(value);
        setPhase({ kind: "ready", loginId, url, submitting: false, notice: OUTCOME_UNSETTLED_NOTICE });
        inputRef.current?.focus();
        return;
      }
    }
    const elevation = result.code === "elevation_required";
    // The server refuses an oversized body before it reaches the login, which keeps running.
    const tooLarge = result.status === 413;
    // A refusal that stays on this login never reached its exchange, so a lost answer stays unresolved.
    if (result.status === 0) networkFailedRef.current = true;
    if (result.code === "invalid_code" || elevation || tooLarge || result.status === 0) {
      setCode(value);
      setPhase({
        kind: "ready",
        loginId,
        url,
        submitting: false,
        notice: elevation ? ELEVATION_SUBMIT_NOTICE : tooLarge ? TOO_LARGE_NOTICE : result.message,
      });
      // A readOnly field, unlike a disabled one, can take focus while the answer is still pending.
      inputRef.current?.focus();
      return;
    }
    loginIdRef.current = null;
    setPhase({ kind: "ended", message: notFound ? NOT_FOUND_NOTICE : result.message });
  };

  const copyLink = async (url: string) => {
    setCopied(await writeClipboard(url));
  };

  let body: React.ReactNode;
  let footer: React.ReactNode;
  if (phase.kind === "starting") {
    body = (
      <div className="flex items-center gap-2 text-xs text-text-muted">
        <Spinner /> Starting the login...
      </div>
    );
    footer = <CancelButton onClick={close} />;
  } else if (phase.kind === "ready") {
    body = (
      <form
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <p className="text-xs text-text-muted">
          Sign in in your browser, then paste the code it shows. The login applies to every sandbox of this agent.
        </p>
        <div className="mt-3 flex items-center gap-2">
          {isHttps(phase.url) && (
            <a
              href={phase.url}
              target="_blank"
              rel="noopener noreferrer"
              className="text-sm text-brand-500 underline hover:text-brand-400"
            >
              Open the page to sign in
            </a>
          )}
          <button
            type="button"
            onClick={() => void copyLink(phase.url)}
            className="rounded-md border border-surface-700/60 px-2 py-1 text-xs text-text-secondary hover:bg-surface-700/40"
          >
            Copy link
          </button>
          {copied && <span className="text-xs text-text-muted">Copied</span>}
        </div>
        <label htmlFor="sandbox-login-code" className="mt-4 block text-xs text-text-secondary">
          Sign-in code
        </label>
        <input
          id="sandbox-login-code"
          name="sandbox-login-code"
          ref={inputRef}
          autoFocus
          type="password"
          autoComplete="off"
          spellCheck={false}
          data-1p-ignore
          data-lpignore="true"
          value={code}
          onChange={(e) => setCode(e.target.value)}
          readOnly={phase.submitting}
          className="mt-1 w-full rounded-lg border border-surface-700/60 bg-surface-900 px-3 py-2.5 text-sm text-text-primary focus:outline-none focus:ring-2 focus:ring-brand-600 read-only:opacity-50"
        />
        {phase.notice && (
          <p role="alert" className="mt-3 text-xs text-status-error">
            {phase.notice}
          </p>
        )}
      </form>
    );
    footer = (
      <>
        <CancelButton onClick={close} disabled={phase.submitting} />
        <button
          type="button"
          onClick={() => void submit()}
          disabled={phase.submitting || !code.trim()}
          className={BUTTON_PRIMARY}
        >
          {phase.submitting ? "Submitting..." : "Submit"}
        </button>
      </>
    );
  } else if (phase.kind === "ended") {
    body = (
      <p role="alert" className="text-xs text-status-error">
        {phase.message}
      </p>
    );
    footer = (
      <>
        <CancelButton onClick={close} />
        <ActionButton label="Start again" onClick={() => void start(force)} />
      </>
    );
  } else {
    const { failure } = phase;
    const elevation = failure.code === "elevation_required";
    const alreadyOk = failure.code === "sandbox_login_already_ok";
    body = (
      <p role="alert" className="text-xs text-status-error">
        {elevation ? ELEVATION_START_NOTICE : failure.message}
      </p>
    );
    footer = (
      <>
        <CancelButton onClick={close} />
        <ActionButton
          label={elevation ? "Log in" : alreadyOk ? "Log in again" : "Try again"}
          onClick={() => void start(alreadyOk ? true : force)}
        />
      </>
    );
  }

  // Outside the page's own tree, so a disabled ancestor (Settings' offline fieldset) cannot disable its controls.
  return createPortal(
    <Dialog id="sandbox-login-dialog" title="Sandbox login" describedBy={false} onDismiss={dismiss} footer={footer}>
      {body}
    </Dialog>,
    document.body,
  );
}

function ActionButton({ label, onClick }: { label: string; onClick: () => void }) {
  return (
    <button type="button" onClick={onClick} className={BUTTON_PRIMARY}>
      {label}
    </button>
  );
}

const STATUS_TEXT: Record<Exclude<SandboxLoginState, "not_applicable">, string> = {
  needed: "Sandbox login: needed",
  ok: "Sandbox login: ok",
};

/** The login status with a Log in button; nothing unless the setting applies. */
export function SandboxLoginStatusRow({
  profile,
  reloadKey,
  readOnly = false,
}: {
  profile?: string;
  reloadKey?: number;
  /** The server refuses the login: show the status without the button. */
  readOnly?: boolean;
}) {
  // The status carries its profile, so another profile's answer is never shown for this one.
  const [read, setRead] = useState<{ profile?: string; state: SandboxLoginState | null } | null>(null);
  // The profile the dialog was opened for: a dialog never carries over to another profile.
  const [openFor, setOpenFor] = useState<{ profile?: string } | null>(null);
  if (openFor && openFor.profile !== profile) setOpenFor(null);
  const open = openFor !== null;

  const [reloads, setReloads] = useState(0);

  // The status follows the profile on screen and re-reads when `reloadKey` changes, as it does after the credential mode is saved.
  useEffect(() => {
    let cancelled = false;
    void getSandboxLoginStatus(undefined, undefined, profile).then((status) => {
      if (!cancelled) setRead({ profile, state: status?.state ?? null });
    });
    return () => {
      cancelled = true;
    };
  }, [reloads, profile, reloadKey]);

  const state = read && read.profile === profile ? read.state : null;
  if (!state || state === "not_applicable") return null;

  return (
    <div className="mb-4 flex items-center justify-between gap-3 rounded-lg border border-surface-700/40 px-4 py-3">
      <span className="text-sm text-text-primary">{STATUS_TEXT[state]}</span>
      {!readOnly && (
        <button
          type="button"
          onClick={() => setOpenFor({ profile })}
          className="rounded-md border border-surface-700/60 px-3 py-1 text-xs text-text-secondary hover:bg-surface-700/40"
        >
          {state === "ok" ? "Log in again" : "Log in"}
        </button>
      )}
      {open && !readOnly && (
        <SandboxLoginDialog
          force={state === "ok"}
          profile={profile}
          onClose={() => {
            setOpenFor(null);
            setReloads((n) => n + 1);
          }}
        />
      )}
    </div>
  );
}
