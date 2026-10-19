import { useCallback, useEffect } from "react";

import { updateMobileTerminalFontSize, type SettingsResponse } from "../lib/api";
import { MIN_FONT_SIZE, MAX_FONT_SIZE } from "../lib/fontSizeRange";
import { onSettingsChanged } from "../lib/settingsEvents";
import { loadSettings, saveField, useProfileSettings } from "../lib/settingsStore";
import { reportError } from "../lib/toastBus";

export function useMobileTerminalFont() {
  const { settings, error } = useProfileSettings(undefined);
  useEffect(() => {
    void loadSettings(undefined);
    return onSettingsChanged(() => void loadSettings(undefined));
  }, []);
  useEffect(() => {
    if (error) reportError(error);
  }, [error]);

  const configured = (settings as SettingsResponse | null)?.web?.mobile_terminal_font_size;
  const fontSize =
    typeof configured === "number" &&
    Number.isInteger(configured) &&
    configured >= MIN_FONT_SIZE &&
    configured <= MAX_FONT_SIZE
      ? configured
      : 14;
  const update = useCallback(
    (value: number) =>
      saveField(undefined, "web", "mobile_terminal_font_size", value, () => updateMobileTerminalFontSize(value)),
    [],
  );
  return { fontSize, update };
}
