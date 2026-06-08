// @vitest-environment jsdom
//
// Contract guard for the structured view composer's "Escape does not cancel an
// active turn" behaviour. The structured view Composer passes
// `cancelOnEscape={false}` to assistant-ui's ComposerPrimitive.Input so
// that pressing Escape inside the textarea does not run
// runtime.cancelRun and abort the in-flight turn. Claude Code CLI also
// binds Escape to cancel in its TUI, so a stray press is easy to make.
//
// This test renders the real ComposerPrimitive.Input inside a minimal
// AssistantRuntimeProvider where the external store's `onCancel` is a
// vi.fn(). It documents the contract via two cases:
//   1. With cancelOnEscape={false}, Escape does NOT invoke onCancel.
//   2. Without that prop (the SDK default), Escape DOES invoke onCancel.
// The second case is the control that makes the first case meaningful:
// if assistant-ui ever changes the prop semantics, both will move and
// the regression surfaces here.

import { describe, expect, it, vi, afterEach } from "vitest";
import { cleanup, render } from "@testing-library/react";
import {
  AssistantRuntimeProvider,
  ComposerPrimitive,
  useExternalStoreRuntime,
  type ThreadMessageLike,
} from "@assistant-ui/react";

afterEach(() => {
  cleanup();
});

function Harness({
  onCancel,
  cancelOnEscape,
}: {
  onCancel: () => void | Promise<void>;
  cancelOnEscape?: boolean;
}) {
  const runtime = useExternalStoreRuntime<ThreadMessageLike>({
    messages: [],
    isRunning: true,
    convertMessage: (m) => m,
    onNew: async () => {},
    onCancel,
  });
  return (
    <AssistantRuntimeProvider runtime={runtime}>
      <ComposerPrimitive.Root>
        <ComposerPrimitive.Input
          data-testid="composer-input"
          cancelOnEscape={cancelOnEscape}
        />
      </ComposerPrimitive.Root>
    </AssistantRuntimeProvider>
  );
}

function pressEscape(target: HTMLElement) {
  // assistant-ui registers its Escape listener on document via
  // @radix-ui/react-use-escape-keydown, which uses a capture-phase
  // document listener. Dispatch from the textarea so the listener's
  // `textareaRef.current.contains(e.target)` guard passes.
  target.dispatchEvent(
    new KeyboardEvent("keydown", { key: "Escape", bubbles: true }),
  );
}

describe("ComposerPrimitive.Input + cancelOnEscape contract", () => {
  it("does not invoke onCancel when cancelOnEscape={false}", async () => {
    const onCancel = vi.fn();
    const { getByTestId } = render(
      <Harness onCancel={onCancel} cancelOnEscape={false} />,
    );
    const input = getByTestId("composer-input");
    input.focus();
    pressEscape(input);
    // Give microtasks a tick in case the SDK queues the cancel.
    await Promise.resolve();
    expect(onCancel).not.toHaveBeenCalled();
  });

  it("invokes onCancel when cancelOnEscape is left at its default", async () => {
    const onCancel = vi.fn();
    const { getByTestId } = render(<Harness onCancel={onCancel} />);
    const input = getByTestId("composer-input");
    input.focus();
    pressEscape(input);
    await Promise.resolve();
    expect(onCancel).toHaveBeenCalledTimes(1);
  });
});
