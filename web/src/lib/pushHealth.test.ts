import { describe, expect, it } from "vitest";

import {
  classifyPushHealth,
  hasFreshGoneFailure,
  type PushHealth,
  type PushHealthInput,
  type ServerSubscriptionStatus,
} from "./pushHealth";

const T1 = "2026-09-01T10:00:00Z";
const T2 = "2026-09-01T11:00:00Z";

const server = (over: Partial<ServerSubscriptionStatus> = {}): ServerSubscriptionStatus => ({
  registered: true,
  owned: true,
  last_success_at: null,
  last_failure_at: null,
  last_failure: null,
  ...over,
});

const base: PushHealthInput = {
  wanted: true,
  permission: "granted",
  subscribed: true,
  keyMatches: true,
  server: server(),
};

describe("classifyPushHealth", () => {
  it.each<[string, Partial<PushHealthInput>, PushHealth]>([
    ["everything lines up", {}, "healthy"],
    ["the status endpoint gave nothing", { server: null, keyMatches: null }, "healthy"],
    ["the user never asked", { wanted: false, subscribed: false }, "not-wanted"],
    ["permission was blocked", { permission: "denied", subscribed: false }, "permission-denied"],
    ["the browser dropped the subscription", { subscribed: false }, "revoked"],
    ["permission reset and the subscription went with it", { permission: "default", subscribed: false }, "revoked"],
    ["the subscription uses another server key", { keyMatches: false }, "key-mismatch"],
    ["the server forgot the endpoint", { server: server({ registered: false }) }, "server-forgot"],
    ["an old token owns the endpoint", { server: server({ owned: false }) }, "server-forgot"],
    [
      "the push service said the key mismatched",
      { server: server({ last_failure: "key-mismatch", last_failure_at: T1 }) },
      "key-mismatch",
    ],
    [
      "the push service said gone and the server dropped it",
      { server: server({ registered: false, last_failure: "gone", last_failure_at: T1 }) },
      "delivery-failed",
    ],
    [
      "a rejection newer than the last success",
      { server: server({ last_failure: "rejected", last_failure_at: T2, last_success_at: T1 }) },
      "delivery-failed",
    ],
    [
      "a rejection older than the last success",
      { server: server({ last_failure: "rejected", last_failure_at: T1, last_success_at: T2 }) },
      "healthy",
    ],
    [
      "only a transient failure",
      { server: server({ last_failure: "failed", last_failure_at: T2, last_success_at: T1 }) },
      "healthy",
    ],
  ])("%s", (_label, over, expected) => {
    expect(classifyPushHealth({ ...base, ...over })).toBe(expected);
  });
});

describe("hasFreshGoneFailure", () => {
  it.each([
    ["a current gone failure", server({ last_failure: "gone", last_failure_at: T2 }), true],
    [
      "a gone failure superseded by success",
      server({ last_failure: "gone", last_failure_at: T1, last_success_at: T2 }),
      false,
    ],
    ["a transient failure", server({ last_failure: "failed", last_failure_at: T2 }), false],
    ["missing status", null, false],
  ])("recognizes %s", (_label, status, expected) => {
    expect(hasFreshGoneFailure(status)).toBe(expected);
  });
});
