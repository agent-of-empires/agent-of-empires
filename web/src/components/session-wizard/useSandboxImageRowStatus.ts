import { useEffect, useRef, useState } from "react";
import { fetchSandboxImageStatus, type SandboxImageStatus } from "../../lib/api";

// Long enough that typing an image name asks once, after the last keystroke.
export const IMAGE_RECHECK_MS = 500;

/** What the Sandbox row says before the image, from a status, a refusal (`{ error }`) or null offline. */
export function sandboxImageStatusLabel(reply: SandboxImageStatus | { error: string } | null): string {
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

/** The status label for `image` (empty: the default image), or null until it is answered. Only the
 *  first check runs at once; an edited image is re-checked once typing stops, even while that first
 *  check is pending. The first answered check skips the Daemon's cache. */
export function useSandboxImageRowStatus(image: string, enabled: boolean): string | null {
  const [answer, setAnswer] = useState<{ image: string; label: string } | null>(null);
  const started = useRef(false);
  const answered = useRef(false);
  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    const check = async () => {
      const reply = await fetchSandboxImageStatus(image || undefined, { fresh: !answered.current });
      if (cancelled) return;
      answered.current = true;
      setAnswer({ image, label: sandboxImageStatusLabel(reply) });
    };
    const timer = started.current ? setTimeout(check, IMAGE_RECHECK_MS) : null;
    started.current = true;
    if (!timer) void check();
    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
    };
  }, [image, enabled]);
  return enabled && answer?.image === image ? answer.label : null;
}
