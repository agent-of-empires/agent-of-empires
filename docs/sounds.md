# Sound Effects

AoE can play a sound when a session changes state (start, running, waiting, idle, error), on the machine running aoe, in the web dashboard, or both. The structured view also plays a chime in the browser when a pending approval or question lands. The sounds are built into aoe, so there is nothing to install. Turn them on in Settings (Sound) in the TUI or the dashboard, or in `config.toml`:

```toml
[sound]
enabled = true            # off by default
server_enabled = true     # play status sounds on the machine running aoe
browser_enabled = true    # play status sounds in the web dashboard
server_volume = 1.0       # 0.1 to 1.5
browser_volume = 1.0      # 0.1 to 1.5
browser_scope = "all"     # all, other_sessions or hidden_tab
server_idle_enabled = true    # also server_{start,running,waiting,error}_enabled
browser_idle_enabled = true   # also browser_{approval,waiting,error}_enabled
```

A profile's `[sound]` section overrides the global one.

```bash
aoe sounds test           # play the idle sound on this machine
aoe sounds test error     # or start, running, waiting, approval
```

## Playback

Status sounds play in two places, each with its own switch under the master `enabled`:

- **Server** (`server_enabled`): the aoe daemon plays them on its own machine, through `afplay` on macOS and `aplay` or `paplay` on Linux (`apt install alsa-utils pulseaudio-utils`). They play whether you use the TUI, the dashboard, or neither, and use the `[sound]` settings of each session's profile. Audio does not work over SSH.
- **Browser** (`browser_enabled`): any open dashboard tab plays the waiting, idle and error sounds for every session, terminal or structured. Use this when `aoe serve` runs on a remote box. Start and running stay server-only. A tab ignores statuses that were already there when it loaded.

A waiting, idle or error status has to hold for a moment (5s for waiting, 2s for idle and error) before it sounds, on both targets, so a flapping session stays quiet. Sessions that are archived, snoozed, ignored or trashed stay silent, and so do sessions you have muted for push notifications (`notify_on_*` set to false). Sessions that reach the same status together play one sound. A waiting sound in the browser is skipped right after the approval chime for the same session.

Each event has an on/off switch per target (under Advanced in settings), all on by default. `browser_scope` limits which sessions sound in the browser: `other_sessions` stays silent for the session open in that tab, and `hidden_tab` sounds only while the tab is hidden. Both apply to the approval chime too. The browser uses the settings of the profile `aoe serve` runs under.

If the dashboard runs on the same machine as aoe, both targets play each sound; turn off `server_enabled` or `browser_enabled`. The server pushes sound cues to the dashboard over a WebSocket, so a hidden tab sounds on time even though browsers slow its timers. Browsers only play sound once you have clicked or typed in the page, so after a reload the dashboard asks for a click if a sound comes first; the push notification still fires when no dashboard is in the foreground.

If nothing plays on the server, run `aoe sounds test` there and check `aoe logs` with `AGENT_OF_EMPIRES_DEBUG=1`.

The sounds are from the CC0 [80 CC0 RPG SFX](https://opengameart.org/content/80-cc0-rpg-sfx) pack by SubspaceAudio (public domain; no attribution required).
