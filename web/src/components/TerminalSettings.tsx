import { useWebSettings } from "../hooks/useWebSettings";
import {
  MAX_PERSISTENT_TERMINALS,
  MIN_PERSISTENT_TERMINALS,
  normalizePersistentTerminalLimit,
} from "../lib/persistentTerminals";
import { FontSizeControl } from "./settings/FontSizeControl";
import { MobileKeysSettings } from "./settings/MobileKeysSettings";
import { TerminalFontPicker } from "./settings/TerminalFontPicker";
import { SETTINGS_CAPTION, SETTINGS_CARD } from "./settings/FormFields";

export function TerminalSettings() {
  const { settings, update } = useWebSettings();
  const maxPersistentTerminals = normalizePersistentTerminalLimit(settings.maxPersistentTerminals);

  return (
    <div>
      <h3 className={SETTINGS_CAPTION}>This browser</h3>

      <div className={`${SETTINGS_CARD} py-4 space-y-4`}>
        <FontSizeControl
          label="Mobile font size"
          testIdPrefix="terminal-mobile-font-size"
          value={settings.mobileFontSize}
          onChange={(value) => update({ mobileFontSize: value })}
          description="Font size for web terminal sessions on mobile devices, including tmux-backed sessions. Pinch the terminal with two fingers to zoom; the new size is saved here."
        />

        <FontSizeControl
          label="Desktop font size"
          testIdPrefix="terminal-desktop-font-size"
          value={settings.desktopFontSize}
          onChange={(value) => update({ desktopFontSize: value })}
          description="Font size for web terminal sessions on desktop, including tmux-backed sessions. Hold Ctrl and scroll over the terminal (or pinch on a trackpad) to zoom; the new size is saved here."
        />

        <TerminalFontPicker
          value={settings.terminalFontFamily}
          onChange={(terminalFontFamily) => update({ terminalFontFamily })}
        />

        <div>
          <label className="flex items-center justify-between gap-3 cursor-pointer">
            <div>
              <div className="text-[13px] text-text-secondary">Auto-open keyboard on mobile</div>
              <p className="text-[11px] text-text-muted mt-1">
                Open the soft keyboard when you select a session. Turn off for monitoring-first workflows.
              </p>
            </div>
            <input
              type="checkbox"
              checked={settings.autoOpenKeyboard}
              onChange={(e) => update({ autoOpenKeyboard: e.target.checked })}
              className="accent-brand-600 w-4 h-4 shrink-0"
            />
          </label>
        </div>

        <div>
          <div className="space-y-3">
            <label className="flex items-center justify-between gap-3 cursor-pointer">
              <div>
                <div className="text-[13px] text-text-secondary">
                  Keep terminals alive <span className="font-mono text-[11px] text-text-muted">(Beta)</span>
                </div>
                <p className="text-[11px] text-text-muted mt-1">
                  Keep recently viewed web terminals mounted for faster switching across many sessions. Uses more
                  browser memory and keeps extra terminal connections open.
                </p>
              </div>
              <input
                type="checkbox"
                checked={settings.persistentTerminals}
                onChange={(e) => update({ persistentTerminals: e.target.checked })}
                className="accent-brand-600 w-4 h-4 shrink-0"
              />
            </label>

            {settings.persistentTerminals && (
              <div>
                <label className="block text-[13px] text-text-secondary mb-2">Terminal keep-alive limit</label>
                <div className="flex items-center gap-3">
                  <input
                    type="range"
                    min={MIN_PERSISTENT_TERMINALS}
                    max={MAX_PERSISTENT_TERMINALS}
                    step={1}
                    value={maxPersistentTerminals}
                    onChange={(e) =>
                      update({
                        maxPersistentTerminals: normalizePersistentTerminalLimit(Number(e.target.value)),
                      })
                    }
                    className="flex-1 accent-brand-600 h-1.5"
                  />
                  <input
                    type="number"
                    min={MIN_PERSISTENT_TERMINALS}
                    max={MAX_PERSISTENT_TERMINALS}
                    step={1}
                    value={maxPersistentTerminals}
                    onChange={(e) =>
                      update({
                        maxPersistentTerminals: normalizePersistentTerminalLimit(Number(e.target.value)),
                      })
                    }
                    className="bg-surface-800 border border-surface-700 rounded-md px-2 py-1 text-sm text-text-primary font-mono w-16 text-center"
                  />
                </div>
                <p className="text-[11px] text-text-muted mt-1">
                  Higher limits improve switching across large workspaces but keep more total terminal renderers,
                  sockets, and tmux attachments alive.
                </p>
              </div>
            )}
          </div>
        </div>

        <label className="flex items-center justify-between gap-3 cursor-pointer">
          <div>
            <div className="text-[13px] text-text-secondary">Scrollbar for full-screen apps</div>
            <p className="text-[11px] text-text-muted mt-1">
              Show scroll arrows and a drag handle beside agents that use the whole screen and take mouse input. Uses a
              few terminal columns; hidden on touch devices.
            </p>
          </div>
          <input
            type="checkbox"
            checked={settings.showForwardScrollbar}
            onChange={(e) => update({ showForwardScrollbar: e.target.checked })}
            className="accent-brand-600 w-4 h-4 shrink-0"
          />
        </label>

        <MobileKeysSettings />
      </div>
    </div>
  );
}
