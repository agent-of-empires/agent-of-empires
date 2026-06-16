// @vitest-environment jsdom
//
// StructuredView forwards the active session's repo roots into
// AcpFileRefContext so the tool cards can render file paths
// repo-relative (#2143). The full component mounts the live ACP runtime
// (WebSocket worker), so we mock AcpRuntime down to a probe that reads
// the context and renders what it received. This pins the one-line
// provider plumbing that no other unit test exercises.

import { describe, expect, it, vi } from "vitest";
import { render } from "@testing-library/react";

import { useAcpFileRef } from "./AcpFileRefContext";
import type { FileRefSession } from "../../lib/fileRef";

vi.mock("./AcpRuntime", () => ({
  SUBAGENT_TASK_NAME: "Task",
  TODO_GROUP_NAME: "Todos",
  TOOL_GROUP_NAME: "Tools",
  AcpRuntime: () => {
    const { fileRefSession } = useAcpFileRef();
    return <div data-testid="probe">{fileRefSession?.project_path ?? "none"}</div>;
  },
}));

import { StructuredView } from "./StructuredView";

describe("StructuredView fileRef plumbing (#2143)", () => {
  it("provides the session repo roots through AcpFileRefContext", () => {
    const fileRefSession: FileRefSession = {
      project_path: "/Users/me/wt",
      main_repo_path: null,
      workspace_repos: [],
    };
    const { getByTestId } = render(
      <StructuredView
        sessionId="s1"
        acpWorkerState="running"
        tool="claude"
        archivedAt={null}
        snoozedUntil={null}
        fileRefSession={fileRefSession}
      />,
    );
    expect(getByTestId("probe").textContent).toBe("/Users/me/wt");
  });

  it("provides no session when none is passed", () => {
    const { getByTestId } = render(
      <StructuredView sessionId="s1" acpWorkerState="absent" tool="claude" archivedAt={null} snoozedUntil={null} />,
    );
    expect(getByTestId("probe").textContent).toBe("none");
  });
});
