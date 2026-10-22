import { useCallback, useEffect, useRef, useState } from "react";
import { dismissSandboxImage, fetchSandboxImageStatus, pullSandboxImage, type SandboxImageStatus } from "../lib/api";
import { onServerDownChange } from "../lib/connectionState";

const POLL_MINUTES = 60;
// Often enough that "updated" follows a pull closely; the Daemon answers from memory.
const PULL_POLL_MS = 2000;

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
      const refused = s && "error" in s ? s.error : null;
      setConfigUnreadable(refused === "config_unreadable");
      const answer = s && !("error" in s) ? s : null;
      if (answer) {
        setStatus(answer);
        if (answer.pull.state === "running") setWatchedPull(true);
      }
      if (timer) clearTimeout(timer);
      const pulling = answer?.pull.state === "running";
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
    setWatchedPull(true);
    setNoticeDismissed(false);
    setStatus((s) => s && { ...s, pull: { state: "running" } });
    void pullSandboxImage().then(() => pollNow.current());
  }, []);

  if (configUnreadable) {
    return configNoticeDismissed
      ? null
      : { kind: "config_unreadable", onDismiss: () => setConfigNoticeDismissed(true) };
  }
  if (!status || !status.checks_enabled || !status.sandbox_in_use) return null;

  const { pull } = status;
  if (pull.state === "running") return { kind: "updating" };
  if (watchedPull && !noticeDismissed) {
    const onDismiss = () => setNoticeDismissed(true);
    if (pull.state === "done") return { kind: "done", onDismiss };
    if (pull.state === "failed") return { kind: "failed", error: pull.error, onRetry: update, onDismiss };
  }

  if (status.state !== "available" || status.dismissed || dismissedDigest === status.remote_digest) return null;
  const digest = status.remote_digest;
  const onDismiss = () => {
    setDismissedDigest(digest);
    void dismissSandboxImage(digest);
  };
  if (status.pull_blocked) {
    return { kind: "gated", image: status.image, command: status.pull_command, onDismiss };
  }
  return { kind: "available", image: status.image, onUpdate: update, onDismiss };
}
