# Sound Effects

AoE can play a sound when a session changes state (start, running, waiting, idle, error). The structured view also plays a browser-side chime when a pending approval or question lands.

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
```

Each transition plays its per-transition override when one is set, and a random sound from your sounds directory otherwise, so setting the same file on every transition gives you one signature sound. Add your own `.wav` or `.ogg` files to the sounds directory and reference them by filename without the extension. A profile's `[sound]` section overrides the global one.

## Playback

Status sounds play on the **host** running the session, through `afplay` on macOS and `aplay` or `paplay` on Linux (`apt install alsa-utils pulseaudio-utils`). Audio does not work over SSH.

`on_approval` is the exception: it plays in the **browser** where the dashboard is open, since `aoe serve` often runs on a remote box. Browsers enforce an autoplay policy, so the first one after a page load may stay silent until you interact with the tab; the push notification still fires.

If nothing plays, check that the files exist and are readable with a `.wav` or `.ogg` extension, that sounds are enabled in Settings, and that the player works directly (`aplay ~/.config/agent-of-empires/sounds/start.wav`). Restart the TUI to refresh the sound list, and check `aoe logs` with `AGENT_OF_EMPIRES_DEBUG=1`.

The bundled sounds are CC0 1.0 Universal (public domain); no attribution required.
