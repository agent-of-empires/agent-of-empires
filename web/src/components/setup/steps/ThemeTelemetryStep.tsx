import { useEffect, useState } from "react";
import { fetchThemes } from "../../../lib/api";
import { useThemeMutation } from "../../../hooks/useThemeMutation";
import { PANEL, RadioRow, Section } from "../ui";

interface Props {
  share: boolean;
  onShareChange: (share: boolean) => void;
  doNotTrack: boolean;
}

/** Theme saves on pick; the telemetry choice saves when the wizard moves on. */
export function ThemeTelemetryStep({ share, onShareChange, doNotTrack }: Props) {
  const [themes, setThemes] = useState<string[]>([]);
  const [selected, setSelected] = useState(() => document.documentElement.dataset.theme ?? "");
  const [error, setError] = useState<string | null>(null);
  const { select } = useThemeMutation();

  useEffect(() => {
    void fetchThemes().then(setThemes);
  }, []);

  const pick = async (name: string) => {
    const previous = selected;
    setSelected(name);
    setError(null);
    const result = await select(name);
    if (!result.ok) {
      setSelected(previous);
      setError(result.error);
    }
  };

  return (
    <>
      <Section title="Theme" hint="Applies here and in the TUI. Picking one repaints this page.">
        <div className={`${PANEL} max-h-80 overflow-y-auto`}>
          {themes.map((name) => (
            <RadioRow key={name} name="theme" checked={selected === name} onSelect={() => void pick(name)}>
              <span className="font-mono text-sm text-text-primary">{name}</span>
            </RadioRow>
          ))}
        </div>
        {error && <p className="text-xs text-status-error">{error}</p>}
      </Section>
      <Section title="Help improve AoE?" hint="Off unless you turn it on. Change it any time in Settings › Telemetry.">
        {doNotTrack ? (
          <p className="text-sm text-text-secondary">
            <span className="font-mono">DO_NOT_TRACK</span> is set, so nothing is sent.
          </p>
        ) : (
          <>
            <div className={PANEL}>
              <RadioRow name="telemetry" checked={!share} onSelect={() => onShareChange(false)}>
                <div className="text-sm text-text-primary">Not now</div>
                <div className="text-xs text-text-dim">Nothing is sent.</div>
              </RadioRow>
              <RadioRow name="telemetry" checked={share} onSelect={() => onShareChange(true)}>
                <div className="text-sm text-text-primary">Share anonymous usage counts</div>
                <div className="text-xs text-text-dim">
                  Number of Sessions, which Agents and model families, AoE version and OS.
                </div>
              </RadioRow>
            </div>
            <p className="text-xs text-text-dim">
              Never sent: prompts, file paths, names, branch names or commands.{" "}
              <span className="font-mono">DO_NOT_TRACK</span> always wins.
            </p>
          </>
        )}
      </Section>
    </>
  );
}
