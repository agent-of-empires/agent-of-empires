# Web dashboard & cockpit screenshots

The PNGs in `docs/assets/web/` and `docs/assets/cockpit/` are generated,
not hand-edited. They are captured by:

```bash
scripts/dev/capture-web-screenshots.sh
```

which drives a seeded `aoe serve` (and a scripted fake ACP agent for the
cockpit shots) through the live Playwright harness
(`web/tests/capture/screenshots.spec.ts`, run via
`web/playwright.capture.config.ts`).

## Maintenance contract

- **Generated, never hand-edited.** To change an image, change the UI or
  the capture spec and re-run the script.
- **Deterministic inputs.** Fixed viewports (1440x900 desktop, 390x844
  mobile), reduced motion, and seeded data only. No live accounts.
- **Hero shots only.** We commit a small set of representative images,
  not one per page. The capture spec can produce more on demand; only
  the committed set ships in the docs.
- **Refresh when the surface changes.** A screenshot that no longer
  matches the UI is a docs bug. Re-run the script in the same PR that
  changes the surface, or open a follow-up.
- **Not wired into CI.** Pixel-diffing a fast-moving beta UI in CI is
  noisy (font and anti-aliasing drift across runners), so capture is a
  manual developer step, not a gate.

## Current images

- `web/dashboard.png`: dashboard home with sidebar and session summary.
- `web/terminal.png`: a session's agent terminal in the desktop split.
- `web/diff.png`: the web diff view with a changed file open.
- `web/settings.png`: the settings view and its tab groups.
- `cockpit/overview.png`: a cockpit turn with plan, tool-call cards, agent text.
- `cockpit/interface.png`: the cockpit composer and cards on a phone viewport.
- `cockpit/approval.png`: a destructive-action approval card.
