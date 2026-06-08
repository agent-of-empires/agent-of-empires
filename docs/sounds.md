# Sound Effects

Agent of Empires plays sound effects when agent sessions change state (start, running, waiting, idle, error). The structured view also plays a browser-side chime when a pending approval lands.

## Quick Start

1. Install sounds:
   ```bash
   aoe sounds install
   ```
   This downloads CC0 (public domain) fantasy/RPG sounds to your config directory. Requires internet for the initial download.
2. Enable sounds: launch `aoe`, press `s` for Settings, select the Sound category, enable sounds.
3. Start an agent session and listen for the transition sounds.

## Available Sounds

`aoe sounds install` ships ~10 CC0 RPG sound effects (from the [80 CC0 RPG SFX](https://opengameart.org/content/80-cc0-rpg-sfx) pack by SubspaceAudio) into:

- Linux: `~/.config/agent-of-empires/sounds/`
- macOS: `~/.agent-of-empires/sounds/`

Defaults cover `start`, `running`, `waiting`, `idle`, and `error`, plus extra variety sounds. Add your own `.wav`/`.ogg` files to the same directory.

### Useful Commands

```bash
aoe sounds list          # check installed sounds
aoe sounds test start    # test a sound
```

## Sound Modes

- **Random** (default): picks a random sound from your sounds directory for each transition.
- **Specific**: always plays the same sound file, for one signature sound across all transitions.

## Configuration

Configure via the TUI (press `s`, select Sound), or edit TOML directly. Toggle the scope to "Profile" (top-right in Settings) to override per profile.

- Enabled: turn sounds on/off
- Mode: Random or Specific
- Per-transition overrides: set a specific sound for each state

**Global**: `~/.config/agent-of-empires/config.toml` (Linux) or `~/.agent-of-empires/config.toml` (macOS)

```toml
[sound]
enabled = true
mode = "random"
on_error = "error"          # specific sound for errors
on_approval = "approval"    # structured view only; browser-side chime
```

**Profile**: `~/.config/agent-of-empires/profiles/<profile>/config.toml`

```toml
[sound]
enabled = true
on_start = "spell"
on_running = "metal"
on_error = "error"
```

## Custom Sounds

Add `.wav` or `.ogg` files to `~/.config/agent-of-empires/sounds/`, then reference them by filename without extension:

```bash
cp ~/Downloads/wololo.wav ~/.config/agent-of-empires/sounds/
# Then in settings, set "On Start" to "wololo"
```

## Audio Playback

Status transition sounds play on the **server host** using platform-native players:

- **macOS**: `afplay`
- **Linux**: `aplay` (ALSA) or `paplay` (PulseAudio)

The `on_approval` sound is the exception: it plays in the **browser** where the dashboard is open, not on the host. Browsers enforce an autoplay policy, so the first approval after a fresh page load may stay silent until you interact with the structured view tab; the OS push notification still surfaces the approval.

If sounds don't play, ensure audio tools are installed:

```bash
# Debian/Ubuntu
sudo apt install alsa-utils pulseaudio-utils

# Arch Linux
sudo pacman -S alsa-utils pulseaudio
```

## Troubleshooting

**Sounds not playing?**
- **SSH session**: audio doesn't work over SSH; you need a local terminal with speakers/headphones.
- Check that sound files exist in `~/.config/agent-of-empires/sounds/`.
- Verify sounds are enabled in Settings.
- Test audio directly: `aplay ~/.config/agent-of-empires/sounds/start.wav` (Linux).
- Check logs: `AGENT_OF_EMPIRES_DEBUG=1 aoe`, then `aoe logs`.

**Custom sounds aren't listed?**
- Ensure files have a `.wav` or `.ogg` extension and are readable.
- Restart the TUI to refresh the sound list.

## License

Bundled sounds are CC0 1.0 Universal (Public Domain); no attribution required. Source: [OpenGameArt.org - 80 CC0 RPG SFX](https://opengameart.org/content/80-cc0-rpg-sfx) by SubspaceAudio.
