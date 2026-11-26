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
      /** False until the Daemon accepts this page's update of the image shown, or a status shows that image
       *  running: a Cancel sent earlier could reach the Daemon before the pull and miss it. */
      cancelReady: boolean;
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
  // The image of the attempt once a status shows it running; a failed reply arriving after that can't
  // undo it, since the Daemon starts the pull before it answers. Another image running doesn't confirm it.
  const confirmedImage = useRef<string | null>(null);
  // When "Update now" was pressed, while its reply is still out; null once it lands.
  const replyPendingSince = useRef<number | null>(null);
  // When a create that pulls was sent; it may reach its pull only after its hooks.
  const createPullSince = useRef<number | null>(null);
  // Each "Update now" or Retry is a new attempt; a reply to an older one changes nothing.
  const attempt = useRef(0);
  // The image the current attempt pulls: the banner's at the click, then the one the Daemon's reply names,
  // since it pulls the default as of the request's arrival.
  const attemptImage = useRef<string | null>(null);
  // `attemptImage` for render, which can't read a ref.
  const [attemptFor, setAttemptFor] = useState<string | null>(null);
  // The image the last Cancel named; its reply and refusal belong to that image until a status names another.
  const cancelImage = useRef<string | null>(null);
  // Moves on whenever a status names another image, so a Cancel sent before that can never apply its reply.
  const cancelToken = useRef(0);
  // The digest the latest status offered, and the one the current attempt pulled, so a finished
  // pull can tell a newer image published since from the one it just pulled.
  const offeredDigest = useRef<string | null>(null);
  const [pulledDigest, setPulledDigest] = useState<string | null>(null);
  // The image a status since the click showed running, and the one whose pull the Daemon accepted for
  // this attempt (it claims the pull before it answers): from then on, that image's outcome is this page's.
  const [seenRunning, setSeenRunning] = useState<string | null>(null);
  // `seenRunning` for the reply, which can't read state.
  const seenRunningRef = useRef<string | null>(null);
  const [acceptedImage, setAcceptedImage] = useState<string | null>(null);
  // Whether the last status showed a pull running, so a pull starting elsewhere re-arms the outcome notice.
  const polledRunning = useRef(false);
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
        confirmedImage.current !== attemptImage.current &&
        answer.image === attemptImage.current &&
        answer.pull.state !== "running";
      if (answer) {
        watching = answer.checks_enabled && answer.sandbox_in_use;
        // A Cancel belongs to the pull it was sent for: once a status names another image, or shows that
        // pull ended, its reply can't own the banner, which may already show a new pull of the same image.
        const ended = answer.pull.state !== "running" && !predatesPull;
        if (cancelImage.current !== null && (cancelImage.current !== answer.image || ended)) {
          cancelImage.current = null;
          cancelToken.current++;
          setCancelling(false);
          setCancelRefused(null);
        }
        offeredDigest.current = answer.state === "available" ? answer.remote_digest : null;
        setStatus(predatesPull ? { ...answer, pull: { state: "running" } } : answer);
        const running = answer.pull.state === "running";
        if (running && !polledRunning.current) setNoticeDismissed(false);
        polledRunning.current = running;
        if (running) {
          seenRunningRef.current = answer.image;
          setSeenRunning(answer.image);
          if (answer.image === attemptImage.current) {
            confirmedImage.current = answer.image;
            // The Daemon is pulling after all, e.g. the "Update now" reply was lost on the way.
            setRefusal(null);
          }
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

  const update = useCallback((image: string) => {
    const mine = ++attempt.current;
    attemptImage.current = image;
    setAttemptFor(image);
    setPulledDigest(offeredDigest.current);
    seenRunningRef.current = null;
    setSeenRunning(null);
    setAcceptedImage(null);
    setNoticeDismissed(false);
    setHidden(false);
    setRefusal(null);
    setCancelling(false);
    setCancelRefused(null);
    expectingPull.current = true;
    confirmedImage.current = null;
    replyPendingSince.current = Date.now();
    setStatus((s) => s && { ...s, pull: { state: "running" } });
    void pullSandboxImage().then((reply) => {
      if (mine !== attempt.current) return;
      replyPendingSince.current = null;
      if (reply.ok) {
        const pulled = reply.image ?? image;
        attemptImage.current = pulled;
        setAttemptFor(pulled);
        setAcceptedImage(pulled);
        // A poll since the click already showed this image's pull, so it says what that pull is doing.
        if (seenRunningRef.current === pulled) confirmedImage.current = pulled;
        // Acceptance ties what comes next to this click, not a state already on screen: until a
        // status fetched after it lands, show the pull as running, if it is that image's.
        if (confirmedImage.current !== pulled) {
          setStatus((s) => s && (s.image === pulled ? { ...s, pull: { state: "running" } } : s));
        }
      }
      if (!reply.ok && confirmedImage.current !== image) {
        // Nothing was pulled, and the Daemon may still hold an older pull's outcome. No status has
        // shown this image's pull since the click, so a "running" for it in `status` is this page's guess.
        expectingPull.current = false;
        // The attempt is over; a pull of another image seen meanwhile keeps its own outcome.
        setAttemptFor(null);
        setStatus(
          (s) => s && (s.image === image && s.pull.state === "running" ? { ...s, pull: { state: "idle" } } : s),
        );
        setRefusal(reply.message ?? "Couldn't reach the Daemon.");
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
        if (pull.state !== "running" && image === attemptImage.current) {
          // This attempt's pull is over, so its reply still on the way no longer decides what shows.
          attempt.current++;
          replyPendingSince.current = null;
        }
        // This page cancelled it, so the outcome is this page's to show.
        seenRunningRef.current = image;
        setSeenRunning(image);
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

  const tied = (image: string) => seenRunning === image || acceptedImage === image;
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

    // The refusal has its own dismissal: it says nothing about an outcome's notice, which stays as the user left it.
    if (refusal !== null) {
      return {
        view: {
          kind: "failed",
          message: REFUSED,
          error: refusal,
          onRetry: () => update(status.image),
          onDismiss: () => setRefusal(null),
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
        view: hidden
          ? null
          : {
              kind: "updating",
              progress,
              onCancel,
              cancelReady: tied(image),
              cancelling,
              cancelRefused,
              onHide: hide,
            },
        running: { progress, hidden, onShow: show },
      };
    }
    // The Daemon keeps its last pull result, so an outcome shows only for a pull tied to this page or for the
    // live attempt's image; otherwise every reload would repeat an old result.
    if ((tied(status.image) || status.image === attemptFor) && !noticeDismissed) {
      const onDismiss = () => setNoticeDismissed(true);
      const again = status.pull_blocked ? undefined : () => update(status.image);
      // An image published since this pull is news again; the one it pulled isn't.
      const newer = status.state === "available" && status.remote_digest !== pulledDigest;
      // An outcome nothing tied to this page may be an older pull's: the offer shows rather than it while one
      // is up, and it never shows for another image than the one the page follows, clicked or watched.
      const followed = attemptFor ?? seenRunning;
      const otherImage = followed !== null && status.image !== followed;
      const unconfirmed = !tied(status.image) && (status.state === "available" || otherImage);
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
    return {
      view: { kind: "available", image: status.image, onUpdate: () => update(status.image), onDismiss },
      running: null,
    };
  })();
  return banner;
}
