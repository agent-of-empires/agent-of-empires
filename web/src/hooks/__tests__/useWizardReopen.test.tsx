// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { useState } from "react";

import { REOPEN_REFUSED_MESSAGE, useWizardReopen } from "../app/useWizardReopen";
import type { WizardRestore } from "../../components/session-wizard/SessionWizard";
import { initialData } from "../../components/session-wizard/wizardReducer";
import { toastBus } from "../../lib/toastBus";

afterEach(() => {
  toastBus.handler = null;
});

const restore = (title: string): WizardRestore => ({
  data: { ...initialData, title },
  failure: { code: "create_failed", stage: "preparing", message: "boom", reference: null },
});

function setup(initiallyOpen: boolean) {
  const info = vi.fn();
  toastBus.handler = { info } as unknown as typeof toastBus.handler;
  const hook = renderHook(() => {
    const [open, setOpen] = useState(initiallyOpen);
    return { open, setOpen, ...useWizardReopen(open, setOpen) };
  });
  return { hook, info };
}

describe("useWizardReopen", () => {
  it("refuses while a wizard is open and reopens from the same handler once it closes", () => {
    const { hook, info } = setup(true);
    // The toast captured this handler before the wizard state changed.
    const reopen = hook.result.current.reopenFailure;

    let accepted: boolean | undefined;
    act(() => {
      accepted = reopen(restore("first"));
    });
    expect(accepted).toBe(false);
    expect(info).toHaveBeenCalledWith(REOPEN_REFUSED_MESSAGE);
    expect(hook.result.current.wizardRestore).toBeNull();

    act(() => hook.result.current.setOpen(false));
    act(() => {
      accepted = reopen(restore("first"));
    });
    expect(accepted).toBe(true);
    expect(hook.result.current.open).toBe(true);
    expect(hook.result.current.wizardRestore?.data.title).toBe("first");
    expect(info).toHaveBeenCalledOnce();

    act(() => hook.result.current.clearRestore());
    expect(hook.result.current.wizardRestore).toBeNull();
  });
});
