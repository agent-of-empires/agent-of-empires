import { useCallback, useEffect, useRef, useState } from "react";
import { dismissSandboxImage, fetchSandboxImageStatus, pullSandboxImage, type SandboxImageStatus } from "../lib/api";
import { onServerDownChange } from "../lib/connectionState";

const POLL_MINUTES = 60;
// Often enough that "updated" follows a pull closely; the Daemon answers from memory.
const PULL_POLL_MS = 2000;
// How long "Downloading" stands in for a pull no status has shown. The Daemon answers Update now as
// soon as it starts the pull, so a reply still missing after this is lost, and statuses are believed.
const UNCONFIRMED_PULL_MS = 30_000;

export type SandboxImageBannerView =
  | { kind: "available"; image: string; onUpdate: () => void; onDismiss: () => void }
  | { kind: "gated"; image: string; command: string; onDismiss: () => void }
  | { kind: "updating" }
  | { kind: "done"; onDismiss: () => void }
  | { kind: "failed"; error: string; onRetry: () => void; onDismiss: () => void }
  | { kind: "config_unreadable"; onDismiss: () => void };

/** The sandbox image banner to show, or `null`. Shown only when checks are on and a Sandbox is in use. */
export function useSandboxImageBanner(): SandboxImageBannerView | null {
  const [status, setStatus] = useState<SandboxImageStatus | null>(null);
  const [dismissedDigest, setDismissedDigest] = useState<string | null>(null);
  // The Daemon keeps its last pull result, so "updated" and "failed" show only for a pull this
  // page saw running; otherwise every reload would repeat an old result.
  const [watchedPull, setWatchedPull] = useState(false);
  const [noticeDismissed, setNoticeDismissed] = useState(false);
  // Why the Daemon refused the last "Update now"; nothing was pulled.
  const [refusal, setRefusal] = useState<string | null>(null);
  // Set from "Update now" until a status answer confirms the pull, so polling stays fast even when
  // the status request itself fails.
  const expectingPull = useRef(false);
  // Set once a status shows the pull this page asked for running; a failed reply arriving after
  // that can't undo it, since the Daemon starts the pull before it answers.
  const pullConfirmed = useRef(false);
  // When "Update now" was pressed, while its reply is still out; null once it lands.
  const replyPendingSince = useRef<number | null>(null);
  // Each "Update now" or Retry is a new attempt; a reply to an older one changes nothing.
  const attempt = useRef(0);
  // The digest the latest status offered, and the one the current attempt pulled, so a finished
  // pull can tell a newer image published since from the one it just pulled.
  const offeredDigest = useRef<string | null>(null);
  const [pulledDigest, setPulledDigest] = useState<string | null>(null);
  // A status showed the current attempt's pull running, so its outcome is this attempt's.
  const [sawRunning, setSawRunning] = useState(false);
  // The Daemon refuses checks and pulls while it can't read its configuration.
  const [configUnreadable, setConfigUnreadable] = useState(false);
  const [configNoticeDismissed, setConfigNoticeDismissed] = useState(false);
  const pollNow = useRef<() => void>(() => {});

  useEffect(() => {
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | null = null;
    let latest = 0;

    // Only the newest poll applies its answer and schedules the next, so a slow older reply
    // can't restore a stale state.
    const poll = async () => {
      const seq = ++latest;
      const s = await fetchSandboxImageStatus();
      if (cancelled || seq !== latest) return;
      // A status always has `state`; a failed check carries an `error` too, so `error` alone
      // doesn't mark a refusal.
      const answer = s && "state" in s ? s : null;
      const refused = s && !("state" in s) ? s.error : null;
      setConfigUnreadable(refused === "config_unreadable");
      // Until the reply lands or a status shows the pull, any other pull state predates it: keep
      // showing "Downloading" and polling fast rather than flashing the old state back. Only for a
      // while: a reply that never comes can't hide a pull that finished unseen.
      const since = replyPendingSince.current;
      const predatesPull =
        !!answer &&
        since !== null &&
        Date.now() - since < UNCONFIRMED_PULL_MS &&
        !pullConfirmed.current &&
        answer.pull.state !== "running";
      if (answer) {
        offeredDigest.current = answer.state === "available" ? answer.remote_digest : null;
        setStatus(predatesPull ? { ...answer, pull: { state: "running" } } : answer);
        if (answer.pull.state === "running") {
          pullConfirmed.current = true;
          setSawRunning(true);
          setWatchedPull(true);
          // The Daemon is pulling after all, e.g. the "Update now" reply was lost on the way.
          setRefusal(null);
        }
        // The cap only limits what's shown: while the reply is out, keep watching for the pull.
        expectingPull.current = predatesPull || answer.pull.state === "running" || since !== null;
      }
      if (timer) clearTimeout(timer);
      const pulling = expectingPull.current;
      timer = setTimeout(poll, pulling ? PULL_POLL_MS : POLL_MINUTES * 60_000);
    };
    pollNow.current = () => {
      if (timer) clearTimeout(timer);
      void poll();
    };

    const initialTimer = setTimeout(() => void poll(), 0);
    const onVisibility = () => {
      if (document.visibilityState === "visible") pollNow.current();
    };
    document.addEventListener("visibilitychange", onVisibility);
    const offServerDown = onServerDownChange((down) => {
      if (!down) pollNow.current();
    });

    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
      clearTimeout(initialTimer);
      document.removeEventListener("visibilitychange", onVisibility);
      offServerDown();
    };
  }, []);

  const update = useCallback(() => {
    const mine = ++attempt.current;
    setPulledDigest(offeredDigest.current);
    setSawRunning(false);
    setWatchedPull(true);
    setNoticeDismissed(false);
    setRefusal(null);
    expectingPull.current = true;
    pullConfirmed.current = false;
    replyPendingSince.current = Date.now();
    setStatus((s) => s && { ...s, pull: { state: "running" } });
    void pullSandboxImage().then((reply) => {
      if (mine !== attempt.current) return;
      replyPendingSince.current = null;
      if (!reply.ok && !pullConfirmed.current) {
        // Nothing was pulled, and the Daemon may still hold an older pull's outcome. No status has
        // shown a pull since the click, so the "running" in `status` is this page's own guess.
        expectingPull.current = false;
        setWatchedPull(false);
        setStatus((s) => s && (s.pull.state === "running" ? { ...s, pull: { state: "idle" } } : s));
        setRefusal(reply.message ?? "Couldn't reach the Daemon.");
        // The user may have dismissed an older outcome shown meanwhile; this refusal is new.
        setNoticeDismissed(false);
      }
      pollNow.current();
    });
    // Poll fast from the click, not from the reply: the Daemon starts the pull before it answers,
    // and the reply has no deadline.
    pollNow.current();
  }, []);

  if (configUnreadable) {
    return configNoticeDismissed
      ? null
      : { kind: "config_unreadable", onDismiss: () => setConfigNoticeDismissed(true) };
  }
  if (!status || !status.checks_enabled || !status.sandbox_in_use) return null;

  if (refusal !== null && !noticeDismissed) {
    return { kind: "failed", error: refusal, onRetry: update, onDismiss: () => setNoticeDismissed(true) };
  }
  const { pull } = status;
  if (pull.state === "running") return { kind: "updating" };
  if (watchedPull && !noticeDismissed) {
    const onDismiss = () => setNoticeDismissed(true);
    // An image published since this pull is news again; the one it pulled isn't.
    const newer = status.state === "available" && status.remote_digest !== pulledDigest;
    // A `done` no status tied to this attempt, while an image is still on offer, may be an older
    // pull's: show the offer rather than success.
    const unconfirmed = !sawRunning && status.state === "available";
    if (pull.state === "done" && !newer && !unconfirmed) return { kind: "done", onDismiss };
    if (pull.state === "failed") return { kind: "failed", error: pull.error, onRetry: update, onDismiss };
  }

  if (status.state !== "available" || status.dismissed || dismissedDigest === status.remote_digest) return null;
  const digest = status.remote_digest;
  // Hidden for this page whether or not the Daemon saves it, as for the AoE update banner: read-only
  // and CityHall servers refuse to save a dismissal, so rolling back would make Dismiss do nothing.
  const onDismiss = () => {
    setDismissedDigest(digest);
    void dismissSandboxImage(digest);
  };
  if (status.pull_blocked) {
    return { kind: "gated", image: status.image, command: status.pull_command, onDismiss };
  }
  return { kind: "available", image: status.image, onUpdate: update, onDismiss };
}
