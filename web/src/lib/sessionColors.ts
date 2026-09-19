import { sessionFlagGate } from "./sessionFlagGate";

/** Session colors are on by default, matching the server's
 *  `session.show_session_colors` default. */
const gate = sessionFlagGate("show_session_colors", true);

export const SessionColorsContext = gate.Context;
export const parseSessionColorsEnabled = gate.parse;
export const useSessionColorsEnabled = gate.use;
