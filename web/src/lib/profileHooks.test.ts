import { expect, it } from "vitest";
import { buildEffectiveHooks } from "./profileHooks";

it("returns the three lifecycle events in TUI order with parity labels, none when unset", () => {
  const groups = buildEffectiveHooks({}, {});
  expect(groups.map((g) => [g.key, g.label, g.source, g.commands])).toEqual([
    ["on_create", "On Create", "none", []],
    ["on_launch", "On Launch", "none", []],
    ["on_destroy", "On Destroy", "none", []],
  ]);
  expect(buildEffectiveHooks(undefined, undefined).map((g) => g.source)).toEqual(["none", "none", "none"]);
});

it("resolves each event's source from the profile override and global hooks", () => {
  const global = { on_create: ["echo global"] };
  const cases: [string, Record<string, unknown>, string, string[]][] = [
    ["non-empty override", { on_create: ["echo hi"] }, "override", ["echo hi"]],
    ["empty override disables global", { on_create: [] }, "override-empty", []],
    ["no override inherits", {}, "inherited", ["echo global"]],
    ["malformed override is absent", { on_create: "echo hi" }, "inherited", ["echo global"]],
  ];
  for (const [name, profile, source, commands] of cases) {
    const onCreate = buildEffectiveHooks(profile as never, global).find((g) => g.key === "on_create")!;
    expect({ source: onCreate.source, commands: onCreate.commands }, name).toEqual({ source, commands });
  }
});
