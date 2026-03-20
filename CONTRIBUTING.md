# Contributing to `govee`

## Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (1.91+ for edition 2024)
- [Lefthook](https://github.com/evilmartians/lefthook) — git hooks manager

## Set up git hooks

Install lefthook and activate the hooks before making any commits:

```sh
# macOS
brew install lefthook

# or via npm/cargo/go — see https://github.com/evilmartians/lefthook/blob/master/docs/install.md
```

```sh
lefthook install
```

This configures:
- **pre-commit** — `cargo fmt --check` and `cargo clippy` (parallel)
- **pre-push** — `cargo build` then `cargo test` (sequential)

## Build

```sh
cargo build
```

## Test

```sh
cargo test
```

All tests run with `cargo test` and zero arguments — no feature flags or special options.

## Lint

```sh
cargo fmt --check
cargo clippy
```

## Branch naming

- `dev/i{NNN}-{slug}` for per-issue branches (e.g., `dev/i023-registry-construction`)
- One PR per issue, merged sequentially
