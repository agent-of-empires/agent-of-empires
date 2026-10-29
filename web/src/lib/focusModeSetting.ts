import { sessionFlagGate } from "./sessionFlagGate";

/** Focus mode is offered by default, matching the server's `session.focus_mode` default. */
const gate = sessionFlagGate("focus_mode", true);

export const FocusModeEnabledContext = gate.Context;
export const parseFocusModeEnabled = gate.parse;
export const useFocusModeEnabled = gate.use;
