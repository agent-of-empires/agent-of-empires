import { useMemo } from "react";

import { useWebSettings } from "../hooks/useWebSettings";
import { detectInstalledFonts } from "../lib/fontDetect";
import {
  MAX_PERSISTENT_TERMINALS,
  MIN_PERSISTENT_TERMINALS,
  normalizePersistentTerminalLimit,
} from "../lib/persistentTerminals";

const FONT_SIZES = Array.from({ length: 23 }, (_, i) => i + 6); // 6..28

export function TerminalSettings() {
  const { settings, update } = useWebSettings();
  const maxPersistentTerminals = normalizePersistentTerminalLimit(settings.maxPersistentTerminals);
  // Probe once per mount; the set of installed fonts doesn't change while the
  // panel is open.
  const detectedFonts = useMemo(() => detectInstalledFonts(), []);

  return (
    <div>
      <h3 className="font-mono text-sm uppercase tracking-widest text-text-muted mb-4">Terminal</h3>

      <div className="space-y-4">
        <div>
          <label className="block text-[13px] text-text-secondary mb-2">Mobile font size</label>
          <div className="flex items-center gap-3">
            <input
              type="range"
              min={6}
              max={28}
              step={1}
              value={settings.mobileFontSize}
              onChange={(e) => update({ mobileFontSize: Number(e.target.value) })}
              className="flex-1 accent-brand-600 h-1.5"
            />
            <select
              value={settings.mobileFontSize}
              onChange={(e) => update({ mobileFontSize: Number(e.target.value) })}
              className="bg-surface-800 border border-surface-700 rounded-md px-2 py-1 text-sm text-text-primary font-mono w-16 text-center"
            >
              {FONT_SIZES.map((s) => (
                <option key={s} value={s}>
                  {s}px
                </option>
              ))}
            </select>
          </div>
          <p className="text-[11px] text-text-muted mt-1">
            Font size for web terminal sessions on mobile devices, including tmux-backed sessions. Pinch the terminal
            with two fingers to zoom; the new size is saved here.
          </p>
        </div>

        <div>
          <label className="block text-[13px] text-text-secondary mb-2">Desktop font size</label>
          <div className="flex items-center gap-3">
            <input
              type="range"
              min={6}
              max={28}
              step={1}
              value={settings.desktopFontSize}
              onChange={(e) => update({ desktopFontSize: Number(e.target.value) })}
              className="flex-1 accent-brand-600 h-1.5"
            />
            <select
              value={settings.desktopFontSize}
              onChange={(e) => update({ desktopFontSize: Number(e.target.value) })}
              className="bg-surface-800 border border-surface-700 rounded-md px-2 py-1 text-sm text-text-primary font-mono w-16 text-center"
            >
              {FONT_SIZES.map((s) => (
                <option key={s} value={s}>
                  {s}px
                </option>
              ))}
            </select>
          </div>
          <p className="text-[11px] text-text-muted mt-1">
            Font size for web terminal sessions on desktop, including tmux-backed sessions. Hold Ctrl and scroll over
            the terminal (or pinch on a trackpad) to zoom; the new size is saved here.
          </p>
        </div>

        <div>
          <label htmlFor="terminal-font-family" className="block text-[13px] text-text-secondary mb-2">
            Font family
          </label>
          <input
            id="terminal-font-family"
            type="text"
            list="terminal-font-options"
            value={settings.terminalFontFamily}
            placeholder="Default (Geist Mono)"
            onChange={(e) => update({ terminalFontFamily: e.target.value })}
            className="w-full bg-surface-800 border border-surface-700 rounded-md px-2 py-1 text-sm text-text-primary font-mono"
          />
          <datalist id="terminal-font-options">
            {detectedFonts.map((f) => (
              <option key={f} value={f} />
            ))}
          </datalist>
          <p className="text-[11px] text-text-muted mt-1">
            Font for web terminal sessions. Pick a detected font or type any font name; it must be installed on this
            device. Leave blank for the bundled default. Use a Nerd Font to render powerline and icon glyphs.
          </p>
        </div>

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
      </div>
    </div>
  );
}
