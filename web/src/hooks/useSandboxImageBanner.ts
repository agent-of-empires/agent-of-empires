import { useCallback, useEffect, useRef, useState } from "react";
import {
  cancelSandboxImagePull,
  dismissSandboxImage,
  fetchSandboxImageStatus,
  pullSandboxImage,
  type SandboxImagePull,
  type SandboxImageStatus,
} from "../lib/api";
import { onServerDownChange } from "../lib/connectionState";
import { onSandboxImagePulled, onSandboxImagePullStarted } from "../lib/sandboxImageEvents";

// Without a Sandbox in use, or with checks off, nothing can show; an hourly look is enough.
const POLL_MINUTES = 60;
// Often enough that a pull another client starts shows here; the Daemon answers from its cache.
const IDLE_POLL_MS = 30_000;
// Often enough that "updated" follows a pull closely; the Daemon answers from memory.
const PULL_POLL_MS = 2000;
// How long "Downloading" stands in for a pull no status has shown. The Daemon answers Update now as
// soon as it starts the pull, so a reply still missing after this is lost, and statuses are believed.
const UNCONFIRMED_PULL_MS = 30_000;

// A refused "Update now" has no runtime reason; Details shows what the Daemon said.
const REFUSED = "Couldn't download the sandbox image.";

export interface SandboxPullProgress {
  phase: "downloading" | "unpacking";
  /** Both set, or neither: a count shows only when the runtime reports both. */
  done?: number;
  known?: number;
}

export type SandboxImageBannerView =
  | { kind: "available"; image: string; onUpdate: () => void; onDismiss: () => void }
  | { kind: "gated"; image: string; command: string; onDismiss: () => void }
  /** `onCancel` is absent where the dashboard can't pull, and so can't cancel either. `cancelRefused` is
   *  why the Daemon refused the last Cancel, for Details. */
  | {
      kind: "updating";
      progress: SandboxPullProgress;
      onCancel?: () => void;
      cancelling: boolean;
      cancelRefused: string | null;
      onHide: () => void;
    }
  | { kind: "done"; onDismiss: () => void }
  /** No `onUpdate`/`onRetry` while the Daemon won't pull from the web. */
  | { kind: "cancelled"; onUpdate?: () => void; onDismiss: () => void }
  /** `error` is the runtime's output, or the Daemon's refusal, for Details. */
  | { kind: "failed"; message: string; error: string; onRetry?: () => void; onDismiss: () => void }
  | { kind: "config_unreadable"; onDismiss: () => void };

/** A running pull whose banner was hidden with ×; `onShow` brings the banner back. */
export interface SandboxPullRunning {
  progress: SandboxPullProgress;
  hidden: boolean;
  onShow: () => void;
}

export interface SandboxImageBannerState {
  /** The banner to show, or null, including while a running pull's banner is hidden. */
  view: SandboxImageBannerView | null;
  /** Set while the Daemon reports a pull running, whether or not its banner shows. */
  running: SandboxPullRunning | null;
}

export function pullProgress(pull: Extract<SandboxImagePull, { state: "running" }>): SandboxPullProgress {
  const phase = pull.phase ?? "downloading";
  const { layers_done: done, layers_known: known } = pull;
  return done !== undefined && known !== undefined && known > 0 ? { phase, done, known } : { phase };
}

/** "3 of 7 layers", or null while the runtime hasn't reported both counts. */
export function layerCount({ done, known }: SandboxPullProgress): string | null {
  return known === undefined || done === undefined ? null : `${done} of ${known} layers`;
}

/** The sandbox image banner and its running pull. Shown only when checks are on and a Sandbox is in use. */
export function useSandboxImageBanner(): SandboxImageBannerState {
  const [status, setStatus] = useState<SandboxImageStatus | null>(null);
  const [dismissedDigest, setDismissedDigest] = useState<string | null>(null);
  // The Daemon keeps its last pull result, so an outcome shows only for a pull this page saw
  // running; otherwise every reload would repeat an old result.
  const [watchedPull, setWatchedPull] = useState(false);
  const [noticeDismissed, setNoticeDismissed] = useState(false);
  // × on a running pull hides its banner until that pull ends; its outcome shows again.
  const [hidden, setHidden] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  // Why the Daemon refused the last Cancel; the pull keeps running. Kept until a click or the pull's end,
  // not the next 2 s poll, so Details stays readable.
  const [cancelRefused, setCancelRefused] = useState<string | null>(null);
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
  // When a create that pulls was sent; it may reach its pull only after its hooks.
  const createPullSince = useRef<number | null>(null);
  // Each "Update now" or Retry is a new attempt; a reply to an older one changes nothing.
  const attempt = useRef(0);
  // The image the last Cancel named; its reply and refusal belong to that image until a status names another.
  const cancelImage = useRef<string | null>(null);
  // Moves on whenever a status names another image, so a Cancel sent before that can never apply its reply.
  const cancelToken = useRef(0);
  // The digest the latest status offered, and the one the current attempt pulled, so a finished
  // pull can tell a newer image published since from the one it just pulled.
  const offeredDigest = useRef<string | null>(null);
  const [pulledDigest, setPulledDigest] = useState<string | null>(null);
  // A status showed the current attempt's pull running, or the Daemon accepted it (it claims the
  // pull before it answers), so the outcome a status shows from then on is this attempt's.
  const [sawRunning, setSawRunning] = useState(false);
  // The Daemon refuses checks and pulls while it can't read its configuration.
  const [configUnreadable, setConfigUnreadable] = useState(false);
  const [configNoticeDismissed, setConfigNoticeDismissed] = useState(false);
  const pollNow = useRef<() => void>(() => {});

  useEffect(() => {
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | null = null;
    let latest = 0;
    // Whether the last answer could show a banner, so the idle poll runs only then.
    let watching = false;

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
        watching = answer.checks_enabled && answer.sandbox_in_use;
        if (cancelImage.current !== null && cancelImage.current !== answer.image) {
          cancelImage.current = null;
          cancelToken.current++;
          setCancelling(false);
          setCancelRefused(null);
        }
        offeredDigest.current = answer.state === "available" ? answer.remote_digest : null;
        setStatus(predatesPull ? { ...answer, pull: { state: "running" } } : answer);
        if (answer.pull.state === "running") {
          pullConfirmed.current = true;
          setSawRunning(true);
          setWatchedPull(true);
          // The Daemon is pulling after all, e.g. the "Update now" reply was lost on the way.
          setRefusal(null);
        } else if (!predatesPull) {
          setHidden(false);
          setCancelRefused(null);
        }
        // The cap only limits what's shown: while the reply is out, keep watching for the pull.
        expectingPull.current = predatesPull || answer.pull.state === "running" || since !== null;
      }
      if (timer) clearTimeout(timer);
      const createPulling =
        createPullSince.current !== null && Date.now() - createPullSince.current < UNCONFIRMED_PULL_MS;
      const delay =
        expectingPull.current || createPulling ? PULL_POLL_MS : watching ? IDLE_POLL_MS : POLL_MINUTES * 60_000;
      timer = setTimeout(() => {
        // A hidden tab skips its idle polls; becoming visible polls at once.
        if (delay === IDLE_POLL_MS && document.visibilityState === "hidden") return;
        void poll();
      }, delay);
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
    // The create's pull supersedes any "Update now", refused or still awaiting its reply.
    const offPulled = onSandboxImagePulled(() => {
      attempt.current++;
      replyPendingSince.current = null;
      createPullSince.current = null;
      setRefusal(null);
      // A Cancel still out now answers an older attempt, which can't reset its button.
      setCancelling(false);
      setCancelRefused(null);
      pollNow.current();
    });
    const offStarted = onSandboxImagePullStarted(() => {
      createPullSince.current = Date.now();
      pollNow.current();
    });

    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
      clearTimeout(initialTimer);
      document.removeEventListener("visibilitychange", onVisibility);
      offServerDown();
      offPulled();
      offStarted();
    };
  }, []);

  const update = useCallback(() => {
    const mine = ++attempt.current;
    setPulledDigest(offeredDigest.current);
    setSawRunning(false);
    setWatchedPull(true);
    setNoticeDismissed(false);
    setHidden(false);
    setRefusal(null);
    setCancelling(false);
    setCancelRefused(null);
    expectingPull.current = true;
    pullConfirmed.current = false;
    replyPendingSince.current = Date.now();
    setStatus((s) => s && { ...s, pull: { state: "running" } });
    void pullSandboxImage().then((reply) => {
      if (mine !== attempt.current) return;
      replyPendingSince.current = null;
      if (reply.ok) {
        setSawRunning(true);
        // Acceptance ties what comes next to this click, not a state already on screen: until a
        // status fetched after it lands, show the pull as running.
        if (!pullConfirmed.current) setStatus((s) => s && { ...s, pull: { state: "running" } });
      }
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

  // `image` is the one on the banner, so a default changed since can't cancel a different pull.
  const cancel = useCallback((image: string) => {
    const mine = attempt.current;
    const token = cancelToken.current;
    cancelImage.current = image;
    setCancelling(true);
    setCancelRefused(null);
    void cancelSandboxImagePull(image).then((reply) => {
      // A newer update owns the banner now, or a status for another image superseded this Cancel.
      if (mine !== attempt.current || token !== cancelToken.current) return;
      setCancelling(false);
      if (!reply.ok) setCancelRefused(reply.message ?? "Couldn't reach the Daemon.");
      if (reply.pull) {
        const pull = reply.pull;
        if (pull.state !== "running") {
          // The pull is over, so a reply to "Update now" still on its way no longer decides what shows.
          attempt.current++;
          replyPendingSince.current = null;
        }
        // This page cancelled it, so the outcome is this page's to show.
        setWatchedPull(true);
        setSawRunning(true);
        setNoticeDismissed(false);
        setHidden(false);
        setStatus((s) => s && { ...s, pull });
      }
      pollNow.current();
    });
  }, []);

  const show = useCallback(() => setHidden(false), []);
  const hide = useCallback(() => {
    setHidden(true);
    setCancelRefused(null);
  }, []);

  const banner = ((): SandboxImageBannerState => {
    if (configUnreadable) {
      return {
        view: configNoticeDismissed
          ? null
          : { kind: "config_unreadable", onDismiss: () => setConfigNoticeDismissed(true) },
        running: null,
      };
    }
    if (!status || !status.checks_enabled || !status.sandbox_in_use) return { view: null, running: null };

    if (refusal !== null && !noticeDismissed) {
      return {
        view: {
          kind: "failed",
          message: REFUSED,
          error: refusal,
          onRetry: update,
          onDismiss: () => setNoticeDismissed(true),
        },
        running: null,
      };
    }
    const { pull } = status;
    if (pull.state === "running") {
      const progress = pullProgress(pull);
      const image = status.image;
      const onCancel = status.pull_blocked ? undefined : () => cancel(image);
      return {
        view: hidden ? null : { kind: "updating", progress, onCancel, cancelling, cancelRefused, onHide: hide },
        running: { progress, hidden, onShow: show },
      };
    }
    if (watchedPull && !noticeDismissed) {
      const onDismiss = () => setNoticeDismissed(true);
      const again = status.pull_blocked ? undefined : update;
      // An image published since this pull is news again; the one it pulled isn't.
      const newer = status.state === "available" && status.remote_digest !== pulledDigest;
      // An outcome no status tied to this attempt, while an image is still on offer, may be an older
      // pull's: show the offer rather than the outcome.
      const unconfirmed = !sawRunning && status.state === "available";
      if (pull.state === "done" && !newer && !unconfirmed) return { view: { kind: "done", onDismiss }, running: null };
      if (pull.state === "cancelled" && !unconfirmed) {
        return { view: { kind: "cancelled", onUpdate: again, onDismiss }, running: null };
      }
      if (pull.state === "failed" && !unconfirmed) {
        return {
          view: { kind: "failed", message: pull.message, error: pull.error, onRetry: again, onDismiss },
          running: null,
        };
      }
    }

    if (status.state !== "available" || status.dismissed || dismissedDigest === status.remote_digest) {
      return { view: null, running: null };
    }
    const digest = status.remote_digest;
    // Hidden for this page whether or not the Daemon saves it, as for the AoE update banner: read-only
    // and CityHall servers refuse to save a dismissal, so rolling back would make Dismiss do nothing.
    const onDismiss = () => {
      setDismissedDigest(digest);
      void dismissSandboxImage(digest);
    };
    if (status.pull_blocked) {
      return { view: { kind: "gated", image: status.image, command: status.pull_command, onDismiss }, running: null };
    }
    return { view: { kind: "available", image: status.image, onUpdate: update, onDismiss }, running: null };
  })();
  return banner;
}
