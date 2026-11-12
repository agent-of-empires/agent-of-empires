import { describe, expect, it } from "vitest";
import type { CreateFailure } from "../../lib/types";
import { failureView } from "./createFailureView";

const failure = (code: string, stage: CreateFailure["stage"] = null, message = "m"): CreateFailure => ({
  code,
  stage,
  message,
  reference: "r",
});

describe("failureView", () => {
  it.each<[string, CreateFailure, { sandboxEnabled?: boolean; runtimeDown?: boolean }, object]>([
    [
      "runtime down",
      failure("sandbox_unavailable", "starting_container"),
      {},
      {
        title: "The Sandbox couldn't start",
        primary: "retry_runtime",
        secondary: "launch_without_sandbox",
        row: "sandbox",
      },
    ],
    [
      "runtime still down after Retry",
      failure("sandbox_unavailable", "starting_container"),
      { runtimeDown: true },
      { primary: "launch_without_sandbox", secondary: "retry_runtime" },
    ],
    [
      "image pull",
      failure("sandbox_image_pull_failed", "starting_container"),
      {},
      { primary: "change_image", secondary: "launch_without_sandbox", row: "sandbox", cue: "image pull failed" },
    ],
    [
      "hook",
      failure("hook_failed", "running_hooks"),
      {},
      { title: "An on_create hook failed", primary: "try_again", secondary: null, row: null, showOutput: true },
    ],
    [
      "missing working directory",
      failure("working_dir_missing", "preparing"),
      {},
      { primary: "choose_project", secondary: null, row: "project" },
    ],
    [
      "unknown, in the container",
      failure("create_failed", "starting_container", "Failed to create session"),
      {},
      {
        title: "The Session couldn't be created",
        step: "It failed while starting the container.",
        detail: null,
        secondary: "launch_without_sandbox",
        row: "sandbox",
      },
    ],
    [
      "unknown, in the container, Sandbox already off",
      failure("create_failed", "starting_container"),
      { sandboxEnabled: false },
      { secondary: null, row: null },
    ],
    [
      "unknown, before the container",
      failure("create_failed", "preparing"),
      {},
      { primary: "try_again", secondary: null, row: null },
    ],
    [
      "missing ACP adapter",
      failure("acp_adapter_missing", null, "Install the adapter."),
      {},
      { title: "The Session couldn't be created", step: null, detail: "Install the adapter.", secondary: null },
    ],
    [
      "validation",
      failure("validation_failed", null, "Title is too long."),
      {},
      { primary: "try_again", secondary: null, row: null, detail: "Title is too long." },
    ],
  ])("%s", (_label, sent, ctx, expected) => {
    expect(failureView(sent, { sandboxEnabled: true, runtimeDown: false, ...ctx })).toMatchObject(expected);
  });
});
