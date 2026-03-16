# govee

A Rust library for controlling Govee smart lighting devices. Provides idiomatic async access to both the Govee cloud API (v1) and the local LAN API over UDP, a unified backend abstraction, device registry with name/alias resolution, and a scene system for multi-device presets.

Designed as a foundation for `govee-cli`, `govee-server`, and `govee-mcp` — it has no opinion about how it is invoked.

## Status

**WIP**: This project is currently in development and not ready for production use.  
**Done**: [M1](#development-plan) in v0.1.0, [M2](#development-plan) in v0.2.0, [M3](#development-plan) in v0.3.0, [M4](#development-plan) in v0.4.0

## Getting started

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (1.91+ for edition 2024)
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

## Usage

### Cloud backend

```rust
use govee::backend::cloud::CloudBackend;
use govee::backend::GoveeBackend;

let backend = CloudBackend::new("your-api-key".into(), None)?;
let devices = backend.list_devices().await?;
let state = backend.get_state(&devices[0].id).await?;
backend.set_brightness(&devices[0].id, 75).await?;
```

The API key is obtained from the Govee Home mobile app. HTTPS is enforced for all remote URLs.

### Local LAN backend

```rust
use govee::backend::local::LocalBackend;
use govee::backend::GoveeBackend;
use std::time::Duration;

let backend = LocalBackend::new(Duration::from_secs(2), 60).await?;
let devices = backend.list_devices().await?;
backend.set_color(&devices[0].id, govee::types::Color::new(255, 0, 128)).await?;
```

Requires the device to be on the same LAN segment. Port 4002 must be available (not used by Home Assistant, govee2mqtt, etc.).

### Configuration

```rust
use govee::config::Config;

let config = Config::load(std::path::Path::new("config.toml"))?;
println!("backend: {:?}", config.backend());
```

See `Config` docs for TOML format and available options.

### Lint

```sh
cargo fmt --check
cargo clippy
```

## Development plan

| Milestone | Scope | Status |
|-----------|-------|--------|
| **M1 — Scaffold & CI/CD** | Cargo project, module stubs, GitHub Actions for CI (fmt + clippy, build, test) and two-step immutable release on tag | v0.1.0 |
| **M2 — Core types & configuration** | `DeviceId`, `Device`, `DeviceState`, `Color`, `GoveeError`, `Config` with TOML parsing, input validation | v0.2.0 |
| **M3 — Cloud backend (v1)** | `GoveeBackend` trait, `CloudBackend` (list, state, control), rate limit handling, User-Agent, timeouts, 84 wiremock+unit tests | v0.3.0 |
| **M4 — Local LAN backend** | `LocalBackend` with UDP multicast discovery, unicast control, state queries, port conflict detection, TTL-based device cache | v0.4.0 |
| **M5 — Device registry** | `DeviceRegistry`: cloud+local merge, name/alias resolution, backend auto-selection, optimistic state cache, groups | Pending |
| **M6 — Scenes & workflow stub** | Built-in + user-defined scene presets, `apply_scene`, workflow engine stub (`NotImplemented`) | Pending |
| **M7 — SRE & hardening** | Structured tracing, retry/backoff, graceful degradation, security audit, integration test suite, threat model docs | Pending |
