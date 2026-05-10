# Installing smited-watch

`smited-watch` is a single self-contained Rust binary. It depends on no
runtime libraries beyond the standard system C library on each platform.

## From source (recommended for now)

```sh
cargo install --git https://github.com/Quantumlyy/smited-watch
```

This builds from the latest `main` and installs the binary into
`~/.cargo/bin/smited-watch`. Make sure that's on your `$PATH`.

You'll need a recent stable Rust toolchain (`rustup default stable`).
The repo currently targets `rustc 1.88` and newer (declared as
`rust-version = "1.88"` in `Cargo.toml`).

## Pinning a specific version

```sh
cargo install --git https://github.com/Quantumlyy/smited-watch --tag v0.1.0
```

## Pre-built binaries

Tagged releases on GitHub publish pre-built binaries for:

- `x86_64-apple-darwin` (Intel Mac)
- `aarch64-apple-darwin` (Apple Silicon)
- `x86_64-pc-windows-msvc`
- `x86_64-unknown-linux-gnu`

Download from <https://github.com/Quantumlyy/smited-watch/releases>,
extract, and put `smited-watch` somewhere on your `$PATH`.

## Platform notes

### Linux / macOS

Fully supported. `smited-watch` allocates a PTY when the parent process
is a TTY so wrapped tools (vitest --watch, cargo watch, …) detect a
terminal and don't downgrade their output.

### Windows

Best-effort for v0.1. Output passthrough works via ConPTY through
`portable-pty`. Stdin forwarding (needed for keystroke-driven
`vitest --watch` and similar) is not yet wired up; the child still
receives keystrokes through Windows' usual console handle inheritance,
but raw-mode forwarding is Unix-only.

### CI and non-TTY environments

When the parent isn't a TTY (CI, log-file redirection, piped output),
`smited-watch` skips PTY allocation and uses plain pipes. Pattern
matching still works fine because tools downgrade to plain output, which
is easier to regex against anyway.

## Verifying

```sh
smited-watch --version
smited-watch --help
smited-watch -- echo hello   # should print "hello"
```

## Uninstall

```sh
cargo uninstall smited-watch
rm -rf "$XDG_CONFIG_HOME/smited"   # or ~/.config/smited on Linux/macOS
```

(On Windows, remove `%APPDATA%\smited\` instead.)
