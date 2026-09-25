// @vitest-environment jsdom
import { describe, expect, it } from "vitest";

import { isAllowedHref, isInternalHref, navigateInternalHref, toInternalPath } from "../pluginHref";
import { NAVIGATE_EVENT, OPEN_SESSION_EVENT } from "../sessionRoute";

describe("isAllowedHref", () => {
  it.each([
    ["https://x.test", true],
    ["http://x.test", true],
    ["/session/xyz", true],
    ["javascript:alert(1)", false],
    ["file:///etc/passwd", false],
    ["data:text/html,evil", false],
    ["//evil.com", false],
    ["//evil.com/path", false],
    ["/\\evil.com", false],
    ["/\t/evil.com", false],
    ["/\r/evil.com", false],
    ["/\n/evil.com", false],
    ["", false],
    [undefined, false],
  ])("isAllowedHref(%j) is %s", (url, expected) => {
    expect(isAllowedHref(url)).toBe(expected);
  });
});

describe("isInternalHref / toInternalPath", () => {
  const origin = window.location.origin;

  it("treats relative paths as internal", () => {
    expect(isInternalHref("/session/xyz")).toBe(true);
    expect(toInternalPath("/session/xyz")).toBe("/session/xyz");
  });

  it("treats same-origin absolute URLs as internal, reduced to path+search+hash", () => {
    expect(isInternalHref(`${origin}/session/xyz?a=1#h`)).toBe(true);
    expect(toInternalPath(`${origin}/session/xyz?a=1#h`)).toBe("/session/xyz?a=1#h");
  });

  it("normalizes dot-segments the same way a real browser navigation would", () => {
    // toInternalPath always resolves through URL (rather than returning a relative
    // href unchanged), so a modified click falling through to native navigation on
    // the same anchor can never land somewhere the SPA router wouldn't.
    expect(toInternalPath("/session/../session/xyz")).toBe("/session/xyz");
  });

  it("treats other-origin absolute URLs as external", () => {
    expect(isInternalHref("https://example.com/x")).toBe(false);
  });

  it("does not treat a scheme-relative URL as internal", () => {
    expect(isInternalHref("//evil.com")).toBe(false);
  });

  it("does not treat a backslash- or control-character-smuggled scheme-relative URL as internal", () => {
    // Browsers normalize `\` to `/` for special schemes and strip tab/CR/LF anywhere in the
    // string, so these resolve to a different origin despite starting with a single `/`.
    for (const href of ["/\\evil.com", "/\t/evil.com", "/\r/evil.com", "/\n/evil.com"]) {
      expect(isInternalHref(href)).toBe(false);
    }
  });
});

function waitForEvent<T>(name: string, fire: () => void): T {
  let detail: T | undefined;
  const handler = (e: Event) => {
    detail = (e as CustomEvent).detail as T;
  };
  window.addEventListener(name, handler);
  try {
    fire();
  } finally {
    window.removeEventListener(name, handler);
  }
  if (detail === undefined) throw new Error(`${name} did not fire`);
  return detail;
}

describe("navigateInternalHref", () => {
  it("dispatches a session-open request for a session-shaped path", () => {
    const detail = waitForEvent<{ sessionId: string }>(OPEN_SESSION_EVENT, () => navigateInternalHref("/session/xyz"));
    expect(detail.sessionId).toBe("xyz");
  });

  it("decodes a percent-encoded session id", () => {
    const detail = waitForEvent<{ sessionId: string }>(OPEN_SESSION_EVENT, () =>
      navigateInternalHref("/session/abc%20def"),
    );
    expect(detail.sessionId).toBe("abc def");
  });

  it("preserves a session link's query string and hash instead of dropping them", () => {
    const detail = waitForEvent<{ sessionId: string; path: string }>(OPEN_SESSION_EVENT, () =>
      navigateInternalHref("/session/xyz?tab=diff#L10"),
    );
    expect(detail.sessionId).toBe("xyz");
    expect(detail.path).toBe("/session/xyz?tab=diff#L10");
  });

  it("matches a mixed-case session path, like react-router's own case-insensitive route matching", () => {
    const detail = waitForEvent<{ sessionId: string }>(OPEN_SESSION_EVENT, () => navigateInternalHref("/Session/xyz"));
    expect(detail.sessionId).toBe("xyz");
  });

  it("matches a session path with a trailing slash", () => {
    const detail = waitForEvent<{ sessionId: string }>(OPEN_SESSION_EVENT, () => navigateInternalHref("/session/xyz/"));
    expect(detail.sessionId).toBe("xyz");
  });

  it("does not treat an extra path segment as a session link, since the route itself wouldn't match it", () => {
    const detail = waitForEvent<{ path: string }>(NAVIGATE_EVENT, () => navigateInternalHref("/session/xyz/extra"));
    expect(detail.path).toBe("/session/xyz/extra");
  });

  it("falls back to a plain route navigation for a non-session path", () => {
    const detail = waitForEvent<{ path: string }>(NAVIGATE_EVENT, () => navigateInternalHref("/settings"));
    expect(detail.path).toBe("/settings");
  });

  it("falls back to a plain route navigation when the session id is malformed percent-encoding", () => {
    const detail = waitForEvent<{ path: string }>(NAVIGATE_EVENT, () => navigateInternalHref("/session/%ZZ"));
    expect(detail.path).toBe("/session/%ZZ");
  });
});
