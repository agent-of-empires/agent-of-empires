import { describe, expect, it } from "vitest";
import { pickWorkerStoppedVariant } from "./workerStoppedBanner";

describe("pickWorkerStoppedVariant", () => {
  it("returns 'none' when the worker is not stopped", () => {
    expect(
      pickWorkerStoppedVariant({
        workerStopped: false,
        startupError: null,
        archivedAt: null,
        snoozedUntil: null,
      }),
    ).toBe("none");
  });

  it("returns 'none' when a startup error is in flight (its own banner wins)", () => {
    expect(
      pickWorkerStoppedVariant({
        workerStopped: true,
        startupError: "missing API key",
        archivedAt: null,
        snoozedUntil: null,
      }),
    ).toBe("none");
  });

  it("returns 'archived' when the session is archived", () => {
    // Regression: an archived structured view session must not show the
    // generic `aoe acp stop` reconnect banner. Reconnecting from
    // the structured view would race the reconciler, which skips
    // archived sessions, and the user would see the spawn flicker
    // and then disappear. See #1581.
    expect(
      pickWorkerStoppedVariant({
        workerStopped: true,
        startupError: null,
        archivedAt: "2026-01-01T00:00:00Z",
        snoozedUntil: null,
      }),
    ).toBe("archived");
  });

  it("returns 'snoozed' when the session is snoozed", () => {
    // Regression: same problem as archived, but with the snooze
    // wake-up path. The snoozed banner surfaces the wake time and
    // points at Unsnooze in the sidebar context menu instead. See
    // #1581.
    expect(
      pickWorkerStoppedVariant({
        workerStopped: true,
        startupError: null,
        archivedAt: null,
        snoozedUntil: "2026-01-01T00:00:00Z",
      }),
    ).toBe("snoozed");
  });

  it("prefers 'archived' over 'snoozed' (defensive multi-flag fallback)", () => {
    // The server's XOR rules prevent both flags from being set on
    // the same session at once, but workspace-level aggregators can
    // surface both via different sessions. Archive is the stronger
    // signal (no automatic wake), so the variant prefers it.
    expect(
      pickWorkerStoppedVariant({
        workerStopped: true,
        startupError: null,
        archivedAt: "2026-01-01T00:00:00Z",
        snoozedUntil: "2026-02-01T00:00:00Z",
      }),
    ).toBe("archived");
  });

  it("returns 'generic' for the `aoe acp stop` / external-teardown case", () => {
    expect(
      pickWorkerStoppedVariant({
        workerStopped: true,
        startupError: null,
        archivedAt: null,
        snoozedUntil: null,
      }),
    ).toBe("generic");
  });

  it("returns 'generic' when only snoozedUntil is null but archivedAt is null too (branch combo)", () => {
    // Branch coverage: the snoozedUntil-falsy leg of the third `if`
    // (after archivedAt also falsy). The "generic" test above hits
    // this path with all-null; this case uses an empty string on
    // startupError to confirm that branch's falsy side as well.
    expect(
      pickWorkerStoppedVariant({
        workerStopped: true,
        startupError: "",
        archivedAt: null,
        snoozedUntil: null,
      }),
    ).toBe("generic");
  });

  it("returns 'archived' when startupError is empty string (not truthy)", () => {
    // Branch coverage: a non-null but falsy startupError must fall
    // through to the archived check rather than short-circuiting on
    // the second `if`.
    expect(
      pickWorkerStoppedVariant({
        workerStopped: true,
        startupError: "",
        archivedAt: "2026-01-01T00:00:00Z",
        snoozedUntil: null,
      }),
    ).toBe("archived");
  });

  it("returns 'snoozed' when archivedAt is null and snoozedUntil is non-null", () => {
    // Mirror of the 'archived' branch with the third `if` taking the
    // falsy side and the fourth `if` taking truthy. Ensures the
    // archivedAt-null leg is exercised independently of the
    // archived-wins-over-snoozed defensive case.
    expect(
      pickWorkerStoppedVariant({
        workerStopped: true,
        startupError: null,
        archivedAt: null,
        snoozedUntil: "2099-01-01T00:00:00Z",
      }),
    ).toBe("snoozed");
  });
});
