import { sessionFlagGate } from "./sessionFlagGate";

/** The unread indicator is on by default, matching the server's
 *  `session.unread_indicator` default. */
const gate = sessionFlagGate("unread_indicator", true);

export const UnreadIndicatorContext = gate.Context;
export const parseUnreadIndicatorEnabled = gate.parse;
export const useUnreadIndicatorEnabled = gate.use;
