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

/** One "Update now" or Retry. Replaced as a unit by the next click; a reply applies only while its `id` is current. */
export interface Attempt {
  id: number;
  /** The banner's image at the click, then the one the Daemon's reply names: it pulls the default as of the
   *  request's arrival. */
  image: string;
  /** The digest the clicked banner offered, so a finished pull can tell a newer image published since. */
  digest: string | null;
  /** The click, for how long "Downloading" stands in for a pull no status has shown. */
  since: number;
  /** The Daemon's reply is still out. */
  replyOut: boolean;
  /** The image a status showed running for this attempt: a failed reply arriving after that can't undo it, since
   *  the Daemon starts the pull before it answers. */
  confirmed: string | null;
  /** The Daemon accepted this attempt's pull (it claims the pull before it answers). */
  accepted: boolean;
  /** Refused: the attempt owns no outcome, even once `refusal` is dismissed. */
  refused: boolean;
  /** Why the Daemon refused; nothing was pulled. Cleared by its own dismissal. */
  refusal: string | null;
}

/** The last Cancel. A reply applies only while this record's `id` is still the current one. */
export interface CancelSent {
  id: number;
  image: string;
  pending: boolean;
  /** Why the Daemon refused it; the pull keeps running. Kept until a click or the pull's end, not the next poll,
   *  so Details stays readable. */
  refused: string | null;
}

export interface BannerInputs {
  status: SandboxImageStatus | null;
  attempt: Attempt | null;
  cancel: CancelSent | null;
  /** The image a status showed running since its last click, or this page cancelled. */
  seenRunning: string | null;
  hiddenImage: string | null;
  dismissedNotice: string | null;
  dismissedDigest: string | null;
  configUnreadable: boolean;
  configNoticeDismissed: boolean;
}

/** What a banner control does. The hook turns each into its handler, so the decision itself never holds one. */
export type BannerIntent =
  | { do: "update"; image: string; digest: string | null }
  | { do: "cancel"; image: string }
  | { do: "hide"; image: string }
  | { do: "show" }
  | { do: "dismissNotice"; image: string }
  | { do: "dismissRefusal" }
  | { do: "dismissOffer"; digest: string }
  | { do: "dismissConfigNotice" };

type Handler = "onUpdate" | "onDismiss" | "onCancel" | "onRetry" | "onHide" | "onShow";
/** `T` with an intent where it has a handler. */
type WithIntents<T> = {
  [K in keyof T]: K extends Handler ? (undefined extends T[K] ? BannerIntent | undefined : BannerIntent) : T[K];
};
type EachWithIntents<T> = T extends unknown ? WithIntents<T> : never;

export interface DerivedBanner {
  view: EachWithIntents<SandboxImageBannerView> | null;
  running: WithIntents<SandboxPullRunning> | null;
}

/** The banner for a state. Pure; exported for its decision-table test. */
export function deriveBanner(input: BannerInputs): DerivedBanner {
  const { status, attempt, cancel, seenRunning, hiddenImage, dismissedNotice } = input;
  if (input.configUnreadable) {
    return {
      view: input.configNoticeDismissed
        ? null
        : { kind: "config_unreadable", onDismiss: { do: "dismissConfigNotice" } },
      running: null,
    };
  }
  if (!status || !status.checks_enabled || !status.sandbox_in_use) return { view: null, running: null };
  const offered = status.state === "available" ? status.remote_digest : null;
  // A pull is this page's once a status showed it running, or the Daemon accepted this page's update of it.
  const tied = (image: string) => seenRunning === image || (attempt?.accepted === true && attempt.image === image);

  // A running pull outranks a refusal: it can't be the refused attempt's (its status would have confirmed it), and
  // the refusal shows once that pull ends.
  const { pull } = status;
  if (pull.state === "running") {
    const progress = pullProgress(pull);
    const image = status.image;
    const hidden = hiddenImage === image;
    return {
      view: hidden
        ? null
        : {
            kind: "updating",
            progress,
            onCancel: status.pull_blocked ? undefined : { do: "cancel", image },
            cancelReady: tied(image),
            cancelling: cancel?.pending ?? false,
            cancelRefused: cancel?.refused ?? null,
            onHide: { do: "hide", image },
          },
      running: { progress, hidden, onShow: { do: "show" } },
    };
  }

  // The refusal has its own dismissal: it says nothing about an outcome's notice, which stays as the user left it.
  if (attempt?.refusal != null) {
    return {
      view: {
        kind: "failed",
        message: REFUSED,
        error: attempt.refusal,
        onRetry: { do: "update", image: status.image, digest: offered },
        onDismiss: { do: "dismissRefusal" },
      },
      running: null,
    };
  }

  // The Daemon keeps its last pull result, so an outcome shows only for a pull tied to this page or for the
  // live attempt's image; otherwise every reload would repeat an old result.
  const attempted = attempt !== null && !attempt.refused && attempt.image === status.image;
  if ((tied(status.image) || attempted) && dismissedNotice !== status.image) {
    const image = status.image;
    const onDismiss: BannerIntent = { do: "dismissNotice", image };
    const again: BannerIntent | undefined = status.pull_blocked
      ? undefined
      : { do: "update", image: status.image, digest: offered };
    // An image published since this pull is news again; the one it pulled isn't.
    const newer = status.state === "available" && status.remote_digest !== (attempt?.digest ?? null);
    // An outcome nothing tied to this page may be an older pull's: the offer shows rather than it while one is up.
    const unconfirmed = !tied(status.image) && status.state === "available";
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

  if (status.state !== "available" || status.dismissed || input.dismissedDigest === status.remote_digest) {
    return { view: null, running: null };
  }
  const digest = status.remote_digest;
  const onDismiss: BannerIntent = { do: "dismissOffer", digest };
  if (status.pull_blocked) {
    return { view: { kind: "gated", image: status.image, command: status.pull_command, onDismiss }, running: null };
  }
  return {
    view: {
      kind: "available",
      image: status.image,
      onUpdate: { do: "update", image: status.image, digest: offered },
      onDismiss,
    },
    running: null,
  };
}

/** The sandbox image banner and its running pull. Shown only when checks are on and a Sandbox is in use. */
export function useSandboxImageBanner(): SandboxImageBannerState {
  const [status, setStatus] = useState<SandboxImageStatus | null>(null);
  const [dismissedDigest, setDismissedDigest] = useState<string | null>(null);
  // The image whose outcome notice was dismissed; that image's next pull, click or Cancel brings notices back.
  const [dismissedNotice, setDismissedNotice] = useState<string | null>(null);
  // × on a running pull hides its banner until that image's pull ends; its outcome shows again.
  const [hiddenImage, setHiddenImage] = useState<string | null>(null);
  // The attempt and the last Cancel, each held as state for render and mirrored in a ref for the async replies,
  // and written only through `writeAttempt` and `writeCancel` so both always agree.
  const [attempt, setAttempt] = useState<Attempt | null>(null);
  const attemptRef = useRef<Attempt | null>(null);
  const attemptIds = useRef(0);
  const [cancelSent, setCancelSent] = useState<CancelSent | null>(null);
  const cancelRef = useRef<CancelSent | null>(null);
  const cancelIds = useRef(0);
  const writeAttempt = useCallback((next: (a: Attempt | null) => Attempt | null) => {
    attemptRef.current = next(attemptRef.current);
    setAttempt(attemptRef.current);
  }, []);
  const writeCancel = useCallback((next: (c: CancelSent | null) => CancelSent | null) => {
    cancelRef.current = next(cancelRef.current);
    setCancelSent(cancelRef.current);
  }, []);
  // Set from "Update now" until a status answer settles it. A ref, not derived from the attempt, so it keeps its
  // last value through a failed status request and polling stays fast.
  const expectingPull = useRef(false);
  // When a create that pulls was sent; it may reach its pull only after its hooks.
  const createPullSince = useRef<number | null>(null);
  // The image a status since the click showed running, or this page cancelled: from then on, that image's outcome
  // is this page's. A click forgets only its own image's.
  const [seenRunning, setSeenRunning] = useState<string | null>(null);
  // `seenRunning` for the reply, which can't read state.
  const seenRunningRef = useRef<string | null>(null);
  // The image the last status showed running, so a pull of an image starting re-arms that image's notice.
  const polledRunning = useRef<string | null>(null);
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
      const a = attemptRef.current;
      const predatesPull =
        !!answer &&
        a !== null &&
        a.replyOut &&
        Date.now() - a.since < UNCONFIRMED_PULL_MS &&
        a.confirmed !== a.image &&
        answer.image === a.image &&
        answer.pull.state !== "running";
      if (answer) {
        watching = answer.checks_enabled && answer.sandbox_in_use;
        // A Cancel belongs to the pull it was sent for: once a status names another image, or shows that
        // pull ended, its reply can't own the banner, which may already show a new pull of the same image.
        const ended = answer.pull.state !== "running" && !predatesPull;
        const c = cancelRef.current;
        if (c !== null && (c.image !== answer.image || ended)) writeCancel(() => null);
        setStatus(predatesPull ? { ...answer, pull: { state: "running" } } : answer);
        const running = answer.pull.state === "running";
        if (running && polledRunning.current !== answer.image) {
          setDismissedNotice((d) => (d === answer.image ? null : d));
        }
        polledRunning.current = running ? answer.image : null;
        if (running) {
          seenRunningRef.current = answer.image;
          setSeenRunning(answer.image);
          // The Daemon is pulling after all, e.g. the "Update now" reply was lost on the way.
          writeAttempt((at) =>
            at && answer.image === at.image ? { ...at, confirmed: answer.image, refusal: null } : at,
          );
        } else if (!predatesPull) {
          setHiddenImage((h) => (h === answer.image ? null : h));
        }
        // The cap only limits what's shown: while the reply is out, keep watching for the pull.
        expectingPull.current = predatesPull || running || (a?.replyOut ?? false);
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
    // The create's pull supersedes an "Update now" of its image, refused or still awaiting its reply, and any Cancel of
    // that image still out; one whose image the dialog didn't know may be any.
    const offPulled = onSandboxImagePulled((image) => {
      createPullSince.current = null;
      if (image === undefined || image === attemptRef.current?.image) {
        writeAttempt((a) => a && { ...a, id: ++attemptIds.current, replyOut: false, refusal: null });
      }
      writeCancel((c) => (c && (image === undefined || c.image === image) ? null : c));
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
  }, [writeAttempt, writeCancel]);

  // `image` and `digest` are the banner's that was clicked, not refs a poll may have moved since.
  const update = useCallback(
    (image: string, digest: string | null) => {
      const mine = ++attemptIds.current;
      writeAttempt(() => ({
        id: mine,
        image,
        digest,
        since: Date.now(),
        replyOut: true,
        confirmed: null,
        accepted: false,
        refused: false,
        refusal: null,
      }));
      writeCancel(() => null);
      // A pull of another image this page saw running keeps its outcome.
      if (seenRunningRef.current === image) seenRunningRef.current = null;
      setSeenRunning((seen) => (seen === image ? null : seen));
      setDismissedNotice((d) => (d === image ? null : d));
      setHiddenImage((h) => (h === image ? null : h));
      expectingPull.current = true;
      setStatus((s) => s && (s.image === image ? { ...s, pull: { state: "running" } } : s));
      void pullSandboxImage().then((reply) => {
        const a = attemptRef.current;
        if (a === null || a.id !== mine) return;
        if (reply.ok) {
          const pulled = reply.image ?? image;
          // A poll since the click already showed this image's pull, so it says what that pull is doing.
          const confirmed = seenRunningRef.current === pulled ? pulled : a.confirmed;
          writeAttempt(() => ({ ...a, image: pulled, accepted: true, replyOut: false, confirmed }));
          // Acceptance ties what comes next to this click, not a state already on screen: until a
          // status fetched after it lands, show the pull as running, if it is that image's.
          if (confirmed !== pulled) {
            setStatus((s) => s && (s.image === pulled ? { ...s, pull: { state: "running" } } : s));
          }
        } else if (a.confirmed !== image) {
          // Nothing was pulled, and the Daemon may still hold an older pull's outcome. No status has
          // shown this image's pull since the click, so a "running" for it in `status` is this page's guess.
          expectingPull.current = false;
          writeAttempt(() => ({
            ...a,
            replyOut: false,
            refused: true,
            refusal: reply.message ?? "Couldn't reach the Daemon.",
          }));
          setStatus(
            (s) => s && (s.image === image && s.pull.state === "running" ? { ...s, pull: { state: "idle" } } : s),
          );
        } else {
          writeAttempt(() => ({ ...a, replyOut: false }));
        }
        pollNow.current();
      });
      // Poll fast from the click, not from the reply: the Daemon starts the pull before it answers,
      // and the reply has no deadline.
      pollNow.current();
    },
    [writeAttempt, writeCancel],
  );

  // `image` is the one on the banner, so a default changed since can't cancel a different pull.
  const cancel = useCallback(
    (image: string) => {
      const id = ++cancelIds.current;
      writeCancel(() => ({ id, image, pending: true, refused: null }));
      void cancelSandboxImagePull(image).then((reply) => {
        // A click, a create's pull, or a status for another image or the pull's end retired this Cancel.
        if (cancelRef.current?.id !== id) return;
        writeCancel(
          (c) =>
            c && {
              ...c,
              pending: false,
              refused: reply.ok ? null : (reply.message ?? "Couldn't reach the Daemon."),
            },
        );
        if (reply.pull) {
          const pull = reply.pull;
          if (pull.state !== "running") {
            // This attempt's pull is over, so its reply still on the way no longer decides what shows.
            writeAttempt((a) => (a && image === a.image ? { ...a, id: ++attemptIds.current, replyOut: false } : a));
          }
          // This page cancelled it, so the outcome is this page's to show.
          seenRunningRef.current = image;
          setSeenRunning(image);
          setDismissedNotice((d) => (d === image ? null : d));
          setHiddenImage((h) => (h === image ? null : h));
          setStatus((s) => s && { ...s, pull });
        }
        pollNow.current();
      });
    },
    [writeAttempt, writeCancel],
  );

  const run = useCallback(
    (intent: BannerIntent) => {
      switch (intent.do) {
        case "update":
          return update(intent.image, intent.digest);
        case "cancel":
          return cancel(intent.image);
        case "hide":
          setHiddenImage(intent.image);
          return writeCancel((c) => c && { ...c, refused: null });
        case "show":
          return setHiddenImage(null);
        case "dismissNotice":
          return setDismissedNotice(intent.image);
        case "dismissRefusal":
          return writeAttempt((a) => a && { ...a, refusal: null });
        case "dismissOffer":
          // Hidden for this page whether or not the Daemon saves it, as for the AoE update banner: read-only
          // and CityHall servers refuse to save a dismissal, so rolling back would make Dismiss do nothing.
          setDismissedDigest(intent.digest);
          return void dismissSandboxImage(intent.digest);
        case "dismissConfigNotice":
          return setConfigNoticeDismissed(true);
      }
    },
    [update, cancel, writeAttempt, writeCancel],
  );
  const { view, running } = deriveBanner({
    status,
    attempt,
    cancel: cancelSent,
    seenRunning,
    hiddenImage,
    dismissedNotice,
    dismissedDigest,
    configUnreadable,
    configNoticeDismissed,
  });
  // Each intent becomes its handler here, in the hook, so the decision above stays pure.
  const handlers: Partial<Record<Handler, () => void>> = {};
  for (const [key, value] of Object.entries(view ?? {})) {
    if (isIntent(value)) handlers[key as Handler] = () => run(value);
  }
  return {
    view: view && ({ ...view, ...handlers } as SandboxImageBannerView),
    running: running && { progress: running.progress, hidden: running.hidden, onShow: () => run(running.onShow) },
  };
}

function isIntent(value: unknown): value is BannerIntent {
  return typeof value === "object" && value !== null && "do" in value;
}
