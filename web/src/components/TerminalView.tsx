import { LiveTerminalView } from "./LiveTerminalView";
import type { SessionResponse } from "../lib/types";

interface Props {
  readOnly: boolean;
  session: SessionResponse;
  active?: boolean;
  recoveryCurrent?: boolean;
}

/** Agent terminal: the capture-snapshot live view (the TUI's live-mode architecture, native scroll, send-keys
 *  input, no PTY attach), on every device. */
export function TerminalView({ session, readOnly, active = true, recoveryCurrent = false }: Props) {
  return <LiveTerminalView session={session} readOnly={readOnly} active={active} recoveryCurrent={recoveryCurrent} />;
}
