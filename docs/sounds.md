# Sound Effects

Agent of Empires can play sound effects when agent sessions change state, providing audio feedback for transitions like starting, running, waiting, idle, and error states. The structured view also plays a browser-side chime when a pending approval lands.

## Features

- 🔊 State transition sounds (start, running, waiting, idle, error)
- 🛡️ Structured view approval chime, played in the browser
- 🎵 Multiple installation options (bundled, AoE II extraction, custom)
- 🎨 Fully customizable - use any .wav/.ogg files
- ⚙️ Configurable via Settings TUI
- 🎯 Per-transition sound overrides
- 🎲 Random or specific sound modes

## Quick Start

1. **Install sounds**:
   ```bash
   aoe sounds install
   ```
   This downloads and installs CC0 fantasy/RPG sounds from GitHub to your config directory.

2. **Enable sounds in settings**:
   - Launch `aoe` (TUI mode)
   - Press `s` to open Settings
   - Navigate to the Sound category
   - Enable sounds

3. **Test it**: Start an agent session and listen for the transition sounds!

## Available Sounds

Agent of Empires can download 10 CC0 (public domain) fantasy/RPG sound effects from GitHub:

### Default State Transition Sounds
- `start.wav` - Spell fire sound (session starting)
- `running.wav` - Blade sound (agent actively working)
- `waiting.wav` - Misc sound (agent waiting for input)
- `idle.wav` - Book sound (agent idle)
- `error.wav` - Roar sound (error occurred)

### Additional Variety Sounds
- `spell.wav` - Alternative spell/magic effect
- `coins.wav` - Coin/reward sound
- `metal.wav` - Metal impact sound
- `chain.wav` - Chain/lock sound
- `gem.wav` - Gem/crystal sound

All sounds are from the [80 CC0 RPG SFX](https://opengameart.org/content/80-cc0-rpg-sfx) pack by SubspaceAudio.

## Installation

### Install Sounds from GitHub

```bash
aoe sounds install
```

This downloads and installs 10 CC0 (public domain) fantasy/RPG sounds from the GitHub repository to:
- Linux: `~/.config/agent-of-empires/sounds/`
- macOS: `~/.agent-of-empires/sounds/`

**Note:** Requires an internet connection for the initial download. Sounds are downloaded from:
`https://github.com/agent-of-empires/agent-of-empires/tree/main/bundled_sounds`

### Useful Commands

**Check installed sounds:**
```bash
aoe sounds list
```

**Test a sound:**
```bash
aoe sounds test start
```

## Sound Modes

### Random Mode (default)
Picks a random sound from your sounds directory for each transition.

### Specific Mode
Always plays the same sound file. Useful if you want one signature sound for all transitions.

## Configuration

### Global Settings
Configure sounds for all profiles:
1. Launch `aoe` TUI
2. Press `s` for Settings
3. Select "Sound" category
4. Configure:
   - **Enabled**: Turn sounds on/off
   - **Mode**: Random or Specific
   - **Per-transition overrides**: Set specific sounds for each state

### Profile Settings
Override sound settings per profile:
1. In Settings, toggle to "Profile" scope (top-right)
2. Configure sound overrides for this profile only

### TOML Configuration

You can also edit configuration files directly:

**Global**: `~/.config/agent-of-empires/config.toml` (Linux) or `~/.agent-of-empires/config.toml` (macOS)

```toml
[sound]
enabled = true
mode = "random"
on_error = "error"          # Use specific sound for errors
on_approval = "approval"    # Structured view only; browser-side chime
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

Add your own sounds to `~/.config/agent-of-empires/sounds/`:

1. **Supported formats**: `.wav`, `.ogg`
2. **File naming**: Use descriptive names (e.g., `wololo.wav`, `rogan.ogg`)
3. **Reference in config**: Use the filename without extension

Example:
```bash
# Linux
cp ~/Downloads/wololo.wav ~/.config/agent-of-empires/sounds/

# Then in settings, set "On Start" to "wololo"
```

## Audio Playback

Status transition sounds (start, running, waiting, idle, error) play on the **server host** using platform-native audio players:
- **macOS**: `afplay`
- **Linux**: `aplay` (ALSA) or `paplay` (PulseAudio)

The structured view `on_approval` sound is the exception: it plays in the **browser** where the dashboard is open, not on the host. Approvals are user-facing and the dashboard is often on a different machine from `aoe serve`. Browsers enforce an autoplay policy, so the first approval after a fresh page load may stay silent until the user interacts with the structured view tab; the OS push notification still surfaces the approval in that case.

If sounds don't play, ensure you have audio tools installed:
```bash
# Debian/Ubuntu
sudo apt install alsa-utils pulseaudio-utils

# Arch Linux
sudo pacman -S alsa-utils pulseaudio
```

## Troubleshooting

**Sounds not playing?**
- **SSH Session**: Audio doesn't work over SSH - you need a local terminal with speakers/headphones
- Check that sound files exist in `~/.config/agent-of-empires/sounds/`
- Verify sounds are enabled in Settings
- Test audio with: `aplay ~/.config/agent-of-empires/sounds/start.wav` (Linux)
- Check logs: `AGENT_OF_EMPIRES_DEBUG=1 aoe` then `aoe logs` to view the resulting `debug.log`

**Want Age of Empires II sounds?**
If you own AoE II, manually copy the taunt files to your sounds directory.

**Custom sounds aren't listed?**
- Ensure files have `.wav` or `.ogg` extension
- Check file permissions are readable
- Restart the TUI to refresh the sound list

## License

Bundled sounds are CC0 1.0 Universal (Public Domain) - no attribution required. You are free to use, modify, and distribute them for any purpose, including commercial use.

Source: [OpenGameArt.org - 80 CC0 RPG SFX](https://opengameart.org/content/80-cc0-rpg-sfx) by SubspaceAudio
