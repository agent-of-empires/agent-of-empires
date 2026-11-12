import { describe, expect, it } from "vitest";
import type { SandboxImageStatus } from "../../lib/api";
import { pullLatestChecked, pullLatestOffer } from "./pullLatestImage";

const status = (over: Record<string, unknown>) =>
  ({
    image: "sb:1",
    state: "available",
    remote_digest: "sha256:b",
    dismissed: false,
    pull_blocked: null,
    pull_command: "docker pull sb:1",
    ...over,
  }) as unknown as SandboxImageStatus;
const ON = { sandbox: true, structured: true };

describe("pullLatestOffer", () => {
  it.each<[string, Parameters<typeof pullLatestOffer>[0], typeof ON, ReturnType<typeof pullLatestOffer>]>([
    ["an update", status({}), ON, { checkedByDefault: true, command: null }],
    ["a dismissed update", status({ dismissed: true }), ON, { checkedByDefault: false, command: null }],
    [
      "pulls turned off",
      status({ pull_blocked: "disabled" }),
      ON,
      { checkedByDefault: true, command: "docker pull sb:1" },
    ],
    ["CityHall", status({ pull_blocked: "cityhall" }), ON, { checkedByDefault: true, command: "docker pull sb:1" }],
    ["Terminal view", status({}), { sandbox: true, structured: false }, null],
    ["the Sandbox off", status({}), { sandbox: false, structured: true }, null],
    ["the latest image", status({ state: "current" }), ON, null],
    ["an image not pulled yet", status({ state: "unknown", reason: "not_pulled" }), ON, null],
    ["a failed check", status({ state: "check_failed", error: "timeout" }), ON, null],
    ["an image that isn't configured", { error: "unknown_image" }, ON, null],
    ["no answer yet", undefined, ON, null],
    ["offline", null, ON, null],
  ])("%s", (_name, reply, where, offer) => {
    expect(pullLatestOffer(reply, where)).toEqual(offer);
  });
});

describe("pullLatestChecked", () => {
  const offer = { checkedByDefault: true, command: null };
  it.each<[string, Parameters<typeof pullLatestChecked>[0], boolean | null, boolean]>([
    ["the default", offer, null, true],
    ["the dismissed default", { ...offer, checkedByDefault: false }, null, false],
    ["unchecked by the user", offer, false, false],
    ["checked by the user after a dismiss", { ...offer, checkedByDefault: false }, true, true],
    ["gated, whatever the user chose", { ...offer, command: "docker pull sb:1" }, true, false],
    ["not offered, whatever the user chose", null, true, false],
  ])("%s", (_name, o, choice, checked) => {
    expect(pullLatestChecked(o, choice)).toBe(checked);
  });
});
