import { LiveTerminalView } from "./LiveTerminalView";
import type { SessionResponse } from "../lib/types";

interface Props {
  session: SessionResponse;
  active?: boolean;
  /** Mobile sidebar-toggle FAB wiring; forwarded to the live view so a
   *  deep-in-a-session thumb can open the session list without reaching the
   *  top bar (#2245). Only the primary agent surface receives these. */
  sidebarOpen?: boolean;
  onToggleSidebar?: () => void;
}

/** Agent terminal: the capture-snapshot live view (the TUI's live-mode
 *  architecture, native scroll, send-keys input, no PTY attach), on every
 *  device. The xterm.js PTY relay was removed in favor of this single
 *  renderer so desktop, mobile, and the TUI all show the pane the same way. */
export function TerminalView({ session, active = true, sidebarOpen, onToggleSidebar }: Props) {
  return (
    <LiveTerminalView session={session} active={active} sidebarOpen={sidebarOpen} onToggleSidebar={onToggleSidebar} />
  );
}
