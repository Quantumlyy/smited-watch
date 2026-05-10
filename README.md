# smited-watch

Wrap any command. Pipe its output through transparently. Buzz when something
goes wrong.

`smited-watch` is a small Rust CLI that wraps any other command (`npm run dev`,
`cargo build`, `pnpm test`, anything that emits text), streams its stdio
through your terminal byte-perfect, and fires haptic sensations on the
[smited](https://github.com/Quantumlyy/smited) daemon when the wrapped
command's output matches configured patterns.

The tool is intentionally tool-agnostic. It works with anything that emits
text on stdout or stderr: no per-tool plugins, no project-file changes, no
language-specific integration. Regex on stdout is the most universal
interface in software.

## Quickstart

```sh
# Install (requires a Rust toolchain).
cargo install --git https://github.com/Quantumlyy/smited-watch

# First run — writes a template config to your platform's user-config dir
# and runs your command in dry-run mode (matches still log; no triggers
# fired because no daemon host is configured yet).
smited-watch -- npm run dev

# Edit the config to point at your daemon and tweak the patterns.
$EDITOR "$XDG_CONFIG_HOME/smited/watch.toml"   # or ~/.config/smited/watch.toml

# Run again. Now matches on `error TS\d+` (and the other defaults) fire
# the configured sensations on the daemon.
smited-watch -- npm run dev
```

See [`examples/wrap.toml`](examples/wrap.toml) for a fully-annotated config
template, [`docs/configuration.md`](docs/configuration.md) for the full
schema reference, and [`docs/patterns-cookbook.md`](docs/patterns-cookbook.md)
for ready-to-paste regex patterns covering the most common toolchains.

## Wrap-anything ethos

Anything that emits text works:

```sh
smited-watch -- npm run dev
smited-watch -- pnpm test
smited-watch -- yarn build
smited-watch -- vitest --watch
smited-watch -- jest --watch

smited-watch -- cargo build
smited-watch -- cargo test
smited-watch -- cargo watch -x build

smited-watch -- dotnet build
smited-watch -- dotnet test

smited-watch -- go build ./...
smited-watch -- go test ./...

smited-watch -- make
smited-watch -- bazel build //...
smited-watch -- nix build
```

If a tool emits the text you'd notice with your eyes, write a regex for it
and `smited-watch` will buzz when it appears.

## How it works

```text
┌────────────────────┐
│  parent terminal   │  ← byte-perfect stdio (PTY when parent is TTY)
└──────────▲─────────┘
           │
           │  byte-perfect tee
           │
┌──────────┴─────────┐    matches    ┌─────────────────────┐
│   smited-watch     ├───────────────►│   smited daemon     │
│   (wraps `cmd`)    │   gRPC trigger │   (your hardware)   │
└──────────┬─────────┘                └─────────────────────┘
           │
           │  spawn
           ▼
       child cmd
       (`npm run dev`, ...)
```

* The wrapped command sees a terminal and renders normally — spinners
  animate, ANSI colour works, terminal clear sequences pass through.
* The watcher reads a *copy* of every byte, accumulates lines, strips
  ANSI, runs each line through a regex set, and fires the matching
  pattern's sensation on the daemon.
* Triggers are fire-and-forget. Daemon down? Network blip? The watcher
  shrugs, the wrapped command keeps running, and the watcher exits with
  the wrapped command's exit code unchanged. CI and shell pipelines
  work unmodified.

## Configuration

`smited-watch` reads `watch.toml` from your platform's user-config
directory:

| Platform | Default path |
|----------|--------------|
| Linux/macOS | `$XDG_CONFIG_HOME/smited/watch.toml` (falls back to `~/.config/smited/watch.toml` when XDG is unset) |
| Windows | `%APPDATA%\smited\watch.toml` |

If the file doesn't exist when you first run `smited-watch`, it writes a
fully-commented template there and proceeds with defaults (matches will
log but no triggers will fire because no host is configured). Edit the
file and rerun.

Override the path with `--config <path>` or `$SMITED_WATCH_CONFIG`. An
explicit `--config` path that doesn't exist is treated as an error
(intentional: only the *default* location auto-creates).

The full schema, including every default and field description, is in
[`docs/configuration.md`](docs/configuration.md).

## Dry-run mode

When the daemon's `host` is unset (in config, on the CLI, or via
`$SMITED_HOST`), `smited-watch` runs in **dry-run mode**: patterns still
match and log at INFO level, but no triggers are fired. Useful for tuning
patterns before pointing at hardware.

You can also force dry-run with `--dry-run` / `-n`:

```sh
smited-watch --dry-run -- npm run dev
```

## Disable for a single run

```sh
SMITED_WATCH_DISABLE=1 smited-watch -- npm run dev
```

This makes the binary a transparent passthrough — spawn, pipe, propagate
exit. No scanning, no triggers. Useful when you want quiet for one run
without editing config.

## CLI reference

```sh
smited-watch --help
```

| Flag | Env var | Purpose |
|------|---------|---------|
| `-c`, `--config <PATH>` | `SMITED_WATCH_CONFIG` | Override config file path |
| `-H`, `--host <HOST:PORT>` | `SMITED_HOST` | Override daemon address |
| `-b`, `--backend-id <ID>` | `SMITED_BACKEND_ID` | Override default backend |
| `-v`, `--verbose` | — | Increase log verbosity (`-v`=debug, `-vv`=trace) |
| `-q`, `--quiet` | — | Suppress all smited-watch logging |
| `-n`, `--dry-run` | — | Match patterns and log them, never fire triggers |
| `--no-banner` | — | Skip the one-line "wrapping <cmd>" startup banner |
| — | `SMITED_WATCH_DISABLE` | Set to `1` for pure passthrough |

## License

MIT OR Apache-2.0
