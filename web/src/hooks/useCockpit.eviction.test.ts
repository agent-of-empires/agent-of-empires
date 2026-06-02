// @vitest-environment jsdom
//
// Tests for the persistState eviction-on-quota policy added for #1345.
//
// Contract (debated and locked):
// - `cockpit:draft:*` keys are NEVER touched, even when older than the
//   cockpit-state entries currently in storage. Drafts hold authoritative
//   client-side data; silent destruction would be data loss.
// - Eviction whitelist-filters by STORAGE_KEY_PREFIX
//   (`aoe:cockpit-state:v1:`), not blacklist-filters.
// - Corrupt entries (parse failure or missing savedAt) are evicted before
//   well-formed ones.
// - Retry depth is exactly 1: on a second failure, persistState gives up
//   silently. Cache is best-effort; replay on next mount reconstructs the
//   transcript.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { emptyCockpitState } from "../lib/cockpitTypes";
import { __test } from "./useCockpit";

const { persistState, evictOldestPersistedCockpitState, STORAGE_KEY_PREFIX } =
  __test;

const DRAFT_KEY_PREFIX = "cockpit:draft:";

function quotaError(): DOMException {
  return new DOMException("The quota has been exceeded.", "QuotaExceededError");
}

beforeEach(() => {
  window.localStorage.clear();
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe("cockpit cache eviction (#1345)", () => {
  it("evicts the oldest cockpit-state entry when the write hits quota and retries", () => {
    // Pre-populate: two existing cache entries, one old + one new.
    const now = Date.now();
    window.localStorage.setItem(
      `${STORAGE_KEY_PREFIX}sess-old`,
      JSON.stringify({ savedAt: now - 86_400_000, state: emptyCockpitState() }),
    );
    window.localStorage.setItem(
      `${STORAGE_KEY_PREFIX}sess-new`,
      JSON.stringify({ savedAt: now, state: emptyCockpitState() }),
    );

    // First setItem call (the persistState write) throws; subsequent calls
    // succeed so the retry after eviction lands.
    const setItem = vi
      .spyOn(Storage.prototype, "setItem")
      .mockImplementationOnce(() => {
        throw quotaError();
      });

    persistState("sess-current", emptyCockpitState());

    expect(setItem).toHaveBeenCalled();
    // Oldest entry was evicted.
    expect(window.localStorage.getItem(`${STORAGE_KEY_PREFIX}sess-old`)).toBeNull();
    // Newer entry survives.
    expect(
      window.localStorage.getItem(`${STORAGE_KEY_PREFIX}sess-new`),
    ).not.toBeNull();
    // Retried write landed.
    expect(
      window.localStorage.getItem(`${STORAGE_KEY_PREFIX}sess-current`),
    ).not.toBeNull();
  });

  it("never evicts cockpit:draft:* entries even when older than cockpit-state entries", () => {
    // Older draft AND older cockpit-state. Eviction must pick the
    // cockpit-state entry, not the draft.
    const now = Date.now();
    window.localStorage.setItem(`${DRAFT_KEY_PREFIX}sess-old`, "draft body");
    window.localStorage.setItem(
      `${STORAGE_KEY_PREFIX}sess-old`,
      JSON.stringify({ savedAt: now - 86_400_000, state: emptyCockpitState() }),
    );

    const removed = evictOldestPersistedCockpitState(
      `${STORAGE_KEY_PREFIX}sess-current`,
    );
    expect(removed).toBe(true);
    expect(
      window.localStorage.getItem(`${STORAGE_KEY_PREFIX}sess-old`),
    ).toBeNull();
    // Draft stays put.
    expect(window.localStorage.getItem(`${DRAFT_KEY_PREFIX}sess-old`)).toBe(
      "draft body",
    );
  });

  it("never evicts unrelated keys (e.g. theme cache, settings)", () => {
    const now = Date.now();
    window.localStorage.setItem("aoe-resolved-theme", "themedata");
    window.localStorage.setItem("aoe-web-settings", "{}");
    window.localStorage.setItem(
      `${STORAGE_KEY_PREFIX}sess-old`,
      JSON.stringify({ savedAt: now - 86_400_000, state: emptyCockpitState() }),
    );

    evictOldestPersistedCockpitState(`${STORAGE_KEY_PREFIX}sess-current`);

    expect(window.localStorage.getItem("aoe-resolved-theme")).toBe("themedata");
    expect(window.localStorage.getItem("aoe-web-settings")).toBe("{}");
    expect(
      window.localStorage.getItem(`${STORAGE_KEY_PREFIX}sess-old`),
    ).toBeNull();
  });

  it("prefers corrupt cockpit-state entries over older valid ones", () => {
    const now = Date.now();
    // Valid older entry that would normally win on savedAt.
    window.localStorage.setItem(
      `${STORAGE_KEY_PREFIX}sess-valid-old`,
      JSON.stringify({ savedAt: now - 86_400_000, state: emptyCockpitState() }),
    );
    // Corrupt entry with newer-looking key (not even valid JSON).
    window.localStorage.setItem(
      `${STORAGE_KEY_PREFIX}sess-corrupt`,
      "not valid json{{{",
    );

    evictOldestPersistedCockpitState(`${STORAGE_KEY_PREFIX}sess-current`);

    // Corrupt entry evicted first.
    expect(
      window.localStorage.getItem(`${STORAGE_KEY_PREFIX}sess-corrupt`),
    ).toBeNull();
    expect(
      window.localStorage.getItem(`${STORAGE_KEY_PREFIX}sess-valid-old`),
    ).not.toBeNull();
  });

  it("does not evict the current session's key (the one being written)", () => {
    const now = Date.now();
    // Only the current session has an entry. Eviction should find no
    // candidate and report false.
    window.localStorage.setItem(
      `${STORAGE_KEY_PREFIX}sess-current`,
      JSON.stringify({ savedAt: now - 86_400_000, state: emptyCockpitState() }),
    );

    const removed = evictOldestPersistedCockpitState(
      `${STORAGE_KEY_PREFIX}sess-current`,
    );
    expect(removed).toBe(false);
    expect(
      window.localStorage.getItem(`${STORAGE_KEY_PREFIX}sess-current`),
    ).not.toBeNull();
  });

  it("retry depth is exactly 1: second failure stays silent and does not loop", () => {
    const now = Date.now();
    window.localStorage.setItem(
      `${STORAGE_KEY_PREFIX}sess-old`,
      JSON.stringify({ savedAt: now - 86_400_000, state: emptyCockpitState() }),
    );

    // Every setItem call throws. The eviction's removeItem still works,
    // but the retried write fails again. persistState must return silently
    // without throwing or looping.
    const setItem = vi
      .spyOn(Storage.prototype, "setItem")
      .mockImplementation(() => {
        throw quotaError();
      });

    expect(() => persistState("sess-current", emptyCockpitState())).not.toThrow();
    // Original + retry = exactly two calls; no infinite loop.
    expect(setItem).toHaveBeenCalledTimes(2);
  });

  it("returns false silently when no cockpit-state candidate exists", () => {
    // Only drafts and unrelated keys; nothing to evict.
    window.localStorage.setItem(`${DRAFT_KEY_PREFIX}sess-a`, "draft");
    window.localStorage.setItem("aoe-resolved-theme", "{}");

    const removed = evictOldestPersistedCockpitState(
      `${STORAGE_KEY_PREFIX}sess-current`,
    );
    expect(removed).toBe(false);
    expect(window.localStorage.getItem(`${DRAFT_KEY_PREFIX}sess-a`)).toBe("draft");
  });
});

describe("persistState strips attachment-bearing queued rows (#1833)", () => {
  it("drops queued rows that carry attachments and writes no base64 bytes", () => {
    const state = {
      ...emptyCockpitState(),
      queuedPrompts: [
        { id: "q1", text: "plain text", queuedAt: "2026-01-01T00:00:00.000Z" },
        {
          id: "q2",
          text: "with image",
          queuedAt: "2026-01-01T00:00:01.000Z",
          attachments: [
            {
              kind: "image" as const,
              mimeType: "image/png",
              dataB64: "QUJDREVG",
              name: "shot.png",
            },
          ],
        },
      ],
    };

    persistState("sess-strip", state);

    const raw = window.localStorage.getItem(`${STORAGE_KEY_PREFIX}sess-strip`);
    expect(raw).not.toBeNull();
    // The base64 payload must never reach localStorage (quota invariant).
    expect(raw).not.toContain("QUJDREVG");

    const parsed = JSON.parse(raw!) as {
      state: { queuedPrompts: Array<{ id: string; attachments?: unknown }> };
    };
    // Only the text-only row survives persistence; the attachment row is
    // dropped entirely so reload never drains a degraded prompt.
    expect(parsed.state.queuedPrompts).toHaveLength(1);
    expect(parsed.state.queuedPrompts[0]?.id).toBe("q1");
  });

  it("leaves a fully text-only queue untouched (no needless clone)", () => {
    const state = {
      ...emptyCockpitState(),
      queuedPrompts: [
        { id: "q1", text: "a", queuedAt: "2026-01-01T00:00:00.000Z" },
      ],
    };
    persistState("sess-textonly", state);
    const raw = window.localStorage.getItem(`${STORAGE_KEY_PREFIX}sess-textonly`);
    const parsed = JSON.parse(raw!) as {
      state: { queuedPrompts: Array<{ id: string }> };
    };
    expect(parsed.state.queuedPrompts).toHaveLength(1);
    expect(parsed.state.queuedPrompts[0]?.id).toBe("q1");
  });
});
