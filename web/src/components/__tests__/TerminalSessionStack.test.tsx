// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import type { SessionResponse } from "../../lib/types";
import { makeSession as baseSession } from "./fixtures";

vi.mock("../TerminalView", () => ({
  TerminalView: ({ session, active }: { session: SessionResponse; active: boolean }) => (
    <div data-testid={`terminal-${session.id}`} data-active={String(active)}>
      {session.title}
    </div>
  ),
}));

import { normalizePersistentTerminalLimit } from "../../lib/persistentTerminals";
import { TerminalSessionStack } from "../TerminalSessionStack";

afterEach(() => {
  cleanup();
});

const makeSession = (id: string) => baseSession({ id, title: id, project_path: `/tmp/${id}`, status: "Running" });

/** Three sessions with a keep-alive limit of 2, activated s1 then s2 so both stay mounted. */
async function mountThreeKeepingTwo() {
  const sessions = [makeSession("s1"), makeSession("s2"), makeSession("s3")];
  const activate = (id: string, limit: number) =>
    rerender(
      <TerminalSessionStack
        activeSessionId={id}
        sessions={sessions}
        persistent={true}
        maxPersistentTerminals={limit}
      />,
    );
  const { rerender } = render(
    <TerminalSessionStack activeSessionId="s1" sessions={sessions} persistent={true} maxPersistentTerminals={2} />,
  );
  await waitFor(() => {
    expect(screen.getByTestId("terminal-s1")).toBeDefined();
  });
  activate("s2", 2);
  await waitFor(() => {
    expect(screen.getByTestId("terminal-s1")).toBeDefined();
    expect(screen.getByTestId("terminal-s2")).toBeDefined();
  });
  return activate;
}

describe("TerminalSessionStack", () => {
  it("renders only the active session when persistence is disabled", () => {
    const sessions = [makeSession("s1"), makeSession("s2")];
    const { rerender } = render(<TerminalSessionStack activeSessionId="s1" sessions={sessions} persistent={false} />);

    expect(screen.getByTestId("terminal-s1").dataset.active).toBe("true");
    expect(screen.queryByTestId("terminal-s2")).toBeNull();

    rerender(<TerminalSessionStack activeSessionId="s2" sessions={sessions} persistent={false} />);

    expect(screen.queryByTestId("terminal-s1")).toBeNull();
    expect(screen.getByTestId("terminal-s2").dataset.active).toBe("true");
  });

  it("evicts older inactive sessions beyond the configured limit", async () => {
    const activate = await mountThreeKeepingTwo();

    activate("s3", 1);
    await waitFor(() => {
      expect(screen.queryByTestId("terminal-s1")).toBeNull();
      expect(screen.queryByTestId("terminal-s2")).toBeNull();
      expect(screen.getByTestId("terminal-s3").dataset.active).toBe("true");
    });
  });

  it("counts the configured limit as the total mounted terminal count", async () => {
    const activate = await mountThreeKeepingTwo();

    activate("s3", 2);
    await waitFor(() => {
      expect(screen.queryByTestId("terminal-s1")).toBeNull();
      expect(screen.getByTestId("terminal-s2").dataset.active).toBe("false");
      expect(screen.getByTestId("terminal-s3").dataset.active).toBe("true");
    });
  });

  it("normalizes configured limits to the supported range", () => {
    expect(normalizePersistentTerminalLimit(0)).toBe(1);
    expect(normalizePersistentTerminalLimit(5.4)).toBe(5);
    expect(normalizePersistentTerminalLimit(99)).toBe(50);
    expect(normalizePersistentTerminalLimit("10")).toBe(5);
  });
});
