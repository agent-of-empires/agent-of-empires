import { LiveTerminalView } from "./LiveTerminalView";
import type { SessionResponse } from "../lib/types";

interface Props {
  readOnly: boolean;
  session: SessionResponse;
  active?: boolean;
}

/** Agent terminal: the capture-snapshot live view (the TUI's live-mode architecture, native scroll, send-keys
 *  input, no PTY attach), on every device. */
export function TerminalView({ session, readOnly, active = true }: Props) {
  return <LiveTerminalView session={session} readOnly={readOnly} active={active} />;
}
