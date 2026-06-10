import { useIsCoarsePointer } from "../hooks/useIsCoarsePointer";
import { LiveTerminalView } from "./LiveTerminalView";
import { XtermTerminalView } from "./XtermTerminalView";
import type { SessionResponse } from "../lib/types";

interface Props {
  session: SessionResponse;
  active?: boolean;
}

/** Agent terminal dispatcher. Touch-primary devices get the
 *  capture-snapshot live view (the TUI's live-mode architecture: native
 *  scrolling, send-keys input, no PTY attach); fine-pointer devices get
 *  the xterm.js PTY relay. Each branch owns all of its hooks, so the
 *  pointer-type flip (rare, e.g. plugging a mouse into a tablet) simply
 *  swaps subtrees. */
export function TerminalView({ session, active = true }: Props) {
  const coarse = useIsCoarsePointer();
  return coarse ? (
    <LiveTerminalView session={session} active={active} />
  ) : (
    <XtermTerminalView session={session} active={active} />
  );
}
