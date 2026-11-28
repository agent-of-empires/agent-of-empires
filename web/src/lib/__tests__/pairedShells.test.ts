import { describe, expect, it } from "vitest";

import { makeSession } from "../../components/__tests__/fixtures";
import { liveShellIndices, pairedShellGroups } from "../pairedShells";
import type { AuxiliaryObservation } from "../types";

const host = (index: number, state: AuxiliaryObservation["state"] = "alive"): AuxiliaryObservation => ({
  target: { kind: "host", index },
  state,
});
const container = (index: number): AuxiliaryObservation => ({ target: { kind: "container", index }, state: "alive" });

describe("pairedShellGroups", () => {
  const a = makeSession({ id: "a", title: "A", auxiliary: [host(0, "absent"), host(2)] });
  const b = makeSession({ id: "b", title: "B", is_sandboxed: true, auxiliary: [host(0), container(0)] });
  const c = makeSession({ id: "c", title: "C", auxiliary: [host(0, "dead"), container(1)] });
  const tool: AuxiliaryObservation = { target: { kind: "tool", tool_name: "lazygit" }, state: "alive" };
  const d = makeSession({ id: "d", title: "D", auxiliary: [tool] });

  it("lists live shells by Session, the given one first, and marks the ones a tab shows", () => {
    const groups = pairedShellGroups(
      [a, b, c, d],
      [
        { sessionId: "b", index: 0 },
        { sessionId: "a", index: 5 },
      ],
      (session) => (session.id === "b" ? "container" : "host"),
      "b",
    );
    expect(groups.map((g) => [g.session.id, g.shells.map((s) => `${s.index}:${s.mode}:${s.open}`)])).toEqual([
      ["b", ["0:host:false", "0:container:true"]],
      // A just-opened tab shows before the Daemon reports its shell.
      ["a", ["2:host:false", "5:host:true"]],
    ]);
  });

  it("skips dead, absent and tool panes, and container shells of an unsandboxed Session", () => {
    expect(pairedShellGroups([c, d], [], () => "host", null)).toEqual([]);
  });

  it("liveShellIndices reads alive shells of either mode", () => {
    expect([liveShellIndices(a), liveShellIndices(b), liveShellIndices(undefined)]).toEqual([[2], [0, 0], []]);
  });
});
