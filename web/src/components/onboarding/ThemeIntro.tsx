import { useCallback, useEffect, useRef, useState } from "react";
import { fetchThemes } from "../../lib/api";
import { useThemeMutation } from "../../hooks/useThemeMutation";

interface Props {
  /** Dismiss the welcome modal and hand off to the tour. Called for both the
   *  Continue button and Escape; the seen flag is owned by the caller. */
  onDone: () => void;
}

/**
 * First-run "Choose your theme" modal, phase one of onboarding. Selecting a
 * theme persists it to the default profile and repaints the whole dashboard
 * live (persist-then-paint via useThemeMutation), so the grid doubles as the
 * preview; the user can re-pick freely before continuing. Shown on any pointer
 * type, unlike the desktop-only tour. Dismissing hands off to the tour.
 */
export function ThemeIntro({ onDone }: Props) {
  const [themes, setThemes] = useState<string[]>([]);
  const [selected, setSelected] = useState<string | null>(
    () =>
      (typeof document !== "undefined"
        ? document.documentElement.dataset.theme
        : undefined) ?? null,
  );
  const [error, setError] = useState<string | null>(null);
  const { select, pending } = useThemeMutation();
  const continueRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    fetchThemes().then(setThemes);
    continueRef.current?.focus();
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onDone();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onDone]);

  const pick = useCallback(
    async (name: string) => {
      if (pending || name === selected) return;
      const prev = selected;
      setSelected(name);
      setError(null);
      const result = await select(name);
      // Persist-then-paint already repainted on success; on failure restore the
      // prior highlight so the grid never claims an unsaved theme is active.
      if (!result.ok) {
        setSelected(prev);
        setError(result.error);
      }
    },
    [pending, selected, select],
  );

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="theme-intro-title"
      className="fixed inset-0 bg-black/60 flex items-center justify-center z-50 animate-fade-in p-4"
    >
      <div className="bg-surface-800 border border-surface-700/50 rounded-lg w-[460px] max-w-[92vw] shadow-2xl animate-slide-up">
        <div className="px-5 py-4 border-b border-surface-700">
          <h2
            id="theme-intro-title"
            className="text-sm font-semibold text-text-bright"
          >
            Welcome! Choose your theme
          </h2>
          <p className="mt-1 text-xs text-text-dim">
            Pick a look for the dashboard and TUI. You can change it any time
            from Settings, under Appearance.
          </p>
        </div>

        <div className="p-5 space-y-4">
          <div
            role="listbox"
            aria-label="Themes"
            className="grid grid-cols-2 gap-2 max-h-64 overflow-y-auto"
          >
            {themes.map((t) => {
              const active = t === selected;
              return (
                <button
                  key={t}
                  type="button"
                  role="option"
                  aria-selected={active}
                  disabled={pending}
                  onClick={() => pick(t)}
                  className={`text-left text-sm rounded-md border px-3 py-2 cursor-pointer transition-colors disabled:opacity-60 disabled:cursor-not-allowed ${
                    active
                      ? "border-brand-500 bg-surface-700 text-text-bright"
                      : "border-surface-700 text-text-secondary hover:border-brand-600 hover:text-text-primary"
                  }`}
                >
                  {t}
                </button>
              );
            })}
          </div>
          {error && (
            <p role="alert" className="text-xs text-status-error">
              {error}
            </p>
          )}
        </div>

        <div className="flex justify-end px-5 py-4 border-t border-surface-700">
          <button
            ref={continueRef}
            type="button"
            onClick={onDone}
            className="text-sm font-medium rounded-md bg-brand-600 hover:bg-brand-500 text-surface-950 px-4 py-1.5 cursor-pointer transition-colors"
          >
            Continue
          </button>
        </div>
      </div>
    </div>
  );
}
