# zlorb-ctl CLI contract

This document is the stable surface of `zlorb-ctl`: the commands, flags,
exit codes, stream rules, and the two JSON schemas (`status --json` and
`health --json`). Agents and scripts may rely on everything stated here.

Stability rule: a breaking change to anything in this document — removing
or renaming a command or flag, changing a meaning, an exit code, or a JSON
field — requires a version bump of `zlorb-ctl` (the version printed by
`zlorb-ctl --version`). Additive changes (a new command, a new flag, a new
JSON field) are not breaking. The human-readable text output of `list`,
`status`, and `health` is not part of the contract; its wording may change.
The JSON documents are the machine-readable form.

## Invocation

```bash
zlorb-ctl <COMMAND> [flags]
zlorb-ctl --help       # list commands
zlorb-ctl --version    # print the zlorb-ctl version
zlorb-ctl <COMMAND> --help
```

No command prompts, and no command reads stdin. Every command runs fully
unattended. Colour codes are emitted only when the stream they go to is a
terminal, so piped output carries no escape codes.

Config directory resolution is shared by `zlorb-ctl` and the daemon, in
this order:

1. `$ZLORB_CONFIG_DIR`, when it is set to a non-blank absolute path
   (the systemd unit written by `setup` pins it explicitly),
2. `$XDG_CONFIG_HOME/zlorb`, when `XDG_CONFIG_HOME` is set to a non-blank
   absolute path,
3. `~/.config/zlorb`.

A relative or blank value in either variable is ignored, and resolution
falls through to the next step.

Commands that talk to the daemon go through `systemctl`: the `--user`
scope for a normal user, the system scope when run as root. Run a command
at the same scope `setup` ran at, or it reads a different unit and config
directory. The repository config format itself (`repositories.toml`,
`service-config.toml`) is documented in the README's Configuration
section.

## Commands

| Command | Effect | Idempotent |
| --- | --- | --- |
| `add` | Track the git repository containing the working directory | Yes, see below |
| `list` | Print the tracked repositories' paths to stdout | Read-only |
| `status [--json]` | Report daemon and per-repository state | Read-only |
| `health [--json]` | Check daemon and web UI are actually responding | Read-only |
| `remove --repo-name <name>` | Stop tracking the repository with that name | Yes |
| `start` | Start the daemon | Yes |
| `stop` | Stop the daemon | Yes |
| `restart` | Restart the daemon | No — always acts |
| `setup [--print-only]` | Create the config and the systemd unit, then enable and start the daemon | Yes |
| `watch` | Follow the daemon's journal (`journalctl -f`) until it ends | Read-only |
| `web` | Run the web UI in the foreground on port 3000 | No — serves until stopped |

### add

Run from anywhere inside a git repository (subdirectories work). Records
the repository root and the branch currently checked out. The remote and
tracked branch are chosen in this order:

1. When the checked-out branch has an upstream configured
   (`branch.<branch>.remote` / `branch.<branch>.merge`), that remote and
   that merge branch are recorded — even when `origin` also exists. An
   upstream naming a remote that does not exist fails with an error
   saying how to fix it, and an upstream naming a path or URL instead of
   a named remote fails the same way. A local (`.`) upstream, and an
   upstream whose merge ref is missing or outside `refs/heads/`, are
   ignored and selection falls through to the next step.
2. Otherwise the `origin` remote, tracking the checked-out branch.
3. Otherwise the repository's only remote, tracking the checked-out
   branch.

A repository with no remote, or with several remotes, no `origin`, and
no upstream configured, fails with an error saying how to fix the
remotes first.

Re-running `add` in a tracked repository:

- is a no-op success when the detected branch, remote, and remote branch
  are unchanged — `repositories.toml` is left byte-identical;
- updates the stored `branch`, `remote`, and `remote_branch` in place
  when they changed and the repository can still be detected (the entry's
  name, path, and build fields are left as they are);
- is a no-op success when detection now fails (for example the remote was
  removed) — the stored entry is left unchanged.

When a different tracked repository already uses the new repository's
name, `add` tracks this one under a disambiguated name and logs the name
it chose.

### list

Prints the configured repositories (a debug rendering of the config,
followed by the list of repository paths) to stdout. Fails when the
config cannot be loaded.

### remove

`--repo-name <name>` (short form `-r`) is required. Removing a name that
is not tracked exits 0 and changes nothing (the command still takes the
config lock file next to `repositories.toml`). Accepted cost: a typo in
the name also exits 0 and removes nothing; the log line is the only
signal that the name was not tracked.

### start / stop / restart

A no-op happens only on a state read positively from
`systemctl is-active`:

- `start` on a running daemon (`active`) succeeds without touching it —
  it does not restart the daemon. On any other positively read state it
  issues the start.
- `stop` on a stopped daemon (`inactive`) succeeds without acting. On
  any other positively read state it issues the stop.
- `restart` always runs the restart; on a stopped daemon it starts it.
- When the state cannot be read (unit not installed, D-Bus unreachable,
  any `is-active` exit other than the state-reporting ones), `start` and
  `stop` exit non-zero naming the `systemctl status zlorb` command to
  inspect the unit, and issue no start or stop at all. `restart` fails
  from the restart itself, with the same inspect command in the error.
- A start, stop, or restart that `systemctl` reports as failed exits
  non-zero with the same inspect command in the error.

### setup

Creates the config directory and an empty `repositories.toml` in it
(existing files are left untouched), writes the systemd unit for the
resolved scope (its `ExecStart` is the `zlorb-service` binary: the
sibling next to `zlorb-ctl` when that file exists, else the path under
`$CARGO_HOME/bin` when `CARGO_HOME` is non-blank, else
`~/.cargo/bin/zlorb-service`; existence of the fallback paths is checked
after the unit is written, as the failure described below), then
runs `systemctl daemon-reload` and `systemctl enable --now zlorb.service`.
For a user service it also tries to enable linger. Prints the config
directory, the service binary, and the unit path to stdout.

- `--print-only` writes the config and unit files, prints the unit and
  the exact `systemctl` commands to run by hand, and runs none of them.
- When the service binary does not exist, `setup` exits non-zero; in
  the default mode the error names the exact commands to finish by hand,
  while with `--print-only` those commands are the `next:` line printed
  on stdout and the error only says to install `zlorb-service` first.
  When registration does not complete, `setup` exits non-zero and the
  error names the exact commands to finish by hand.
- Re-running `setup` rewrites the unit from the values resolved at that
  run; re-run it after changing `ZLORB_CONFIG_DIR` or `XDG_CONFIG_HOME`
  so the daemon and `zlorb-ctl` keep reading the same directory.

### watch

Runs `journalctl -f -u zlorb` (`--user` for a non-root user) and streams
its output. `watch` exits 0 only when `journalctl` itself exits 0; a
non-zero `journalctl` exit, or `journalctl` missing, is a command failure.

### web

Runs the web UI in the foreground, bound to `0.0.0.0:3000`, until the
process is stopped. It serves `/` (the repository page) and `/healthz`,
which answers the body `zlorb-web ok` plus a trailing newline. When the port is already
taken, `web` exits 1 with an error line instead of panicking. The web UI
is an on-demand process, not a systemd unit; nothing starts it
automatically.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success — including a no-op success, and a `status` report whose findings are problems (see below) |
| 1 | The command failed; its failure is reported as one `error: …` line on stderr |
| 2 | Argument error (unknown command or flag, missing required flag); clap prints a multi-line usage message to stderr and nothing to stdout |

Two reporting exceptions to the code-1 rule:

- `status` treats daemon and per-repository problems as its data, so
  finding a problem still exits 0 — the report itself succeeded. Only a
  config it cannot load at all is a command failure: exit 1, and with
  `--json` the JSON document is still printed to stdout, carrying the
  problem in its top-level `error` field (see the schema).
- `health` is a check, not a report: it exits 0 when healthy and 1 when
  not, with the report still printed to stdout and one `error: …` line
  on stderr naming each failed component.

## Streams and error format

- Data output goes to stdout. Log lines go to stdout when they are
  informational (`[INFO]: …`, `[DEBUG]: …`) and to stderr when they are
  warnings or errors (`[WARN]: …`, `[ERROR]: …`).
- A failed command reports its failure as one stderr line of the form
  `error: <message>`, with embedded newlines and escape sequences in the
  message flattened away, and exits 1. That line is the command's own
  report; it is not guaranteed to be the only stderr line — a warning
  logged on the way (for example `setup` finding a unit in the other
  scope) and the diagnostics of a child process whose stderr is
  inherited (`systemctl` for `start` / `stop` / `restart` / `setup`,
  `journalctl` for `watch`) can also appear on stderr.
- Argument errors are clap's: a multi-line usage message on stderr,
  exit 2, nothing on stdout.
- `SIGPIPE` keeps its default Unix behaviour, so piping a command's
  output into a closed pipe does not panic the process.

## `status --json` schema

One pretty-printed JSON document on stdout. Field names and types are
the contract; a test in `zlorb-ctl` pins the exact field-name sets.

Top level:

| Field | Type | Meaning |
| --- | --- | --- |
| `daemon_running` | boolean | True only when `systemctl is-active` positively read `active`. False when the daemon is stopped and when its state could not be read |
| `daemon_error` | string \| null | The daemon-state read failure, or null when the state was read (running or not) |
| `repositories` | array | One entry per tracked repository, in config order. Empty when no repository is tracked, and on a config load failure |
| `error` | string \| null | Null on a successful report. On a config load failure: the problem, with `repositories` empty, `daemon_running` false, `daemon_error` null — and the command exits 1 |

Each `repositories` entry:

| Field | Type | Meaning |
| --- | --- | --- |
| `name` | string | The tracked name from the config |
| `path` | string | The repository path from the config |
| `remote` | string | The remote name from the config |
| `branch` | string | The local branch from the config |
| `remote_branch` | string | The resolved tracked branch: the config's `remote_branch`, trimmed, or `branch` when that is unset or blank |
| `build_strategy` | string \| null | The resolved strategy name, `"bun"` or `"rust"` (a configured `"cargo"` resolves to `"rust"`). Null when the strategy cannot be resolved — that problem is in `error` |
| `build_command` | string | The configured legacy build command, verbatim (empty string when unset) |
| `restart_command` | string \| null | The configured restart command, trimmed; null when unset or blank |
| `out_dir` | string \| null | The configured output directory, verbatim; null when unset |
| `out_dir_exists` | boolean \| null | Whether `out_dir` exists (relative paths resolve against the repository root). Null when `out_dir` is unset, and also when the repository path itself does not exist |
| `path_exists` | boolean | Whether `path` exists on disk |
| `head_oid` | string \| null | The commit HEAD points at; null when it cannot be read |
| `last_fetch_oid` | string \| null | The commit on the `FETCH_HEAD` line for the tracked branch, read from the repository's common git dir; null when that file has no line for it |
| `last_fetch_unix` | integer \| null | The `FETCH_HEAD` file's modification time, in seconds since the Unix epoch; null when the repository was never fetched |
| `error` | string \| null | The first current problem found, in probe order — invalid branch or remote name, unresolvable build strategy, missing path, unreadable HEAD, unopenable repository, unreadable remote — or null when none was found |

Example (one healthy repository, daemon running):

```json
{
  "daemon_running": true,
  "daemon_error": null,
  "repositories": [
    {
      "name": "my-app",
      "path": "/home/you/my-app",
      "remote": "origin",
      "branch": "main",
      "remote_branch": "main",
      "build_strategy": "bun",
      "build_command": "",
      "restart_command": null,
      "out_dir": null,
      "out_dir_exists": null,
      "path_exists": true,
      "head_oid": "9f2c1ab4e5d60718293a4b5c6d7e8f90a1b2c3d4",
      "last_fetch_oid": null,
      "last_fetch_unix": null,
      "error": null
    }
  ],
  "error": null
}
```

`status` reports no build status: the daemon keeps build outcomes only
in memory and persists no status file, so no field is invented for it.

## `health --json` schema

One pretty-printed JSON document on stdout, printed whether the check
passes or fails.

| Field | Type | Meaning |
| --- | --- | --- |
| `daemon_running` | boolean | True only when `systemctl is-active` positively read `active` |
| `daemon_error` | string \| null | The daemon-state read failure, or null when the state was read |
| `web_responding` | boolean | True when the web probe read an HTTP status. The identity check below is applied to 2xx responses only, so a non-2xx status sets this true with no identity check performed |
| `web_status` | integer \| null | The HTTP status the web UI answered with; null when no status was read or the server failed the identity check |
| `web_error` | string \| null | Why the web probe failed (connect, timeout, non-HTTP, identity), or null when a status was read |
| `healthy` | boolean | True only when `daemon_running` is true and `web_status` is a 2xx. The command's exit code follows this field: 0 when true, 1 when false |

The web probe is one HTTP `GET /healthz` to `127.0.0.1:3000` — the
address `zlorb-ctl web` binds — under one 3-second deadline, reading at
most 8 KiB. A 2xx counts only when the response body, trimmed of surrounding
whitespace, equals `zlorb-web ok`, so a foreign server on port 3000 answering 200 fails the
identity check (`web_status` null, `web_error` set) instead of passing
as the web UI. A server answering a non-2xx status is responding but not
healthy: `web_responding` true, `web_status` set, `healthy` false.

Evidence level, stated as-is: the probe does not validate
`Content-Length` completeness. A peer that sends headers plus the
identity body and then stalls or closes early reads as responding, and
counts toward healthy when the daemon is also running.

Example (healthy):

```json
{
  "daemon_running": true,
  "daemon_error": null,
  "web_responding": true,
  "web_status": 200,
  "web_error": null,
  "healthy": true
}
```
