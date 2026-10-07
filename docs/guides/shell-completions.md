# Shell Completions

`aoe completion <shell>` prints a tab-completion script for `bash`, `zsh`, `fish`, `powershell`, or `elvish`. The script is rendered from the binary's command tree when you run it, so it always matches that build of `aoe`.

## Recommended: eval on shell startup

Regenerating on each shell launch costs a few milliseconds and never goes stale after an `aoe update`. This is the pattern `gh`, `rustup`, and `kubectl` recommend.

```bash
eval "$(aoe completion bash)"                          # ~/.bashrc
eval "$(aoe completion zsh)"                           # ~/.zshrc, before compinit
aoe completion fish | source                           # ~/.config/fish/config.fish
aoe completion powershell | Out-String | Invoke-Expression   # $PROFILE
eval (aoe completion elvish | slurp)                   # ~/.config/elvish/rc.elv
```

## Alternative: a static file

Writing the script to a file your shell loads avoids the per-launch cost, but the file is a snapshot: regenerate it after an `aoe update` adds or renames a subcommand or flag. `aoe update` prints a reminder.

```bash
aoe completion bash > ~/.local/share/bash-completion/completions/aoe
aoe completion zsh  > ~/.zfunc/_aoe       # ~/.zfunc must be on fpath before compinit
aoe completion fish > ~/.config/fish/completions/aoe.fish
aoe completion elvish > ~/.elvish/lib/aoe.elv
```

For PowerShell, write a dedicated file and dot-source it from your profile; redirecting into `$PROFILE` would overwrite the profile itself:

```powershell
$dir = Split-Path -Parent $PROFILE.CurrentUserAllHosts
New-Item -ItemType Directory -Force -Path $dir | Out-Null
aoe completion powershell > "$dir\aoe.completion.ps1"
# then add to $PROFILE.CurrentUserAllHosts:
#   . "$PSScriptRoot\aoe.completion.ps1"
```

Restart your shell, or re-source the relevant file, after installing.
