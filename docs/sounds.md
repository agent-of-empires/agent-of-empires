# Sound Effects

AoE can play a sound when a session changes state (start, running, waiting, idle, error), on the machine running aoe, in the web dashboard, or both. The structured view also plays a browser-side chime when a pending approval or question lands.

```bash
aoe sounds install       # download the CC0 pack into your config dir
aoe sounds list
aoe sounds test start
```

The install downloads about ten CC0 RPG effects from the [80 CC0 RPG SFX](https://opengameart.org/content/80-cc0-rpg-sfx) pack by SubspaceAudio into `~/.config/agent-of-empires/sounds/` (Linux) or `~/.agent-of-empires/sounds/` (macOS). Then enable sounds in the TUI settings (`s`, Sound category) or in `config.toml`:

```toml
[sound]
enabled = true
on_error = "error"        # unset transitions play a random sound
on_approval = "approval"  # structured view only; plays in the browser
server_enabled = true     # play status sounds on the machine running aoe
browser_enabled = true    # play status sounds in the web dashboard
```

Each transition plays its per-transition override when one is set, and a random sound from your sounds directory otherwise, so setting the same file on every transition gives you one signature sound. Add your own `.wav` or `.ogg` files to the sounds directory and reference them by filename without the extension. A profile's `[sound]` section overrides the global one.

## Playback

Status sounds play in two places, each with its own switch under the master `enabled`:

- **Server** (`server_enabled`): the machine running aoe, through `afplay` on macOS and `aplay` or `paplay` on Linux (`apt install alsa-utils pulseaudio-utils`). Only the TUI triggers these, and audio does not work over SSH.
- **Browser** (`browser_enabled`): any open dashboard tab plays the waiting, idle and error sounds for every session, terminal or structured. Use this when `aoe serve` runs on a remote box. Start and running stay server-only. A tab ignores statuses that were already there when it loaded, and a status has to hold for a moment (5s for waiting, 2s for idle and error) before it sounds, so a flapping session stays quiet. A waiting sound is skipped right after the approval chime for the same session.

The browser uses the settings of the profile `aoe serve` runs under, so that profile's `[sound]` overrides apply there. Sessions you have muted for push notifications (`notify_on_*` set to false) stay silent in the browser too. If you run the TUI and a dashboard tab together, or two dashboard tabs, each plays its own sound; turn off `server_enabled` or `browser_enabled`, or close the extra tab. Status comes from the dashboard's 3-second session refresh, and browsers slow timers in tabs hidden for more than five minutes, so a long-hidden tab can sound up to a minute late. Browsers also enforce an autoplay policy, so the first sound after a page load may stay silent until you interact with the tab; the push notification still fires when no dashboard is in the foreground.

`on_approval` plays only in the browser, and only for structured sessions.

If nothing plays, check that the files exist and are readable with a `.wav` or `.ogg` extension, that sounds are enabled in Settings, and that the player works directly (`aplay ~/.config/agent-of-empires/sounds/start.wav`). Restart the TUI to refresh the sound list, and check `aoe logs` with `AGENT_OF_EMPIRES_DEBUG=1`.

The bundled sounds are CC0 1.0 Universal (public domain); no attribution required.
