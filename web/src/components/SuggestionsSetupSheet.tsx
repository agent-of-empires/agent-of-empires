import { useCallback, useEffect, useState, type ReactNode } from "react";
import { fetchSuggestions, refreshSuggestions, runSuggestionChecks, type PluginDiscoveryResult } from "../lib/api";
import { sourceLabel } from "../lib/featuredSuggestionSources";
import type { SuggestionSetupCheck, SuggestionsSnapshot } from "../lib/types";
import { PluginInstallConsentModal } from "./settings/PluginInstallConsentModal";
import { PluginJobProgressModal } from "./settings/PluginJobProgressModal";
import { sourceFromCommand, usePluginMarketplace, type PluginJobRef } from "./settings/pluginFlows";

interface Props {
  /** Featured providers that declare `[suggestions]` and are not installed yet. */
  sources: PluginDiscoveryResult[];
  onClose: () => void;
  /** The sheet finished; the homepage section should reload. */
  onFinished: () => void;
  onOpenMarketplace: () => void;
}

type Step = "pick" | "checks" | "done";

const POLL_MS = 2000;
const LINK = "cursor-pointer text-brand-500 hover:underline";
const GHOST = "h-8 rounded-md px-3 text-xs text-text-secondary hover:bg-surface-800 disabled:opacity-50";
const PRIMARY =
  "h-8 rounded-md bg-brand-500 px-3 text-xs font-medium text-black hover:bg-brand-400 disabled:opacity-50";

export function SuggestionsSetupSheet({ sources, onClose, onFinished, onOpenMarketplace }: Props) {
  const [step, setStep] = useState<Step>("pick");
  const [checked, setChecked] = useState(() => new Set(sources.map((s) => s.slug)));
  // Sources still to install; the head is the one being previewed or installed.
  const [queue, setQueue] = useState<string[]>([]);
  const [job, setJob] = useState<PluginJobRef | null>(null);
  const market = usePluginMarketplace(setJob);
  const installing = queue.length > 0;
  const toDone = useCallback(() => setStep("done"), []);

  // Once a plugin is installed, closing finishes the sheet so the hint does not offer the same install again.
  const close = step === "pick" ? onClose : onFinished;

  useEffect(() => {
    if (step === "done") return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !installing) close();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [step, installing, close]);

  const startInstall = () => {
    const picked = sources.filter((s) => checked.has(s.slug)).map((s) => sourceFromCommand(s.install_command));
    const [head] = picked;
    if (!head) return;
    setQueue(picked);
    void market.install(head);
  };

  const jobClosed = () => {
    const [, next, ...after] = queue;
    setJob(null);
    setQueue(next ? [next, ...after] : []);
    if (next) void market.install(next);
    else setStep("checks");
  };

  const cancelInstall = () => {
    market.closeConsent();
    setQueue([]);
  };

  const title = step === "done" ? "Suggestions are on" : "Set up suggestions";

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-surface-950/80 p-4"
      role="dialog"
      aria-modal="true"
      aria-label={title}
      data-testid="suggestions-setup-sheet"
    >
      <div className="max-h-[90vh] w-full max-w-[560px] overflow-auto rounded-lg border border-surface-700 bg-surface-900">
        <div className="flex items-center justify-between border-b border-surface-700/40 px-4 py-3">
          <h2 className="text-[15px] font-semibold text-text-bright">{title}</h2>
          {step !== "done" && (
            <button
              type="button"
              title="Close"
              aria-label="Close"
              disabled={installing}
              onClick={close}
              className="size-8 rounded-md text-text-dim hover:bg-surface-800 disabled:opacity-50"
              data-testid="suggestions-setup-close"
            >
              ✕
            </button>
          )}
        </div>
        {step === "pick" && (
          <PickStep
            sources={sources}
            checked={checked}
            setChecked={setChecked}
            busy={installing}
            error={market.error ?? null}
            onInstall={startInstall}
            onCancel={onClose}
            onOpenMarketplace={onOpenMarketplace}
          />
        )}
        {step === "checks" && <ChecksStep onDone={toDone} onFinishLater={onFinished} />}
        {step === "done" && (
          <>
            <p className="px-4 py-4 text-sm text-text-secondary">
              You'll see up to 3 on your homepage. Choose a suggestion and start it to keep the work going.
            </p>
            <Footer note="Turn them off any time in Settings › Suggestions.">
              <button type="button" className={PRIMARY} onClick={onFinished} data-testid="suggestions-setup-show">
                Show suggestions
              </button>
            </Footer>
          </>
        )}
      </div>
      {market.consent && (
        <PluginInstallConsentModal
          consent={market.consent}
          busy={market.installBusy}
          error={market.installError}
          onApprove={() => void market.approveInstall()}
          onClose={cancelInstall}
        />
      )}
      {job && <PluginJobProgressModal jobId={job.id} title={job.title} onClose={jobClosed} />}
    </div>
  );
}

function Footer({ note, children }: { note?: string; children: ReactNode }) {
  return (
    <div className="flex flex-wrap items-center justify-between gap-3 border-t border-surface-700/40 px-4 py-3">
      <span className="min-w-0 flex-1 text-xs text-text-dim">{note}</span>
      <div className="flex w-full items-center justify-end gap-2 md:w-auto">{children}</div>
    </div>
  );
}

function PickStep(p: {
  sources: PluginDiscoveryResult[];
  checked: Set<string>;
  setChecked: (next: Set<string>) => void;
  busy: boolean;
  error: string | null;
  onInstall: () => void;
  onCancel: () => void;
  onOpenMarketplace: () => void;
}) {
  const toggle = (slug: string) => {
    const next = new Set(p.checked);
    if (!next.delete(slug)) next.add(slug);
    p.setChecked(next);
  };
  const count = p.sources.filter((s) => p.checked.has(s.slug)).length;
  return (
    <>
      <p className="px-4 py-3 text-sm text-text-secondary">
        <strong className="font-medium text-text-bright">Where do you keep your work?</strong> AoE suggests Sessions
        from what you pick.
      </p>
      {p.sources.length === 0 && (
        <p className="px-4 pb-3 text-xs text-text-dim" data-testid="suggestions-setup-empty">
          No featured sources yet. Other sources are in the Marketplace.
        </p>
      )}
      {p.sources.map((s) => {
        const on = p.checked.has(s.slug);
        return (
          <label
            key={s.slug}
            className={`flex min-h-11 cursor-pointer gap-3 px-4 py-3 ${on ? "bg-brand-500/[0.07]" : ""}`}
            data-testid={`suggestions-setup-option-${s.slug}`}
          >
            <input
              type="checkbox"
              checked={on}
              disabled={p.busy}
              onChange={() => toggle(s.slug)}
              className="mt-0.5 size-[18px] accent-brand-500"
            />
            <span className="min-w-0">
              <span className="block text-sm text-text-primary">{sourceLabel(s.slug)}</span>
              {s.description && <span className="block text-xs text-text-muted">{s.description}</span>}
            </span>
          </label>
        );
      })}
      {p.error && (
        <p className="px-4 py-2 text-xs text-status-error-text" data-testid="suggestions-setup-error">
          {p.error}
        </p>
      )}
      <div className="border-t border-surface-700/40 px-4 py-3 text-xs">
        <button
          type="button"
          className={LINK}
          title="Opens Settings › Plugins › Marketplace. Other sources there, like Linear, are unvetted"
          onClick={p.onOpenMarketplace}
          data-testid="suggestions-setup-marketplace"
        >
          More sources in the Marketplace ›
        </button>
      </div>
      <div className="flex flex-wrap items-center justify-between gap-3 border-t border-surface-700/40 px-4 py-3">
        <span
          className="cursor-help text-xs text-brand-500"
          title="AoE installs the selected plugins so it can read your assigned issues and review requests. Manage them in Settings › Plugins."
        >
          What gets installed?
        </span>
        <div className="flex w-full items-center justify-end gap-2 md:w-auto">
          <button type="button" className={`${GHOST} hidden md:inline-block`} onClick={p.onCancel}>
            Cancel
          </button>
          <button
            type="button"
            className={`${PRIMARY} w-full md:w-auto`}
            disabled={p.busy || count === 0}
            onClick={p.onInstall}
            data-testid="suggestions-setup-install"
          >
            Install {count} plugin{count === 1 ? "" : "s"}
          </button>
        </div>
      </div>
    </>
  );
}

function ChecksStep({ onDone, onFinishLater }: { onDone: () => void; onFinishLater: () => void }) {
  const [snapshot, setSnapshot] = useState<SuggestionsSnapshot | null>(null);
  const [polling, setPolling] = useState(true);
  const [rechecking, setRechecking] = useState(false);

  // Polls until the first suggestions land or a check fails; Check again restarts it.
  useEffect(() => {
    if (!polling) return;
    let alive = true;
    const tick = async () => {
      const next = await fetchSuggestions();
      if (!alive) return;
      if (next) setSnapshot(next);
      if (next && isReady(next)) onDone();
      else if (next && checkRows(next).some((c) => c.status === "fail")) setPolling(false);
      else timer = setTimeout(() => void tick(), POLL_MS);
    };
    let timer = setTimeout(() => void tick(), 0);
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [polling, onDone]);

  const checks = snapshot ? checkRows(snapshot) : [];
  const firstFail = checks.findIndex((c) => c.status === "fail");
  const failed = firstFail !== -1;

  const recheck = async () => {
    setRechecking(true);
    await runSuggestionChecks();
    if (snapshot?.providers.some((p) => p.status === "error")) await refreshSuggestions();
    setRechecking(false);
    setPolling(true);
  };

  return (
    <>
      <ul data-testid="suggestions-setup-checks">
        {checks.map((c, i) => {
          const notRun = failed && i > firstFail;
          return (
            <li
              key={c.id}
              className={`flex gap-3 border-b border-surface-700/40 px-4 py-3 ${notRun ? "text-text-dim" : "text-text-primary"}`}
              data-testid={`suggestions-setup-check-${c.id}`}
              data-status={notRun ? "not-run" : c.status}
            >
              <span aria-hidden="true" className={`w-[18px] shrink-0 text-center font-mono ${glyphClass(c.status)}`}>
                {notRun ? "" : glyph(c.status)}
              </span>
              <span className="min-w-0 text-sm">
                {c.label}
                {!notRun && c.detail && <span className="block text-xs text-text-dim">{c.detail}</span>}
                {!notRun && c.status === "fail" && c.fix && (
                  <span className="block text-xs text-text-secondary">{c.fix}</span>
                )}
              </span>
            </li>
          );
        })}
        {!failed && (
          <li className="flex gap-3 px-4 py-3" data-testid="suggestions-setup-waiting">
            <span aria-hidden="true" className="w-[18px] shrink-0 text-center font-mono text-brand-500">
              ⠙
            </span>
            <span className="text-sm text-text-primary">
              {checks.length === 0 ? "Waiting for the plugin to report its checks" : "Getting your first suggestions"}
              <span className="block text-xs text-text-dim">Usually under a minute.</span>
            </span>
          </li>
        )}
      </ul>
      <Footer
        note={failed ? "The plugin stays installed. You can come back to this in Settings › Suggestions." : undefined}
      >
        <button type="button" className={GHOST} onClick={onFinishLater} data-testid="suggestions-setup-later">
          Finish later
        </button>
        {failed && (
          <button
            type="button"
            className={PRIMARY}
            disabled={rechecking}
            onClick={() => void recheck()}
            data-testid="suggestions-setup-recheck"
          >
            Check again
          </button>
        )}
      </Footer>
    </>
  );
}

/** Every provider's reported checks, plus a failed row for a provider whose first fetch errored. */
function checkRows(s: SuggestionsSnapshot): SuggestionSetupCheck[] {
  return s.providers.flatMap((p) => [
    ...(p.checks ?? []),
    ...(p.status === "error"
      ? [
          {
            id: `${p.id}-fetch`,
            label: "Couldn't get your first suggestions",
            status: "fail" as const,
            detail: "",
            fix: p.error ?? "",
          },
        ]
      : []),
  ]);
}

/** Done once a provider is loaded, every one has fetched successfully, and no check failed. */
function isReady(s: SuggestionsSnapshot): boolean {
  return (
    s.providers.length > 0 &&
    s.providers.every((p) => p.status === "ok") &&
    !checkRows(s).some((c) => c.status === "fail")
  );
}

const glyph = (status: "ok" | "warn" | "fail") => (status === "ok" ? "✓" : status === "warn" ? "!" : "✕");
const glyphClass = (status: "ok" | "warn" | "fail") =>
  status === "ok" ? "text-status-running" : status === "warn" ? "text-status-waiting" : "text-status-error-text";
