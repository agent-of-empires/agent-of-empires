# Settings & Profiles

The settings view mirrors the TUI settings layout so muscle memory
carries over. It is grouped into tabs you can edit per profile or
globally. This page maps the tabs and covers the web-only pieces:
the profile picker, connected-device tracking, and step-up elevation.
For running the server, see the [Web Dashboard overview](../web-dashboard.md).

![The settings view with its tab groups and profile picker](../../assets/web/settings.png)

## Tab groups

Settings are organized into the same groups as the TUI:

- **Appearance**: Theme.
- **Sessions**: Session defaults and Structured view (the replay and watchdog
  tuning knobs; see [Structured view Setup](../../structured-view/setup.md)).
- **Environment**: Sandbox, Worktree, and Tmux.
- **Notifications**: Sound and Notifications (web push; see
  [Push notifications](../../push-notifications.md)).
- **Web Dashboard**: Terminal, Security, and Connected Devices.
- **System**: Updates and Logging.

Every config field that exists in the TUI settings is editable here too: the
panel is generated from the same settings schema as the TUI, so a field
declared once appears on both surfaces and they never drift. The only host-side
knob the dashboard does not surface is the host environment list, which stays
TUI/`config.toml`-only.

## Profiles

The profile picker switches the active profile and scopes which
settings you are editing. Each profile carries its own session defaults,
sandbox / worktree config, and overrides; global settings apply when a
profile does not override a field. Creating, renaming, deleting, or
changing the default profile is a gated action (see Step-up elevation
below).

## Connected devices

Under **Web Dashboard > Connected Devices**, the dashboard lists every
device that has authenticated, with a browser and OS label parsed from
the user agent and a relative "last seen" time. The list polls every
ten seconds and refreshes when you return to the tab, so a device that
just connected (or went quiet) shows up without a manual reload. This is
the surface for spotting an unexpected session and is also where token
rotation and rate-limit lockouts surface.

## Step-up elevation

When passphrase login is configured, day-to-day actions (sending
prompts, resolving approvals, switching mode) never re-prompt. Editing
persisted config is different: saving the global settings panel,
creating / deleting / renaming a profile, editing a profile, or changing
the default profile requires that your login session has been elevated
within the last 15 minutes via `POST /api/login/elevate`. The first such
action after a fresh page load surfaces an inline passphrase prompt;
subsequent edits inside the 15-minute window go through without
re-prompting.

This narrow gate covers the persisted-tamper attack (an attacker with a
stolen session planting a malicious Docker image, worktree template, or
profile) without putting friction on the conversation surface.
