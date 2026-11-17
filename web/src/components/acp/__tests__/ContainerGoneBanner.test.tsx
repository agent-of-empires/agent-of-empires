// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";

import { useReducer } from "react";

import { emptyAcpState, type AcpEvent } from "../../../lib/acpTypes";
import { reducer, type Action } from "../../../hooks/acpSession/reducer";
import type { RecreateContainerBlock } from "../../sidebar/sessionActionTargets";
import { composerBlockedReason, containerGoneRemedy, type ContainerGoneRemedy } from "../containerGone";
import { isNewSessionStarting } from "../startupWait";
import { SessionBanners } from "../SessionBanners";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const GONE = "This Session's sandbox container is gone. Recreate it or start a new Session and container.";
const frame = (seq: number, event: AcpEvent): Action => ({ kind: "frame", frame: { session_id: "s1", seq, event } });

/** A started Session whose last startup error is `event`, owned as StructuredView owns it. */
function renderFailed(
  event: AcpEvent,
  {
    canRestart = true,
    block = null as RecreateContainerBlock | null,
    onRestore = undefined as (() => Promise<boolean>) | undefined,
  } = {},
) {
  // As App derives both from one Session.
  const recreateContainerTitle = block === null ? "my-session" : undefined;
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
        recreateContainerBlock={block}
        onRestore={onRestore}
        newSession={{ project: "proj", open: openNew }}
      />
    );
  }
  render(<Owner />);
  return { requests, openNew };
}

const GONE_LINE = "This Session's sandbox container is gone.";
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
    fireEvent.click(screen.getByText("New Session in proj"));
    expect(openNew).toHaveBeenCalledTimes(1);
    // The server's message stays reachable, word for word.
    fireEvent.click(screen.getByText("Details"));
    expect(screen.getByText(GONE).tagName).toBe("PRE");
  });

  it("offers Recreate only where it's offered, and otherwise names the control that is", () => {
    const rows: [string, { canRestart?: boolean; block?: RecreateContainerBlock | null }, string, string[]][] = [
      ["normal", {}, "Recreate it to use this Session again.", ["Recreate container", "New Session in proj"]],
      // Read-only and CityHall dashboards, whatever the Session's state.
      ["read-only", { canRestart: false }, "Ask the owner to recreate the container.", []],
      ["read-only, trashed", { canRestart: false, block: "trashed" }, "Ask the owner to recreate the container.", []],
      [
        "archived",
        { block: "archived" },
        "Unarchive this Session from the sidebar, then recreate its container.",
        ["New Session in proj"],
      ],
      [
        "trashed",
        { block: "trashed" },
        "Restore this Session from Trash, then recreate its container.",
        ["Restore", "New Session in proj"],
      ],
      ["creating", { block: "creating" }, "Wait for this Session to finish being created.", ["New Session in proj"]],
      ["deleting", { block: "deleting" }, "Wait for this Session to finish being deleted.", ["New Session in proj"]],
    ];
    const got = rows.map(([name, opts]) => {
      renderFailed(typed, { ...opts, onRestore: vi.fn(async () => true) });
      const seen = [name, screen.getByTestId("container-gone-line").textContent, buttons()];
      cleanup();
      return seen;
    });
    expect(got).toEqual(rows.map(([name, , line, actions]) => [name, `${GONE_LINE} ${line}`, [...actions, "Details"]]));
  });

  it("Restore restores the trashed Session, and comes back if that fails", async () => {
    let settle: (ok: boolean) => void = () => {};
    const onRestore = vi.fn(() => new Promise<boolean>((resolve) => (settle = resolve)));
    renderFailed(typed, { block: "trashed", onRestore });
    fireEvent.click(screen.getByTestId("container-gone-restore"));
    expect(onRestore).toHaveBeenCalledTimes(1);
    expect(screen.getByTestId("container-gone-restore").textContent).toBe("Restoring…");
    expect(screen.getByTestId("container-gone-restore").hasAttribute("disabled")).toBe(true);
    await act(async () => settle(false));
    expect(screen.getByTestId("container-gone-restore").textContent).toBe("Restore");
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

describe("composerBlockedReason", () => {
  it("names the same remedy as the banner, for every Session state the composer is shown in", () => {
    const rows: [boolean, RecreateContainerBlock | null, ContainerGoneRemedy, string][] = [
      [true, null, "recreate", "Recreate the container to send messages."],
      [false, null, "owner", "Ask the owner to recreate the container."],
      [false, "deleting", "owner", "Ask the owner to recreate the container."],
      [true, "creating", "creating", "Wait for this Session to finish being created."],
      [true, "deleting", "deleting", "Wait for this Session to finish being deleted."],
      [true, "not_sandboxed", "none", "Start a new Session to send messages."],
    ];
    const got = rows.map(([canRestart, block]) => {
      const remedy = containerGoneRemedy(canRestart, block);
      return [canRestart, block, remedy, composerBlockedReason(remedy)];
    });
    expect(got).toEqual(rows);
  });
});
