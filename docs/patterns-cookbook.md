# Patterns cookbook

Ready-to-paste regex patterns for the most common toolchains. Drop these
into the `[[patterns]]` array of your `watch.toml`. Each entry below
shows the regex, the recommended `debounce_ms`, and a sample line the
regex matches.

> **Tip.** The `name` is for logs only; pick whatever helps you read the
> output. The `sensation` should match a sensation registered on your
> backend — the cookbook leaves that as a placeholder.

## TypeScript

### `tsc` errors

```toml
[[patterns]]
name        = "tsc_error"
regex       = 'error TS\d+:'
sensation   = "compile_error_mild"
debounce_ms = 500
```

Matches:

```
src/index.ts:42:7 - error TS2322: Type 'string' is not assignable to type 'number'.
```

### `tsc --watch` summary lines (compile finished cleanly or with errors)

```toml
[[patterns]]
name        = "tsc_watch_failed"
regex       = 'Found [1-9]\d* errors?\. Watching for file changes\.'
sensation   = "compile_error_severe"
debounce_ms = 1500
```

Matches:

```
[12:34:01 PM] Found 3 errors. Watching for file changes.
```

> Pair with a "compile clean" sensation if you want positive reinforcement:
> `regex = 'Found 0 errors\. Watching for file changes\.'` →
> `sensation = "deploy_success"`.

## Vite / esbuild

```toml
[[patterns]]
name        = "vite_error"
regex       = '\[vite\].*error|✗ Build failed'
sensation   = "compile_error_severe"
debounce_ms = 1000
```

Matches:

```
[vite] Internal server error: Failed to resolve import "react"
✗ Build failed in 142ms
```

## Vitest / Jest

### Test failure (vitest)

```toml
[[patterns]]
name        = "vitest_failed"
regex       = '(?i)^\s*Test Files\s+\d+\s+failed'
sensation   = "test_failed"
debounce_ms = 1500
```

Matches:

```
 Test Files  3 failed | 12 passed (15)
```

### Test failure (jest)

```toml
[[patterns]]
name        = "jest_failed"
regex       = '(?i)Tests:\s+\d+\s+failed'
sensation   = "test_failed"
debounce_ms = 1500
```

Matches:

```
Tests:       3 failed, 12 passed, 15 total
```

### Generic "N failed" / "N failing" catch-all

Pure shotgun pattern useful as a fallback for less-known runners.

```toml
[[patterns]]
name        = "generic_test_failure"
regex       = '(?i)\b(\d+)\s+(failed|failing)\b'
sensation   = "test_failed"
debounce_ms = 2000
```

Matches "1 failed", "12 failing", "FAILED 3", etc.

## ESLint

### Per-file lint warnings (chatty — use a long debounce)

```toml
[[patterns]]
name            = "eslint_warning"
regex           = '\d+ warning'
sensation       = "test_failed"
debounce_ms     = 5000
intensity_scale = 30   # turn it down — these fire constantly during editing
```

Matches:

```
✖ 12 problems (0 errors, 12 warnings)
```

### ESLint summary "N problems" line

```toml
[[patterns]]
name        = "eslint_summary"
regex       = '✖\s+\d+\s+problems?'
sensation   = "compile_error_mild"
debounce_ms = 2000
```

Matches:

```
✖ 12 problems (3 errors, 9 warnings)
```

## Cargo (Rust)

### Compile errors

```toml
[[patterns]]
name        = "cargo_compile_error"
regex       = '^error(\[E\d+\])?:'
sensation   = "compile_error_mild"
debounce_ms = 500
```

Matches:

```
error[E0308]: mismatched types
error: cannot find macro `vex` in this scope
```

### Panic in `cargo run` / `cargo test`

```toml
[[patterns]]
name        = "rust_panic"
regex       = "thread '.+' panicked at"
sensation   = "compile_error_severe"
debounce_ms = 1000
```

Matches:

```
thread 'main' panicked at src/lib.rs:42:5:
```

### Test failures (`cargo test` summary)

```toml
[[patterns]]
name        = "cargo_test_failed"
regex       = 'test result: FAILED\.'
sensation   = "test_failed"
debounce_ms = 1500
```

Matches:

```
test result: FAILED. 5 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out
```

## .NET

### `dotnet build` MSBuild errors

```toml
[[patterns]]
name        = "msbuild_error"
regex       = '\): error [A-Z]+\d+:'
sensation   = "compile_error_mild"
debounce_ms = 500
```

Matches:

```
/path/to/Project.cs(42,7): error CS1002: ; expected
```

### `dotnet test` failures

```toml
[[patterns]]
name        = "dotnet_test_failed"
regex       = 'Failed!\s+-\s+Failed:\s+\d+'
sensation   = "test_failed"
debounce_ms = 1500
```

Matches:

```
Failed!  - Failed:    3, Passed:    12, Skipped:     0, Total:    15
```

## Go

### Compile errors

```toml
[[patterns]]
name        = "go_compile_error"
regex       = '^.+\.go:\d+:\d+:\s+'
sensation   = "compile_error_mild"
debounce_ms = 500
```

Matches:

```
./main.go:42:7: undefined: foo
```

### Test failures

```toml
[[patterns]]
name        = "go_test_failed"
regex       = '^FAIL\s+\S+'
sensation   = "test_failed"
debounce_ms = 1000
```

Matches:

```
FAIL    github.com/example/pkg  0.123s
```

## Stack Overflow URLs in browser logs (cursed but funny)

Detect when the dev console logs a Stack Overflow URL — typically
because somebody pasted a reproduction snippet into a debugger and
the URL surfaced. Useful for self-shaming during pair programming.

```toml
[[patterns]]
name            = "so_url"
regex           = 'stackoverflow\.com/(questions|a)/'
sensation       = "test_failed"
debounce_ms     = 5000
intensity_scale = 50
```

Matches:

```
[INFO] visited: https://stackoverflow.com/questions/12345/why-is-this-broken
```

## Composing patterns

Patterns are independent — every match fires its own sensation (subject
to its own debouncer). This means a single line can trigger multiple
sensations if it matches multiple regexes. Use distinct sensation names
when you want to feel the difference, or the same sensation name plus
a long combined debounce when you want them to coalesce.
