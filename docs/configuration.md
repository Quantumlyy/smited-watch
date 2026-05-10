# Configuring smited-watch

Configuration lives in a single TOML file: `watch.toml`.

## Default config path

| Platform | Default path |
|----------|--------------|
| Linux/macOS | `$XDG_CONFIG_HOME/smited/watch.toml` (falls back to `~/.config/smited/watch.toml` when `XDG_CONFIG_HOME` is unset or empty) |
| Windows | `%APPDATA%\smited\watch.toml` |

Override with `--config <path>` or `$SMITED_WATCH_CONFIG`.

If the *default* location doesn't exist on first run, `smited-watch`
writes a fully-commented template there and proceeds with defaults
(matches still log; no triggers fire because no host is set). An
*explicit* `--config` path that doesn't exist is treated as an error.

A complete annotated example is in
[`../examples/wrap.toml`](../examples/wrap.toml).

## Schema

```toml
# ─── Daemon connection ─────────────────────────────────────────────────

[smited]
host       = "windows-rig.local:7777"   # optional; unset ⇒ dry-run mode
backend_id = "mock-owo"                 # optional; default backend for triggers

[smited.connection]
timeout_ms = 500                        # per-trigger connect+RPC timeout
strategy   = "persistent"               # or "per_trigger"

# ─── Patterns (zero or more) ───────────────────────────────────────────

[[patterns]]
name            = "ts_error"            # required; appears in logs
regex           = 'error TS\d+'         # required; Rust `regex` crate syntax
sensation       = "compile_error_mild"  # required; sensation name on the daemon
backend_id      = "owo-primary"         # optional; falls back to [smited].backend_id
debounce_ms     = 500                   # default 500
intensity_scale = 75                    # optional 0..100; falls back to sensation default
priority        = 25                    # optional -1000..1000; default 0

# ─── On-exit sensations ────────────────────────────────────────────────

[on_exit]
success_sensation        = "deploy_success"
failure_sensation        = "compile_error_severe"
success_min_duration_ms  = 30000        # default 30000 (30 s)
failure_dedupe_window_ms = 2000         # default 2000 (2 s)
```

### Field reference

#### `[smited]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `host` | `string?` | unset | `host:port` of the daemon. Unset ⇒ **dry-run mode** (matches log but no triggers fire). |
| `backend_id` | `string?` | unset | Default backend id for triggers; per-pattern entries can override. Falls back to `mock-owo` if unset. |

#### `[smited.connection]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `timeout_ms` | `u64` | `500` | Per-trigger connect+RPC timeout. Triggers exceeding this are dropped silently and logged at debug. |
| `strategy` | `"persistent"` \| `"per_trigger"` | `"persistent"` | Connection lifecycle. |

* `persistent` keeps a single gRPC channel open for the watcher's lifetime;
  faster, with the trade-off that a daemon restart leaves a stale channel
  until the next trigger fails (then we reconnect).
* `per_trigger` opens a fresh channel for each trigger; slower but always
  fresh.

#### `[[patterns]]` (each entry)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | `string` | required | Stable identifier; appears in log lines and in the daemon's `client_trace_id`. |
| `regex` | `string` | required | Pattern matched against each line of the wrapped command's combined stdout/stderr. Single-line, anchored. ANSI codes are stripped before matching. Rust `regex` syntax (no lookaround). |
| `sensation` | `string` | required | Sensation name to fire on the daemon. Must already be registered on the target backend. |
| `backend_id` | `string?` | falls back to `[smited].backend_id` | Per-pattern backend override. |
| `debounce_ms` | `u64` | `500` | Leading-edge debounce window. Subsequent matches within the window are dropped (not queued). `0` disables debouncing. |
| `intensity_scale` | `u32?` | unset | 0–100; falls back to the sensation's own default. |
| `priority` | `i32?` | unset | -1000..1000; falls back to `0` on the wire. |

Each pattern's regex is compiled once at startup. A bad regex is rejected
with a clear error mentioning the pattern's `name` so you can find it in
your config.

Multi-line patterns are not supported. If you need them, pre-process the
output in your shell command.

#### `[on_exit]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `success_sensation` | `string` | `""` | Sensation fired on exit code 0. Empty disables. |
| `failure_sensation` | `string` | `""` | Sensation fired on nonzero exit code. Empty disables. |
| `success_min_duration_ms` | `u64` | `30000` | Don't celebrate trivially fast successes. |
| `failure_dedupe_window_ms` | `u64` | `2000` | Don't double-fire on failure if a `[[patterns]]` entry already matched within the last N ms. |

### Behaviour summary

* **Dry-run mode.** Triggered when `host` is unset (in config, on the CLI,
  or via `$SMITED_HOST`) or when `--dry-run` / `-n` is passed.
  Patterns match, log at INFO, no triggers fired.
* **`SMITED_WATCH_DISABLE=1`.** Pure passthrough. No config load, no
  scanning, no triggers, no banner. Stdio passes through, exit code
  propagates. Useful for one-off "quiet" runs.
* **Exit-code propagation.** The watcher always exits with the wrapped
  command's exit code. Trigger failures (network errors, daemon down,
  timeouts) are NEVER user-visible — they're debug-level log lines.
* **Signal forwarding.** SIGINT/SIGTERM are forwarded to the child. The
  watcher then exits, suppressing any `[on_exit]` sensation (the user
  asked for the run to stop, not for confirmation it did).
* **PTY vs pipes.** When the parent is a TTY, the watcher allocates a
  PTY for the child so build tools detect a terminal and render normally
  (spinners, colours). When the parent isn't a TTY, the watcher uses
  plain pipes.

## Environment variables

| Variable | Purpose |
|----------|---------|
| `SMITED_HOST` | Equivalent to `--host`. Overrides `[smited].host`. |
| `SMITED_BACKEND_ID` | Equivalent to `--backend-id`. Overrides `[smited].backend_id`. |
| `SMITED_WATCH_CONFIG` | Equivalent to `--config`. Overrides the config file path. |
| `SMITED_WATCH_DISABLE` | Set to `1` for pure-passthrough mode. |
| `RUST_LOG` | Standard `tracing-subscriber` filter; overrides the `-v`/`-q` derived filter. |

## Trace ids

Every fired trigger carries a `client_trace_id`:

* Pattern matches: `watch-<pattern.name>-<unix_ms>`
* On-exit fires: `watch-on-exit-<unix_ms>`

The daemon's history shows these so you can correlate a sensation back
to the watcher invocation that fired it.
