# HOL Guard

HOL Guard can be used with coding agents launched by Agent of Empires.

## Host sessions

Install and initialize HOL Guard before creating the AoE session:

```bash
pipx install hol-guard
hol-guard init
hol-guard status
```

For an explicit Codex setup:

```bash
hol-guard install codex
hol-guard doctor codex
```

Then start Codex through AoE as usual:

```bash
aoe add --cmd codex .
```

HOL Guard is configured through the agent's own integration. AoE continues to manage the session and does not require a Guard-specific AoE plugin for this host-session setup.

## Sandboxed sessions

AoE sandbox sessions run inside an isolated container and use a private per-session agent store. A HOL Guard installation on the host is not automatically available inside that container.

For sandbox use, build a custom AoE sandbox image that includes Python, pipx, and HOL Guard, then configure Guard for the agent inside the sandbox environment. See [Container Sandbox](sandbox.md) for custom images and per-session agent stores.

AoE repository lifecycle hooks are separate from the agent integration used by HOL Guard.
