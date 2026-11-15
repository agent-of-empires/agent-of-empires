import { useEffect, useState } from "react";
import { createPortal } from "react-dom";

import { fetchAgentInstallInfo } from "../../lib/api";
import { writeClipboard } from "../../lib/clipboard";
import { RecreateContainerDialog } from "../sidebar/RecreateContainerDialog";
import { RecoveryActionRow, type RecoveryAction } from "./RecoveryActionRow";

export interface NewSessionLike {
  /** The Project, as the button names it. */
  project: string;
  open: () => void;
}

const NO_INSTALL = { error: null, output: null };

/** The startup error for a sandboxed Session whose container is gone. Retry isn't offered: Recreate is the fix. */
export function ContainerGoneBanner({
  sessionId,
  message,
  canRestart,
  recreateContainerTitle,
  newSession,
  onRecreated,
}: {
  sessionId: string;
  message: string;
  /** False on read-only and CityHall dashboards, which get the owner line instead of actions. */
  canRestart: boolean;
  /** The Session's title when the dashboard may recreate its container. */
  recreateContainerTitle?: string;
  newSession?: NewSessionLike;
  onRecreated?: () => void;
}) {
  const [name, setName] = useState("The agent");
  const [recreating, setRecreating] = useState(false);
  const [detailsOpen, setDetailsOpen] = useState(false);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    let alive = true;
    void fetchAgentInstallInfo(sessionId).then((info) => {
      if (alive && info?.display_name) setName(info.display_name);
    });
    return () => {
      alive = false;
    };
  }, [sessionId]);

  const actions: RecoveryAction[] = canRestart
    ? [
        ...(recreateContainerTitle !== undefined
          ? [{ label: "Recreate container", onClick: () => setRecreating(true), testId: "container-gone-recreate" }]
          : []),
        ...(newSession
          ? [
              {
                label: `New Session in ${newSession.project}`,
                onClick: newSession.open,
                testId: "container-gone-new-session",
              },
            ]
          : []),
      ]
    : [];
  const [primary, ...secondary] = actions;

  return (
    <section
      aria-label="Structured view startup error"
      data-testid="container-gone-banner"
      className="border-b border-status-error/40 bg-status-error/10 px-4 py-3 text-text-primary"
    >
      <div role="alert">
        <h2 className="text-sm font-semibold">{name} couldn&apos;t start in Structured view</h2>
        <p className="mt-1 text-xs text-text-subtle">
          This Session&apos;s sandbox container is gone. Recreate it to use this Session again.
        </p>
        {!canRestart && (
          <p data-testid="container-gone-owner" className="mt-1 text-xs text-text-subtle">
            Ask the owner to recreate the container.
          </p>
        )}
      </div>
      {primary && <RecoveryActionRow primary={primary} secondary={secondary} install={NO_INSTALL} />}
      <div className="mt-3 border-t border-status-error/30 pt-2">
        <button
          type="button"
          data-testid="container-gone-details"
          aria-expanded={detailsOpen}
          onClick={() => setDetailsOpen(!detailsOpen)}
          className="text-xs font-medium text-text-primary underline-offset-2 hover:underline"
        >
          Details
        </button>
        {detailsOpen && (
          <div className="mt-2">
            <pre className="whitespace-pre-wrap break-all font-mono text-xs text-text-subtle">{message}</pre>
            <button
              type="button"
              onClick={() => void writeClipboard(message).then(setCopied)}
              className="mt-1 text-xs font-medium text-text-primary underline-offset-2 hover:underline"
            >
              {copied ? "Copied" : "Copy"}
            </button>
          </div>
        )}
      </div>
      {recreating &&
        recreateContainerTitle !== undefined &&
        createPortal(
          <RecreateContainerDialog
            sessionId={sessionId}
            sessionTitle={recreateContainerTitle}
            onClose={() => setRecreating(false)}
            onRecreated={onRecreated}
          />,
          document.body,
        )}
    </section>
  );
}
