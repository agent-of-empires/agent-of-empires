# Remote Machines

The TUI can list the sessions of `aoe serve` daemons on other machines beside
your local ones, preview and type into their terminal sessions, open their
structured sessions, and create new sessions on them.

## Registering a remote

On the remote machine, open Remote Access (`R`) and pick **Local network** or
**Internet (HTTPS)**. The exposed view shows a pairing code and the command to
run on this machine:

```sh
aoe remote add 192.168.1.5:8081          # prompts for the pairing code
aoe remote add aoe-mini.tailnet.ts.net --code K7F-3QX
aoe remote add https://mini.example.com --name mini --token <token>
AOE_REMOTE_PASSPHRASE=… aoe remote add https://mini.example.com --token <token>
```

The address is `host:port` for a LAN, link-local or tailnet (`100.64.0.0/10`)
address or a `.local` name, which is reached over plain HTTP and needs the
port the remote shows; any other hostname is reached over HTTPS. A full URL
works too.

Pairing needs no token. The code is single use and valid for 10 minutes; the
view replaces it as soon as it expires or a device redeems it, so the code on
screen always works. The entry stores a device-bound session instead of a
token, so rotating the remote's token or changing its passphrase does not
affect it, and it passes a passphrase wall. The view also lists paired
devices; select one and press `x` twice to revoke it. A revoked or expired
pairing shows as "not authorized" on the remote's header, and the TUI stops
asking that remote until its entry changes; run `aoe remote add` again with a
new code. Re-adding an address under a new name replaces the old entry.

Five wrong pairing codes or tokens from one IP within 15 minutes lock it out
for 15 minutes. The TUI waits out a lockout instead of retrying and shows the
time left on the remote's header. The exposed view on the remote lists locked
out IPs; press `u` there to let them try again.

Without `--name`, the entry is named after the remote's hostname (with a
numeric suffix if another remote has that name); adding an address that is
already registered updates that entry.

The token is the one the remote daemon prints at startup. A URL carrying
`?token=`, as `aoe serve --status` prints it, also works: the token moves to
`--token` and the stored URL drops the query. The exposed view shows that URL,
with a QR code for a phone, beside the pairing code on a wide terminal or
behind `w` on a narrow one. A daemon started with `--remote` also has a
passphrase wall: `--passphrase` (or `AOE_REMOTE_PASSPHRASE`, which keeps it
out of `ps`) is exchanged once for a device-bound login session. The
passphrase itself is never stored; when the session expires, add the remote
again.

`aoe remote add` refuses a URL it could never use: it must be `http` or
`https` with a host and no credentials, query or fragment, and a token or
login travels only over HTTPS or a loopback `http://` URL unless you confirm
plain HTTP when asked or pass `--insecure` (see below). It then reads the
remote's session list with those credentials and saves the entry only if that
works, so a wrong base path (HTTP 404) or a rejected token (HTTP 401 or 403)
fails the add instead of every later refresh.

`aoe remote list`, `aoe remote toggle <name> [--off]` and
`aoe remote remove <name>` manage the set. Entries live in `remotes.toml` in
the app directory, which must stay owner-only (`0600`). If that file cannot
be read, for example because its permissions were loosened, the home list
shows a `remotes.toml` row carrying the error instead of silently dropping
every remote.

## Plain HTTP on a trusted LAN

Without Tailscale Funnel or a TLS proxy, a daemon bound to a LAN address can
still be registered over plain HTTP. On the remote machine (debug builds
listen on port 8081, release builds on 8080):

```sh
./target/debug/aoe serve --daemon --host 0.0.0.0 --passphrase <passphrase>
./target/debug/aoe serve --status   # the URL line carries ?token=<token>
```

On your machine:

```sh
aoe remote add 192.168.1.20:8081 --token <token> --passphrase <passphrase>
```

On a terminal, `aoe remote add` asks before sending anything over plain HTTP;
`--insecure` gives that answer up front for scripts. The choice is stored on
that entry only, and `aoe remote list` marks it.
The token, passphrase and login session travel unencrypted, so anyone who can
see traffic on that network can read them and run commands as the remote
user. Use it only on a network you trust.

## Remote sessions in the home view

By default the list groups by machine: a `local` section, then one section per
enabled remote, each collapsible. Press `g` to pick another grouping; remote
sections then sit above the Archived and Trash shelf, which also gathers each
remote's archived and trashed rows. A remote that cannot be reached keeps its
header with the reason.

* **Terminal sessions** show their live output and info panel in the preview
  pane without resizing the remote pane. `Enter` or `Tab` starts live-send
  (see [Live Mode](live-mode.md)), which takes the pane's size until the exit
  chord. Keys typed before the daemon grants control are held and sent in
  order once it does; if it refuses, another viewer takes over, or the
  connection drops, live-send ends with a status message.
* **Structured sessions** open full screen against the remote daemon on
  `Enter`; `Ctrl+Q` returns to the home view.
* **Trashed rows** have to be restored on their own machine.

Right-clicking a remote row opens the same context menu a local row does: New
Session on that machine, Rename, Archive or Unarchive, Snooze, Mark read or
unread, and Delete. Snooze and the unread toggle appear under the same
conditions as on a local row, so the two menus match. The keyboard reaches all
of them through the same keys, and `x` stops a remote session while `e`
restarts it.

Three local entries have no remote counterpart, because each would answer from
this machine rather than the one that owns the row: Add project would file
another machine's path in this one's project list, and Fork and the view
switch both resolve agent configuration locally. The restart also carries no
overrides for the same reason; it relaunches with the settings that machine
already holds, rather than offering pickers filled from this one's agents.

A rename edits the title alone, since the machine that owns the row decides
whether its worktree directory follows. Delete reads the remote's own cleanup
defaults to decide whether it trashes first or deletes outright.
Right-clicking a machine header offers New Session there and the collapse
toggle.

A state change is sent and then waited on: the row keeps showing what the
remote last reported until the next poll confirms the change, so a failure
surfaces as a status line rather than a row that silently reverts.

## Creating a session on a remote

When a remote is configured, the new-session dialog (`n`) shows a Remote
picker above Profile. It starts on `Local`; `Left`/`Right` cycles through the
enabled remotes. Choosing a remote switches the dialog to that machine: its
profiles (starting on its default), its installed agents, a starting path at
its home directory, `Ctrl+P` browsing its filesystem, and the sandbox option
only when the remote reports a running container runtime. The session is
created by the remote daemon. A remote that is unreachable or still connecting
is listed with that status and cannot be submitted to; reopen the dialog once
it connects.

The dialog never approves repository hooks on another machine. If the repo's
hooks are not yet trusted there, the create is refused (HTTP 403); trust them
on that machine first, for example with `aoe add --trust-hooks`.

## A temporary remote from `AOE_DAEMON_URL`

```sh
AOE_DAEMON_URL=https://aoe.example.com AOE_DAEMON_TOKEN=… aoe
aoe --daemon-url https://aoe.example.com
```

The home view lists that daemon as one more remote, named after its host and
port, without saving it. A registered remote with the same URL is listed once,
under its registered name; a registered remote that merely shares the host
name keeps it, and the temporary one gets an ` (env)` suffix. The TUI still
starts as it does locally (it needs tmux and a valid profile), and local
sessions keep using the local daemon. `aoe acp` verbs and
`aoe serve --status` target the `AOE_DAEMON_URL` daemon instead.
