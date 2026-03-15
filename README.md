# govee

A Rust library for controlling Govee smart lighting devices. Provides idiomatic async access to both the Govee cloud API (v1) and the local LAN API over UDP, a unified backend abstraction, device registry with name/alias resolution, and a scene system for multi-device presets.

Designed as a foundation for `govee-cli`, `govee-server`, and `govee-mcp` — it has no opinion about how it is invoked.

## Getting started

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (1.85+ for edition 2024)
- [Lefthook](https://github.com/evilmartians/lefthook) — git hooks manager

### Set up git hooks

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

### Build

```sh
cargo build
```

### Test

```sh
cargo test
```

### Lint

```sh
cargo fmt --check
cargo clippy
```

## Development plan

| Milestone | Scope |
|-----------|-------|
| **M1 — Scaffold & CI/CD** | Cargo project, module stubs, GitHub Actions for CI (fmt + clippy, build, test) and two-step immutable release on tag |
| **M2 — Core types & configuration** | `DeviceId`, `Device`, `DeviceState`, `Color`, `GoveeError`, `Config` with TOML parsing, input validation |
| **M3 — Cloud backend (v1)** | `GoveeBackend` trait, `CloudBackend` implementation (list, state, control), rate limit handling, wiremock tests |
| **M4 — Local LAN backend** | `LocalBackend` with UDP multicast discovery, unicast control, state queries, port conflict detection |
| **M5 — Device registry** | `DeviceRegistry`: cloud+local merge, name/alias resolution, backend auto-selection, optimistic state cache, groups |
| **M6 — Scenes & workflow stub** | Built-in + user-defined scene presets, `apply_scene`, workflow engine stub (`NotImplemented`) |
| **M7 — SRE & hardening** | Structured tracing, retry/backoff, graceful degradation, security audit, integration test suite, threat model docs |
