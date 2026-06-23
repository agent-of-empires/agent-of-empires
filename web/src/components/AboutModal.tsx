import { useEffect, useRef, useState } from "react";
import { fetchAbout } from "../lib/api";
import { writeClipboard } from "../lib/clipboard";
import { reportError, reportInfo } from "../lib/toastBus";

interface Props {
  onClose: () => void;
  /** Id of the currently-open session, or null on the dashboard / no session.
   *  When set, the modal shows a "Copy session id" row; the id is otherwise
   *  unreachable in a PWA install where the URL bar is hidden. */
  sessionId: string | null;
}

interface LinkRow {
  label: string;
  href: string;
  display: string;
}

const LINKS: LinkRow[] = [
  {
    label: "Website",
    href: "https://agent-of-empires.com",
    display: "agent-of-empires.com",
  },
  {
    label: "GitHub",
    href: "https://github.com/agent-of-empires/agent-of-empires",
    display: "github.com/agent-of-empires/agent-of-empires",
  },
  {
    label: "Twitter",
    href: "https://x.com/agentofempires",
    display: "@agentofempires",
  },
];

function buildFeedbackUrl(version: string | null): string {
  const body = [
    "<!-- Replace with a description of what happened and what you expected. -->",
    "",
    "**Environment**",
    `- Version: ${version ?? "unknown"}`,
    `- Platform: ${navigator.platform}`,
    `- User agent: ${navigator.userAgent}`,
    "",
    "**Steps to reproduce**",
    "1. ",
    "",
    "**Expected**",
    "",
    "**Actual**",
  ].join("\n");
  const params = new URLSearchParams({
    title: "web dashboard: ",
    body,
    labels: "web,feedback",
  });
  return `https://github.com/agent-of-empires/agent-of-empires/issues/new?${params.toString()}`;
}

export function AboutModal({ onClose, sessionId }: Props) {
  const closeRef = useRef<HTMLButtonElement>(null);
  const [version, setVersion] = useState<string | null>(null);

  useEffect(() => {
    closeRef.current?.focus();
    fetchAbout().then((a) => setVersion(a?.version ?? null));
  }, []);

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="about-modal-title"
      className="fixed inset-0 bg-black/60 flex items-center justify-center z-50 animate-fade-in"
      onClick={onClose}
    >
      <div
        className="bg-surface-800 border border-surface-700/50 rounded-lg w-[420px] max-w-[90vw] shadow-2xl animate-slide-up"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between px-5 py-4 border-b border-surface-700">
          <div className="flex items-center gap-2 min-w-0">
            <img src="/icon-192.png" alt="" width="24" height="24" className="rounded-sm shrink-0" />
            <h2 id="about-modal-title" className="text-sm font-semibold text-text-bright truncate">
              Agent of Empires
            </h2>
            {version && (
              <span className="font-mono text-[11px] text-text-muted shrink-0" aria-label={`Version ${version}`}>
                v{version}
              </span>
            )}
          </div>
          <button
            ref={closeRef}
            onClick={onClose}
            className="text-text-muted hover:text-text-secondary cursor-pointer text-lg leading-none px-1"
            aria-label="Close"
          >
            &times;
          </button>
        </div>

        <div className="p-5 space-y-4">
          <p className="text-sm text-text-secondary">
            Terminal session manager for parallel AI coding agents. Open source, cross-platform, sandboxed.
          </p>

          {sessionId && (
            <button
              type="button"
              onClick={async () => {
                const ok = await writeClipboard(sessionId);
                if (ok) reportInfo("Copied session id");
                else reportError("Copy failed");
              }}
              className="w-full flex items-center justify-between gap-3 px-3 py-2 rounded-md bg-surface-900 border border-surface-700/50 hover:border-surface-700 hover:bg-surface-850 transition-colors group cursor-pointer text-left"
              title="Copy session id to clipboard"
              aria-label="Copy session id"
            >
              <span className="font-mono text-[11px] uppercase tracking-wider text-text-muted shrink-0">
                Session id
              </span>
              <span className="text-sm text-text-secondary group-hover:text-text-primary font-mono truncate">
                {sessionId}
              </span>
            </button>
          )}

          <div className="space-y-2">
            {LINKS.map((link) => (
              <a
                key={link.href}
                href={link.href}
                target="_blank"
                rel="noopener noreferrer"
                className="flex items-center justify-between gap-3 px-3 py-2 rounded-md bg-surface-900 border border-surface-700/50 hover:border-surface-700 hover:bg-surface-850 transition-colors group"
              >
                <span className="font-mono text-[11px] uppercase tracking-wider text-text-muted">{link.label}</span>
                <span className="text-sm text-brand-500 group-hover:text-brand-400 font-mono truncate">
                  {link.display}
                </span>
              </a>
            ))}
          </div>

          <a
            href={buildFeedbackUrl(version)}
            target="_blank"
            rel="noopener noreferrer"
            className="block text-center py-2 rounded-md border border-surface-700/50 text-sm text-text-secondary hover:bg-surface-850 hover:text-text-primary hover:border-surface-700 transition-colors"
          >
            Send feedback
          </a>
        </div>

        <div className="px-5 py-3 border-t border-surface-700">
          <p className="font-mono text-[11px] text-text-dim">Built for developers running many agents at once.</p>
        </div>
      </div>
    </div>
  );
}
