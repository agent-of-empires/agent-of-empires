import { useEffect, useRef, useState } from "react";
import { fetchSandboxImageStatus, type SandboxImageStatus } from "../../lib/api";

// Long enough that typing an image name asks once, after the last keystroke.
export const IMAGE_RECHECK_MS = 500;
// While the image downloads, so the dialog follows it and offers its checkbox again once it ends.
export const RUNNING_RECHECK_MS = 2000;
// Otherwise, so a pull that starts while the dialog is open shows.
export const IDLE_RECHECK_MS = 10_000;

/** What the Sandbox row says before the image, from a status, a refusal (`{ error }`) or null offline. */
export function sandboxImageStatusLabel(reply: SandboxImageReply): string {
  if (!reply || !("state" in reply)) return "Update status unknown";
  switch (reply.state) {
    case "available":
      return "Update available";
    case "current":
      return "Up to date";
    case "check_failed":
      return "Couldn't check for updates";
    case "unknown":
      return reply.reason === "not_pulled" ? "Downloads on launch" : "Update status unknown";
  }
}

export type SandboxImageReply = SandboxImageStatus | { error: string } | null;

/** The status label and reply for `image` (empty: the default image), or null until it is answered.
 *  Only the first check runs at once; an edited image is re-checked once typing stops, even while that
 *  first check is pending, every 2 s while it downloads, through failed checks, and every 10 s
 *  otherwise. The first answered check skips the Daemon's cache. */
export function useSandboxImageRowStatus(
  image: string,
  enabled: boolean,
): { label: string; reply: SandboxImageReply } | null {
  const [answer, setAnswer] = useState<{ image: string; label: string; reply: SandboxImageReply } | null>(null);
  const started = useRef(false);
  const answered = useRef(false);
  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | null = null;
    let pulling = false;
    const check = async () => {
      const reply = await fetchSandboxImageStatus(image || undefined, { fresh: !answered.current });
      if (cancelled) return;
      answered.current = true;
      const conclusive = !!reply && "state" in reply;
      // A failed check says nothing about a running pull: keep showing it, and ask again.
      if (!conclusive && pulling) {
        timer = setTimeout(check, RUNNING_RECHECK_MS);
        return;
      }
      pulling = conclusive && reply.pull.state === "running";
      setAnswer({ image, label: sandboxImageStatusLabel(reply), reply });
      timer = setTimeout(check, pulling ? RUNNING_RECHECK_MS : IDLE_RECHECK_MS);
    };
    if (started.current) timer = setTimeout(check, IMAGE_RECHECK_MS);
    else void check();
    started.current = true;
    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
    };
  }, [image, enabled]);
  return enabled && answer?.image === image ? { label: answer.label, reply: answer.reply } : null;
}
