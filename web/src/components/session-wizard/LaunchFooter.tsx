import { useEffect, type ReactNode } from "react";
import { useServerDown, OFFLINE_TITLE } from "../../lib/connectionState";
import { hasFinePointer } from "../../lib/platform";
import { launchEligible } from "./createRequest";
import { BUTTON_GHOST, BUTTON_PRIMARY } from "../../lib/surfaces";
import { Spinner } from "../Dialog";

const ERROR_BANNER = "mb-3 rounded-lg bg-status-error/10 p-3 text-[15px] md:text-sm text-status-error";

interface LaunchData {
  path: string;
  tool: string;
  scratch: boolean;
  [key: string]: unknown;
}

interface Props {
  data: LaunchData;
  isSubmitting: boolean;
  error: string | null;
  /** Shown in place of `error`: a create the server refused. */
  failure?: ReactNode;
  onSubmit: () => void;
  /** CityHall name-only mode: the server derives the project. */
  nameOnly?: boolean;
  /** False until the wizard's profile defaults have been applied (or their
   *  fetch has failed); a launch before then would submit placeholder
   *  sandbox/worktree/yolo values. Omitted means ready. */
  defaultsReady?: boolean;
  /** Offered while a create runs: closes the wizard and lets it finish unattended. */
  onBackground?: () => void;
  /** Blocks Launch without showing a create, while a check that may itself resend runs. */
  held?: boolean;
}

const isMac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.userAgent);
/** Launch button, submit gate, Cmd/Ctrl+Enter shortcut, and error banners. */
export function LaunchFooter({
  data,
  isSubmitting,
  error,
  failure,
  onSubmit,
  nameOnly = false,
  defaultsReady = true,
  onBackground,
  held = false,
}: Props) {
  const offline = useServerDown();
  const canSubmit = !isSubmitting && !held && launchEligible(data, { ready: defaultsReady, offline, nameOnly });

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Enter" && (e.metaKey || e.ctrlKey) && canSubmit) {
        e.preventDefault();
        onSubmit();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [canSubmit, onSubmit]);

  return (
    <div>
      {failure}
      {/* Shown with a failure, the error says why its actions are disabled. */}
      {error && <div className={ERROR_BANNER}>{error}</div>}
      {offline && <div className={ERROR_BANNER}>{OFFLINE_TITLE}</div>}
      <button type="button" onClick={onSubmit} disabled={!canSubmit} className={`${BUTTON_PRIMARY} w-full`}>
        {isSubmitting ? (
          <>
            <Spinner />
            Creating session...
          </>
        ) : (
          <span>
            Launch session {hasFinePointer() && <span className="opacity-60">({isMac ? "⌘" : "Ctrl"}+Enter)</span>}
          </span>
        )}
      </button>
      {isSubmitting && onBackground && (
        <button type="button" onClick={onBackground} className={`${BUTTON_GHOST} w-full mt-2`}>
          Continue in background
        </button>
      )}
    </div>
  );
}
