import { LiveTerminalView } from "./LiveTerminalView";
import type { SessionResponse } from "../lib/types";

interface Props {
  session: SessionResponse;
  active?: boolean;
}

/** Agent terminal: the capture-snapshot live view (the TUI's live-mode
 *  architecture, native scroll, send-keys input, no PTY attach), on every
 *  device. The xterm.js PTY relay was removed in favor of this single
 *  renderer so desktop, mobile, and the TUI all show the pane the same way. */
export function TerminalView({ session, active = true }: Props) {
  return <LiveTerminalView session={session} active={active} />;
}
