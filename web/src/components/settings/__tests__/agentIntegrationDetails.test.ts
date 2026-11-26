import { describe, expect, it } from "vitest";
import { agentIntegrationDetails, displayAgentText } from "../agentIntegrationDetails";

describe("agentIntegrationDetails", () => {
  it("discloses launch, detection and fallback behavior safely", () => {
    expect(
      agentIntegrationDetails(
        {
          tool: "agent",
          provider_versions: ["^1.0"],
          replies: false,
          launch: {
            aliases: ["alias"],
            oneshot_flag: "-p",
            instruction_flag: "--prompt={}",
            model_flag: "--model",
            yolo: { kind: "env", key: "AUTO", value: "1" },
            container_env: { HOME_DIR: "/tmp\u001b[2J" },
            install_hint: "install \u202enow",
            host_only: true,
            send_keys_enter_delay_ms: 150,
            resume: { kind: "flag_pair", existing: "--continue", new_session: "--new" },
            capture: { backend: "omp", host: "pane_scoped", sandbox: "unsupported" },
          },
          detection: { mode: "native_only" },
          reply_fallback: {
            allow: [{ kind: "literal", value: "y" }],
            deny: [{ kind: "named", value: "Escape" }],
            allow_always: [{ kind: "named", value: "Enter" }],
          },
        },
        "/tmp/aoe-hooks",
      ),
    ).toEqual([
      "launch flags: oneshot -p; instruction --prompt={}; model --model; yolo AUTO=1",
      "enter delay: 150 ms",
      "sandbox environment: HOME_DIR=/tmp\\u{1b}[2J",
      "aliases: alias",
      "resume: flags --continue / --new",
      "capture: omp (host pane_scoped, sandbox unsupported)",
      "host only",
      "install: install \\u{202e}now",
      "detection: native_only",
      "sandbox reports: /tmp/aoe-hooks/<session id>",
      "provider versions: ^1.0",
      "native replies: no",
      'reply fallback: allow ["y"], deny [Escape], allow always [Enter]',
    ]);
    expect(displayAgentText("a\u202eb\u001b")).toBe("a\\u{202e}b\\u{1b}");
  });

  it("discloses alternative launch variants", () => {
    expect(
      agentIntegrationDetails(
        {
          tool: "agent",
          launch: { yolo: { kind: "flag", flag: "--yes" }, resume: { kind: "flag", flag: "--resume" } },
        },
        "/tmp/aoe-hooks",
      ),
    ).toEqual([
      "launch flags: yolo --yes",
      "resume: flag --resume",
      "sandbox reports: /tmp/aoe-hooks/<session id>",
      "native replies: no",
    ]);
    expect(
      agentIntegrationDetails(
        {
          tool: "agent",
          replies: true,
          launch: { yolo: { kind: "always" }, resume: { kind: "subcommand", command: "resume" } },
        },
        "/tmp/aoe-hooks",
      ),
    ).toEqual([
      "launch flags: yolo always",
      "resume: subcommand resume",
      "sandbox reports: /tmp/aoe-hooks/<session id>",
      "native replies: yes",
    ]);
  });

  it("reads the sandbox report directory from the host, not a literal", () => {
    expect(agentIntegrationDetails({ tool: "agent" }, "/srv/hooks")).toContain(
      "sandbox reports: /srv/hooks/<session id>",
    );
    // A host that reports no directory says nothing rather than a wrong path.
    expect(agentIntegrationDetails({ tool: "agent" }, "")).not.toContainEqual(
      expect.stringContaining("sandbox reports"),
    );
  });
});
