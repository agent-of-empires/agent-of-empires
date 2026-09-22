// @vitest-environment jsdom
//
// A bare <img src="/api/.../attachments/..."> can't carry the passphrase-mode
// device-binding header, so it renders broken. Pin that the Image part
// resolves through ArtifactImage's authenticated fetch instead (see artifactMedia.tsx).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, waitFor } from "@testing-library/react";
import {
  AssistantRuntimeProvider,
  ThreadPrimitive,
  useExternalStoreRuntime,
  type ThreadMessageLike,
} from "@assistant-ui/react";

import { UserMessage } from "./ThreadMessages";

const ATTACHMENT_URL = "/api/sessions/s1/acp/attachments/att1";

function Harness({ messages }: { messages: ThreadMessageLike[] }) {
  const runtime = useExternalStoreRuntime<ThreadMessageLike>({
    messages,
    convertMessage: (m) => m,
    onNew: async () => {},
  });
  return (
    <AssistantRuntimeProvider runtime={runtime}>
      <ThreadPrimitive.Messages components={{ UserMessage }} />
    </AssistantRuntimeProvider>
  );
}

beforeEach(() => {
  vi.stubGlobal("URL", {
    createObjectURL: vi.fn(() => "blob:mock-url"),
    revokeObjectURL: vi.fn(),
  });
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("UserMessage image part", () => {
  it("renders an attachment through authenticated fetch, never as a bare <img src>", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: true, blob: async () => new Blob(["x"]) }));
    const { container } = render(
      <Harness messages={[{ role: "user", content: [{ type: "image", image: ATTACHMENT_URL }] }]} />,
    );

    await waitFor(() => {
      const img = container.querySelector("img.acp-artifact-image");
      expect(img).not.toBeNull();
      expect(img?.getAttribute("src")).toBe("blob:mock-url");
    });
    expect(fetch).toHaveBeenCalledWith(ATTACHMENT_URL);
    expect(container.querySelector(`img[src="${ATTACHMENT_URL}"]`)).toBeNull();
  });
});
