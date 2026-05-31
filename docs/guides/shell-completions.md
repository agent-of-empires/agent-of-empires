# Shell Completions

`aoe completion <shell>` prints a tab-completion script for your shell. It supports `bash`, `zsh`, `fish`, `powershell`, and `elvish`. The script is rendered from the binary's command tree at the moment you run the command, so it always matches the version of `aoe` that produced it.

There are two ways to wire it up. Pick one per shell.

## Recommended: eval on shell startup

Source the completions every time your shell starts. The script is regenerated from the current binary on each launch, so it never goes stale after an `aoe update`. The cost is a few milliseconds added to shell startup.

This is the pattern `gh`, `rustup`, and `kubectl` recommend.

**Bash** (add to `~/.bashrc`):

```bash
eval "$(aoe completion bash)"
```

**Zsh** (add to `~/.zshrc`, before any `compinit` call):

```zsh
eval "$(aoe completion zsh)"
```

**Fish** (add to `~/.config/fish/config.fish`):

```fish
aoe completion fish | source
```

**PowerShell** (add to your `$PROFILE`):

```powershell
aoe completion powershell | Out-String | Invoke-Expression
```

**Elvish** (add to `~/.config/elvish/rc.elv`):

```elvish
eval (aoe completion elvish | slurp)
```

## Alternative: static file

Write the script to a file your shell loads at startup. This avoids the per-launch cost, but the file is a snapshot: after an `aoe update` adds or renames a subcommand or flag, the file is stale until you regenerate it (see [Keeping static completions fresh](#keeping-static-completions-fresh)).

**Bash:**

```bash
aoe completion bash > ~/.local/share/bash-completion/completions/aoe
```

**Zsh** (ensure `~/.zfunc` is on your `fpath` in `~/.zshrc` before `compinit`):

```zsh
aoe completion zsh > ~/.zfunc/_aoe
```

**Fish:**

```fish
aoe completion fish > ~/.config/fish/completions/aoe.fish
```

**PowerShell** (write to a dedicated file, then dot-source it from your profile; redirecting straight into `$PROFILE` would overwrite the profile script itself):

```powershell
$dir = Split-Path -Parent $PROFILE.CurrentUserAllHosts
New-Item -ItemType Directory -Force -Path $dir | Out-Null
aoe completion powershell > "$dir\aoe.completion.ps1"
# Add this line to $PROFILE.CurrentUserAllHosts:
#   . "$PSScriptRoot\aoe.completion.ps1"
```

**Elvish:**

```elvish
aoe completion elvish > ~/.elvish/lib/aoe.elv
```

Restart your shell, or re-source the relevant file, after installing.

## Keeping static completions fresh

A static completion file does not update itself. Each time you run `aoe update`, regenerate the file so it reflects any new subcommands or flags:

```bash
aoe completion zsh > ~/.zfunc/_aoe   # adjust shell and path to match your install
```

`aoe update` prints a reminder about this after a successful update. If you would rather not think about it, use the eval-on-startup method above; it is always in sync with the installed binary.
