import { useEffect, useRef, useState } from "react";
import { useFocusTrap } from "../hooks/useFocusTrap";
import { fetchAbout } from "../lib/api";
import { writeClipboard } from "../lib/clipboard";
import { reportError, reportInfo } from "../lib/toastBus";
import { BUTTON_SECONDARY } from "../lib/surfaces";
import { CloseButton, Dialog } from "./Dialog";
import { INSET_CARD } from "../lib/surfaces";

const LINK_ROW =
  "group flex items-center justify-between gap-3 min-h-11 px-3 py-2 hover:bg-surface-800 transition-colors";
const LINK_LABEL = "font-mono text-[11px] uppercase tracking-wider text-text-muted";

interface Props {
  onClose: () => void;
  /** Id of the currently-open session, or null on the dashboard / no session. */
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
  const dialogRef = useRef<HTMLDivElement>(null);
  const [version, setVersion] = useState<string | null>(null);

  useEffect(() => {
    closeRef.current?.focus();
    fetchAbout().then((a) => setVersion(a?.version ?? null));
  }, []);

  // Tab and Shift+Tab wrap instead of leaving for the dialogs behind.
  useFocusTrap(() => dialogRef.current);

  return (
    <Dialog
      id="about-modal"
      rootRef={dialogRef}
      describedBy={false}
      bodyClassName="space-y-4"
      onDismiss={onClose}
      title={
        <>
          <img src="/icon-192.png" alt="" width="24" height="24" className="rounded-sm shrink-0" />
          <span className="truncate text-text-bright">Agent of Empires</span>
        </>
      }
      headerEnd={
        <>
          {version && (
            <span className="font-mono text-[11px] text-text-muted shrink-0" aria-label={`Version ${version}`}>
              v{version}
            </span>
          )}
          <CloseButton buttonRef={closeRef} onClick={onClose} />
        </>
      }
      footer={
        <p className="mr-auto font-mono text-[11px] text-text-dim">Built for developers running many agents at once.</p>
      }
    >
      <p className="text-[15px] md:text-sm text-text-secondary">
        Terminal session manager for parallel AI coding agents. Open source, cross-platform, sandboxed.
      </p>

      <div className={`${INSET_CARD} overflow-hidden divide-y divide-surface-700/40`}>
        {sessionId && (
          <button
            type="button"
            onClick={async () => {
              const ok = await writeClipboard(sessionId);
              if (ok) reportInfo("Copied session id");
              else reportError("Copy failed");
            }}
            className={`${LINK_ROW} w-full cursor-pointer text-left`}
            title="Copy session id to clipboard"
            aria-label="Copy session id"
          >
            <span className={`${LINK_LABEL} shrink-0`}>Session id</span>
            <span className="text-sm text-text-secondary group-hover:text-text-primary font-mono truncate">
              {sessionId}
            </span>
          </button>
        )}
        {LINKS.map((link) => (
          <a key={link.href} href={link.href} target="_blank" rel="noopener noreferrer" className={LINK_ROW}>
            <span className={LINK_LABEL}>{link.label}</span>
            <span className="text-sm text-brand-500 group-hover:text-brand-400 font-mono truncate">{link.display}</span>
          </a>
        ))}
      </div>

      <a
        href={buildFeedbackUrl(version)}
        target="_blank"
        rel="noopener noreferrer"
        className={`${BUTTON_SECONDARY} w-full`}
      >
        Send feedback
      </a>
    </Dialog>
  );
}
