import { useMemo } from "react";

import { detectInstalledFonts } from "../../lib/fontDetect";
import {
  BUNDLED_TERMINAL_FONTS,
  DEFAULT_TERMINAL_FONT,
  DEFAULT_TERMINAL_FONT_NAME,
  SYSTEM_TERMINAL_FONTS,
  terminalFontChoices,
  terminalFontStack,
} from "../../lib/terminalFonts";

interface TerminalFontPickerProps {
  value: string;
  onChange: (value: string) => void;
}

export function TerminalFontPicker({ value, onChange }: TerminalFontPickerProps) {
  // Installed fonts don't change while the panel is open.
  const detected = useMemo(() => detectInstalledFonts(SYSTEM_TERMINAL_FONTS), []);
  const { installed, missing } = terminalFontChoices(value, detected);
  // Desktop Chromium and Firefox draw each option in its font; macOS and mobile native menus ignore it.
  const fontStyle = (family: string) => ({ fontFamily: terminalFontStack(family) ?? "var(--font-mono)" });

  return (
    <div>
      <label htmlFor="terminal-font-family" className="block text-[13px] text-text-secondary mb-2">
        Font family
      </label>
      <select
        id="terminal-font-family"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        style={fontStyle(missing ? DEFAULT_TERMINAL_FONT : value)}
        className="w-full bg-surface-800 border border-surface-700 rounded-md px-2 py-1 text-sm text-text-primary"
      >
        <optgroup label="Bundled">
          <option value={DEFAULT_TERMINAL_FONT} style={fontStyle(DEFAULT_TERMINAL_FONT)}>
            {DEFAULT_TERMINAL_FONT_NAME} (default)
          </option>
          {BUNDLED_TERMINAL_FONTS.map((name) => (
            <option key={name} value={name} style={fontStyle(name)}>
              {name}
            </option>
          ))}
        </optgroup>
        {installed.length > 0 && (
          <optgroup label="Installed on this device">
            {installed.map((name) => (
              <option key={name} value={name} style={fontStyle(name)}>
                {name}
              </option>
            ))}
          </optgroup>
        )}
        {missing && (
          <optgroup label="Not on this device">
            <option value={missing}>{missing}</option>
          </optgroup>
        )}
      </select>
      {missing && (
        <p className="text-[11px] text-status-warning mt-1" data-testid="terminal-font-missing">
          {missing} isn't installed on this device, so terminals here use {DEFAULT_TERMINAL_FONT_NAME}.
        </p>
      )}
      <p className="text-[11px] text-text-muted mt-1">
        Font for web terminal sessions. Bundled fonts work everywhere; installed fonts are listed when this device has
        them, and the choice follows you to other devices. Pick a Nerd Font to render powerline and icon glyphs.
      </p>
    </div>
  );
}
