import { useCallback, useEffect, useRef, useState } from "react";
import { fetchThemes } from "../../lib/api";
import { useThemeMutation } from "../../hooks/useThemeMutation";

const PANEL = "hidden lg:flex lg:w-64 lg:flex-col gap-2 bg-surface-900/40 p-4";
const PANEL_LABEL = "text-[10px] font-medium uppercase tracking-wide text-text-muted";

/** Static acp-style preview: a couple of turns plus the composer. Built
 *  from the same theme tokens the real surfaces use, so it repaints with the
 *  selected theme and shows the user what a session looks like. Decorative and
 *  aria-hidden; the live theme grid is the only interactive control. */
function ComposerPreview() {
  return (
    <div className={`${PANEL} border-r border-surface-700`} aria-hidden="true">
      <span className={PANEL_LABEL}>Composer</span>
      <p className="text-[11px] leading-relaxed text-text-primary">Sure, I'll harden the token expiry check.</p>
      <div className="flex items-center gap-2 rounded-md border border-surface-700 bg-surface-800/50 px-2 py-1">
        <span className="h-1.5 w-1.5 rounded-full bg-status-running" />
        <span className="font-mono text-[10px] text-text-secondary">Edit src/auth.ts</span>
      </div>
      <span className="self-end rounded-2xl rounded-br-sm border border-surface-700 bg-surface-800/70 px-3 py-1.5 text-[11px] text-text-primary">
        Ship it
      </span>
      <div className="mt-auto flex items-center justify-between rounded-xl border border-surface-700 bg-surface-850 px-3 py-2">
        <span className="text-[11px] text-text-dim">Message the agent...</span>
        <span className="rounded bg-brand-600 px-2 py-0.5 text-[10px] text-white">Send</span>
      </div>
    </div>
  );
}

function DiffRow({ num, text, kind }: { num: string; text: string; kind?: "add" | "del" }) {
  const body =
    kind === "add"
      ? "bg-status-running/5 text-status-running"
      : kind === "del"
        ? "bg-status-error/5 text-status-error"
        : "text-text-secondary";
  return (
    <div className="flex">
      <span className="w-7 shrink-0 border-r border-surface-700/30 px-1 text-right text-text-dim">{num}</span>
      <span className={`flex-1 whitespace-pre px-2 ${body}`}>{text}</span>
    </div>
  );
}

/** Static diff-viewer preview, mirroring DiffFileViewer's tokens so the user
 *  sees what review will look like in the chosen theme. Decorative. */
function DiffPreview() {
  return (
    <div className={`${PANEL} border-l border-surface-700`} aria-hidden="true">
      <span className={PANEL_LABEL}>Diff viewer</span>
      <div className="overflow-hidden rounded-md border border-surface-700/40 bg-surface-900 font-mono text-[11px]">
        <div className="border-y border-surface-700/20 bg-surface-850 px-2 py-1 text-accent-600">@@ src/auth.ts @@</div>
        <DiffRow num="11" text="   const now = Date.now();" />
        <DiffRow num="" text=" - if (exp < now) reject();" kind="del" />
        <DiffRow num="12" text=" + if (exp <= now) reject();" kind="add" />
        <DiffRow num="13" text="   next();" />
      </div>
    </div>
  );
}

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
    () => (typeof document !== "undefined" ? document.documentElement.dataset.theme : undefined) ?? null,
  );
  const [error, setError] = useState<string | null>(null);
  const { select, pending } = useThemeMutation();
  const continueRef = useRef<HTMLButtonElement>(null);
  const previousFocusRef = useRef<HTMLElement | null>(null);

  // Capture the previously focused element on mount and restore it on unmount
  // so keyboard users return to where they were instead of losing focus to
  // document.body, matching DeleteSessionDialog and CommandPalette.
  useEffect(() => {
    previousFocusRef.current = document.activeElement as HTMLElement | null;
    fetchThemes().then(setThemes);
    continueRef.current?.focus();
    return () => {
      previousFocusRef.current?.focus?.();
    };
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
      <div className="bg-surface-800 border border-surface-700/50 rounded-lg w-[460px] max-w-[92vw] lg:w-auto lg:max-w-[1024px] shadow-2xl animate-slide-up overflow-hidden">
        <div className="lg:flex lg:items-stretch">
          <ComposerPreview />
          <div className="lg:flex-1 lg:min-w-[380px]">
            <div className="px-5 py-4 border-b border-surface-700">
              <h2 id="theme-intro-title" className="text-sm font-semibold text-text-bright">
                Welcome! Choose your theme
              </h2>
              <p className="mt-1 text-xs text-text-dim">
                Pick a look for the dashboard and TUI. You can change it any time from Settings, under Appearance.
              </p>
            </div>

            <div className="p-5 space-y-4">
              <div role="listbox" aria-label="Themes" className="grid grid-cols-2 gap-2 max-h-64 overflow-y-auto">
                {themes.map((t) => {
                  const active = t === selected;
                  return (
                    <button
                      key={t}
                      type="button"
                      role="option"
                      aria-selected={active}
                      disabled={pending && !active}
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
          <DiffPreview />
        </div>
      </div>
    </div>
  );
}
