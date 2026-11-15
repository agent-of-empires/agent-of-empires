// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";

import { useReducer } from "react";

import { emptyAcpState, type AcpEvent } from "../../../lib/acpTypes";
import { reducer, type Action } from "../../../hooks/acpSession/reducer";
import { isNewSessionStarting } from "../startupWait";
import { SessionBanners } from "../SessionBanners";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const GONE = "This Session's sandbox container was removed. Start the Session again to create a new one.";
const frame = (seq: number, event: AcpEvent): Action => ({ kind: "frame", frame: { session_id: "s1", seq, event } });

/** A started Session whose last startup error is `event`, owned as StructuredView owns it. */
function renderFailed(
  event: AcpEvent,
  { canRestart = true, recreateContainerTitle = "my-session" as string | undefined } = {},
) {
  const requests: string[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init?: RequestInit) => {
      requests.push(`${init?.method ?? "GET"} ${url}`);
      return new Response(JSON.stringify(url.endsWith("/install-info") ? { display_name: "Claude" } : {}));
    }),
  );
  const openNew = vi.fn();
  function Owner() {
    const [state, send] = useReducer(reducer, reducer(emptyAcpState(), frame(1, event)));
    const connection = {
      status: "open",
      hasEverOpened: true,
      reconnecting: false,
      retryCount: 0,
      maxRetries: 7,
    } as const;
    return (
      <SessionBanners
        sessionId="s1"
        state={state}
        startingAgent={isNewSessionStarting(state, connection, "absent", true) ? "Claude" : null}
        canRestart={canRestart}
        onRestarted={(epoch, stoppedSent, retriedError) =>
          send({ kind: "restart_started", epoch, stoppedSent, retriedError })
        }
        acpWorkerState="absent"
        trashedAt={null}
        archivedAt={null}
        snoozedUntil={null}
        dismissError={vi.fn()}
        onSwitchToTerminal={vi.fn()}
        recreateContainerTitle={recreateContainerTitle}
        newSession={{ project: "proj", open: openNew }}
      />
    );
  }
  render(<Owner />);
  return { requests, openNew };
}

const typed: AcpEvent = { AgentStartupError: { message: GONE, cause: { kind: "container_missing" } } };
const buttons = () =>
  Array.from(screen.getByTestId("container-gone-banner").querySelectorAll("button"), (b) => b.textContent);

describe("ContainerGoneBanner", () => {
  it("offers Recreate container and a New Session in the Project, not Retry or Terminal view", async () => {
    const { openNew } = renderFailed(typed);
    expect(screen.queryByTestId("startup-error-banner")).toBeNull();
    expect((await screen.findByRole("heading")).textContent).toBe("Claude couldn't start in Structured view");
    expect(screen.getByRole("alert").textContent).toContain(
      "This Session's sandbox container is gone. Recreate it to use this Session again.",
    );
    expect(buttons()).toEqual(["Recreate container", "New Session in proj", "Details"]);
    expect(screen.queryByTestId("container-gone-owner")).toBeNull();
    fireEvent.click(screen.getByText("New Session in proj"));
    expect(openNew).toHaveBeenCalledTimes(1);
    // The server's message stays reachable, word for word.
    fireEvent.click(screen.getByText("Details"));
    expect(screen.getByText(GONE).tagName).toBe("PRE");
  });

  it("asks the owner, with no actions, where the server refuses a restart", () => {
    // Read-only and CityHall dashboards, even for a container the Session menu could recreate.
    renderFailed(typed, { canRestart: false });
    expect(screen.getByTestId("container-gone-owner").textContent).toBe("Ask the owner to recreate the container.");
    expect(buttons()).toEqual(["Details"]);
  });

  it("hands Recreate to the startup line once the Daemon accepts it", async () => {
    const { requests } = renderFailed(typed);
    fireEvent.click(screen.getByText("Recreate container"));
    expect(requests.some((r) => r.startsWith("POST"))).toBe(false);
    await act(async () => fireEvent.click(screen.getByTestId("recreate-container-confirm")));
    expect(requests).toContain("POST /api/sessions/s1/recreate-container");
    expect(screen.queryByTestId("container-gone-banner")).toBeNull();
    expect(screen.getByTestId("startup-wait-line").textContent).toContain("Starting Claude…");
  });

  it("leaves an untyped startup error to the generic banner", () => {
    renderFailed({ AgentStartupError: { message: GONE } });
    expect(screen.queryByTestId("container-gone-banner")).toBeNull();
    expect(screen.getByTestId("startup-error-banner")).toBeTruthy();
  });
});
