import { describe, expect, it } from "vitest";
import { buildEffectiveHooks } from "./profileHooks";

describe("buildEffectiveHooks", () => {
  it("returns the three lifecycle events in TUI order with parity labels", () => {
    const groups = buildEffectiveHooks(undefined, undefined);
    expect(groups.map((g) => g.key)).toEqual([
      "on_create",
      "on_launch",
      "on_destroy",
    ]);
    expect(groups.map((g) => g.label)).toEqual([
      "On Create",
      "On Launch",
      "On Destroy",
    ]);
  });

  it("marks a non-empty profile override as `override`", () => {
    const groups = buildEffectiveHooks(
      { on_create: ["echo hi"] },
      { on_create: ["echo global"] },
    );
    const onCreate = groups.find((g) => g.key === "on_create")!;
    expect(onCreate.source).toBe("override");
    expect(onCreate.commands).toEqual(["echo hi"]);
  });

  it("marks an explicit empty array as `override-empty` (disables global)", () => {
    const groups = buildEffectiveHooks(
      { on_launch: [] },
      { on_launch: ["notify-send launch"] },
    );
    const onLaunch = groups.find((g) => g.key === "on_launch")!;
    expect(onLaunch.source).toBe("override-empty");
    expect(onLaunch.commands).toEqual([]);
  });

  it("inherits global commands when the profile has no override", () => {
    const groups = buildEffectiveHooks(
      {},
      { on_create: ["docker compose up"] },
    );
    const onCreate = groups.find((g) => g.key === "on_create")!;
    expect(onCreate.source).toBe("inherited");
    expect(onCreate.commands).toEqual(["docker compose up"]);
  });

  it("reports `none` when neither profile nor global define the event", () => {
    const groups = buildEffectiveHooks({}, {});
    expect(groups.every((g) => g.source === "none")).toBe(true);
    expect(groups.every((g) => g.commands.length === 0)).toBe(true);
  });

  it("treats a malformed (non-array) field as absent, not a crash", () => {
    const groups = buildEffectiveHooks(
      { on_create: "echo hi" as unknown as string[] },
      { on_create: ["echo global"] },
    );
    const onCreate = groups.find((g) => g.key === "on_create")!;
    expect(onCreate.source).toBe("inherited");
    expect(onCreate.commands).toEqual(["echo global"]);
  });
});
