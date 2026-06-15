import { useEffect, useState } from "react";
import { useIsCoarsePointer } from "../hooks/useIsCoarsePointer";
import { LiveTerminalView } from "./LiveTerminalView";
import { XtermTerminalView } from "./XtermTerminalView";
import type { SessionResponse } from "../lib/types";

interface Props {
  session: SessionResponse;
  active?: boolean;
}

const FORCE_LIVE_KEY = "aoe.forceLiveTerminal";

// Capture `?live=1` / `?live=0` at module load, BEFORE the SPA router runs and
// strips query params, persisting the choice to localStorage. Prototype flag:
// force the capture-snapshot live renderer on every device, not just touch
// ones, so the desktop agent view can be A/B'd against the xterm.js PTY relay
// (the open question is keyboard echo latency). Not a shipped setting yet, just
// a switch for evaluating the unify-on-live-mode direction.
if (typeof window !== "undefined") {
  const param = new URLSearchParams(window.location.search).get("live");
  if (param === "1") localStorage.setItem(FORCE_LIVE_KEY, "1");
  else if (param === "0") localStorage.removeItem(FORCE_LIVE_KEY);
}

function useForceLiveTerminal(): boolean {
  const [forced, setForced] = useState(
    () => typeof window !== "undefined" && localStorage.getItem(FORCE_LIVE_KEY) === "1",
  );
  useEffect(() => {
    const sync = () => setForced(localStorage.getItem(FORCE_LIVE_KEY) === "1");
    window.addEventListener("storage", sync);
    return () => window.removeEventListener("storage", sync);
  }, []);
  return forced;
}

/** Agent terminal dispatcher. Touch-primary devices get the
 *  capture-snapshot live view (the TUI's live-mode architecture: native
 *  scrolling, send-keys input, no PTY attach); fine-pointer devices get
 *  the xterm.js PTY relay. Each branch owns all of its hooks, so the
 *  pointer-type flip (rare, e.g. plugging a mouse into a tablet) simply
 *  swaps subtrees. The `?live=1` flag forces the live view everywhere for
 *  evaluating a single unified renderer (see useForceLiveTerminal). */
export function TerminalView({ session, active = true }: Props) {
  const coarse = useIsCoarsePointer();
  const forceLive = useForceLiveTerminal();
  return coarse || forceLive ? (
    <LiveTerminalView session={session} active={active} />
  ) : (
    <XtermTerminalView session={session} active={active} />
  );
}
