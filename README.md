# `govee`

A Rust library for controlling Govee smart lighting devices. Provides idiomatic async access to both the Govee cloud API (v1) and the local LAN API over UDP, a unified backend abstraction, device registry with name/alias resolution, and a scene system for multi-device presets.

Designed as a foundation for higher-level consumers — it has no opinion about how it is invoked.

## Status

**WIP** — in active development, not ready for production use. See the [development plan](#development-plan) for progress.

## Ecosystem

| Crate | Description | Repo |
|-------|-------------|------|
| **govee** | Core library — backends, registry, scenes | [wkusnierczyk/govee](https://github.com/wkusnierczyk/govee) |
| **govee-workflow** | Workflow engine — timed command sequences, choreography | [wkusnierczyk/govee-workflow](https://github.com/wkusnierczyk/govee-workflow) |
| **govee-cli** | Command-line interface | [wkusnierczyk/govee-cli](https://github.com/wkusnierczyk/govee-cli) |
| **govee-server** | HTTP/WebSocket server for remote control | [wkusnierczyk/govee-server](https://github.com/wkusnierczyk/govee-server) |
| **govee-mcp** | Model Context Protocol server for AI agents | [wkusnierczyk/govee-mcp](https://github.com/wkusnierczyk/govee-mcp) |

## Getting started

### Install

Add to your `Cargo.toml`:

```toml
[dependencies]
govee = "0.6"
tokio = { version = "1", features = ["full"] }
```

Or from source:

```sh
git clone https://github.com/wkusnierczyk/govee.git
cd govee
cargo build
```

### Quick example

```rust
use govee::config::Config;
use govee::registry::DeviceRegistry;
use govee::scene::SceneTarget;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(std::path::Path::new("config.toml"))?;
    let registry = DeviceRegistry::start(config).await?;

    // Control a device by name or alias
    let id = registry.resolve("kitchen")?;
    registry.set_brightness(&id, 75).await?;

    // Apply a scene to a group
    registry.apply_scene("warm", SceneTarget::Group("upstairs".into())).await?;

    Ok(())
}
```

For detailed API usage, see the sections below.

## Usage

### Device registry

The `DeviceRegistry` is the primary entry point. It merges devices from cloud and local backends, provides name/alias resolution, per-device backend routing, optimistic state caching, and command delegation.

```rust
use govee::config::Config;
use govee::registry::DeviceRegistry;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(std::path::Path::new("config.toml"))?;
    let registry = DeviceRegistry::start(config).await?;

    // Resolve by name or alias
    let id = registry.resolve("kitchen")?;
    let state = registry.get_state(&id).await?;
    println!("brightness: {}", state.brightness);

    // Control
    registry.set_brightness(&id, 75).await?;
    registry.set_color(&id, govee::types::Color::new(255, 128, 0)).await?;

    // Group commands (concurrent)
    registry.group_set_power("upstairs", true).await?;

    Ok(())
}
```

Group commands execute concurrently via `join_all`. Designed for typical Govee setups with fewer than ~20 devices per group. Larger groups may trigger cloud API rate limits.

### Scenes

Built-in and user-defined lighting presets. Each scene sets brightness and either an RGB color or a color temperature.

```rust
use govee::scene::SceneTarget;

// Apply a built-in scene to a single device
registry.apply_scene("warm", SceneTarget::DeviceName("kitchen".into())).await?;

// Apply to a group
registry.apply_scene("focus", SceneTarget::Group("office".into())).await?;

// Apply to all devices
registry.apply_scene("night", SceneTarget::All).await?;
```

Built-in scenes: `warm` (2700K/40%), `focus` (5500K/80%), `night` (red/10%), `movie` (2200K/20%), `bright` (6500K/100%).

User-defined scenes are loaded from the config file and can override built-ins:

```toml
[scenes.reading]
color_temp = 4000
brightness = 60

[scenes.night]  # overrides built-in
color = { r = 128, g = 0, b = 0 }
brightness = 5
```

`apply_scene` sends 2 commands per device (color/temp then brightness). On partial failure, some devices may be in an intermediate state — no rollback is attempted.

### Cloud backend

For direct backend access without the registry:

```rust
use govee::backend::cloud::CloudBackend;
use govee::backend::GoveeBackend;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let backend = CloudBackend::new("your-api-key".into(), None)?;
    let devices = backend.list_devices().await?;
    let state = backend.get_state(&devices[0].id).await?;
    backend.set_brightness(&devices[0].id, 75).await?;
    Ok(())
}
```

The API key is obtained from the Govee Home mobile app. HTTPS is enforced for all remote URLs.

### Local LAN backend

```rust
use govee::backend::local::LocalBackend;
use govee::backend::GoveeBackend;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let backend = LocalBackend::new(Duration::from_secs(2), 60).await?;
    let devices = backend.list_devices().await?;
    backend.set_color(&devices[0].id, govee::types::Color::new(255, 0, 128)).await?;
    Ok(())
}
```

Requires the device to be on the same LAN segment. Port 4002 must be available (not used by Home Assistant, govee2mqtt, etc.).

### Configuration

```toml
api_key = "your-api-key"
backend = "auto"            # auto | cloud | local
discovery_interval_secs = 60

[aliases]
kitchen = "H6076 Kitchen Strip"
bedroom = "H6078 Bedroom Light"

[groups]
upstairs = ["bedroom"]
all = ["kitchen", "bedroom"]

[scenes.reading]
color_temp = 4000
brightness = 60
```

See `Config` docs for all available options.

## Security

The Govee LAN protocol is **unauthenticated plaintext UDP**. Any device on the local network can discover, control, and impersonate Govee devices. This is a fundamental property of the protocol — the library mitigates where possible (e.g., using UDP source IP over payload IP) but cannot fully prevent LAN-based attacks.

The cloud backend uses HTTPS with system CA verification (no certificate pinning). The API key is sent in every request header. `Config` redacts the API key in `Debug` and `Serialize` output, but the key is stored in memory in plaintext.

Scene names are restricted to alphanumeric characters, `-`, and `_` to prevent log injection. Color temperature is capped at 10000K to avoid undefined firmware behavior.

See [SECURITY.md](SECURITY.md) for the full threat model.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for build, test, lint, and git hooks setup.

## Development plan

| Milestone | Scope | Status |
|-----------|-------|--------|
| **M1 — Scaffold & CI/CD** | Cargo project, module stubs, GitHub Actions for CI (fmt + clippy, build, test) and two-step immutable release on tag | v0.1.0 |
| **M2 — Core types & configuration** | `DeviceId`, `Device`, `DeviceState`, `Color`, `GoveeError`, `Config` with TOML parsing, input validation | v0.2.0 |
| **M3 — Cloud backend (v1)** | `GoveeBackend` trait, `CloudBackend` (list, state, control), rate limit handling, User-Agent, timeouts | v0.3.0 |
| **M4 — Local LAN backend** | `LocalBackend` with UDP multicast discovery, unicast control, state queries, port conflict detection, TTL-based device cache | v0.4.0 |
| **M5 — Device registry** | `DeviceRegistry`: cloud+local merge, name/alias resolution, backend auto-selection, optimistic state cache, groups | v0.5.0 |
| **M6 — Scenes** | Built-in + user-defined scene presets, `apply_scene` with device/group/all targeting, scene validation | v0.6.0 |
| **M7 — SRE & hardening** | Structured tracing, retry/backoff, graceful degradation, security audit, integration test suite, threat model docs | Pending |
