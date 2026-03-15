# govee — Library Design Document

The `govee` crate is a Rust library for controlling Govee smart lighting devices. It provides
idiomatic async access to both the Govee cloud API and the local LAN API, a unified backend
abstraction, device registry, and a workflow engine for scripting complex multi-device behaviours.

It is the foundation on which `govee-cli`, `govee-server`, and `govee-mcp` are built. It has no
opinion about how it is invoked — no CLI, no HTTP server, no MCP protocol.

---

## 1. Govee APIs

Govee exposes two independent control planes: a cloud REST API and a local LAN API over UDP. They
are not mirrors of each other — they differ in authentication model, latency, feature surface, and
reliability characteristics.

### 1.1 Cloud API (v1 / v2)

A conventional HTTPS REST API hosted by Govee. Authentication uses a static API key passed as the
`Govee-API-Key` header, obtained via the Govee Home mobile app.

**Endpoints (v1):**

| Method | Endpoint | Purpose |
|---|---|---|
| `GET` | `/v1/devices` | List all devices (model, MAC, name, controllability) |
| `GET` | `/v1/devices/state` | Query current state of a single device |
| `PUT` | `/v1/devices/control` | Send a control command to a single device |

**Control commands (v1):**

| Command | Parameter | Range |
|---|---|---|
| `turn` | `value: "on" \| "off"` | — |
| `brightness` | `value: 0–100` | integer |
| `color` | `{ r, g, b }` | 0–255 each |
| `colorTem` | `value` | device-dependent Kelvin range |

**Rate limiting:** 10,000 requests per 24 hours at the account level, with an additional
per-minute cap. Exceeding either returns HTTP 429.

**State freshness:** The state endpoint returns cached values. The `online` field is unreliable.
State changed via Bluetooth from the Govee app is not reflected until a Wi-Fi sync occurs.

**v2 API:** Released late 2023. Adds capability negotiation — devices declare which commands they
support, including scenes, music mode, and DIY effects. The v2 device list returns a `capabilities`
array per device. H6078 is explicitly supported in v2. Less community documentation than v1 but
meaningfully richer for capable devices.

### 1.2 Local LAN API

Communicates directly with devices on the same LAN segment over UDP. Must be explicitly enabled
per device in the Govee Home app (Settings → LAN Control).

**Confirmed supported devices for this project:** H6076, H6078, H6079.

**Transport:**

- Discovery: multicast UDP to `239.255.255.250:4001`; devices respond to the client on port `4002`
- Control: unicast UDP to the device IP on port `4003`
- Port numbers are fixed by the protocol — only one LAN API implementation may run per host IP

**Discovery:**

```json
// client → 239.255.255.250:4001
{ "msg": { "cmd": "scan", "data": { "account_topic": "reserve" } } }

// device → client:4002
{ "msg": { "cmd": "scan", "data": {
  "ip": "192.168.1.42", "device": "AA:BB:CC:DD:EE:FF:00:11",
  "sku": "H6078", "bleVersionHard": "3.01.01", "wifiVersionSoft": "1.02.03"
}}}
```

**Control commands:**

```json
{ "msg": { "cmd": "turn",       "data": { "value": 1 } } }
{ "msg": { "cmd": "brightness", "data": { "value": 80 } } }
{ "msg": { "cmd": "colorwc",    "data": { "color": {"r":255,"g":100,"b":0}, "colorTemInKelvin": 0 } } }
{ "msg": { "cmd": "devStatus",  "data": {} } }
```

**State query response:**

```json
{ "msg": { "cmd": "devStatus", "data": {
  "onOff": 1, "brightness": 100,
  "color": {"r": 255, "g": 100, "b": 0}, "colorTemInKelvin": 7200
}}}
```

**State update lag:** Devices do not reliably reflect a write command in their state response for
several seconds. The recommended pattern is optimistic update, not read-after-write.

### 1.3 API Comparison

| Dimension | Cloud API | Local LAN API |
|---|---|---|
| **Auth** | Static API key (HTTPS header) | None |
| **Latency** | ~200–400ms | ~5–15ms |
| **Rate limit** | 10,000 req/24h + per-minute cap | None |
| **Requires internet** | Yes | No |
| **Device coverage** | Broader (includes BT-only devices) | WiFi + LAN-enabled devices only |
| **State accuracy** | Cached, can be stale | Fresher, but post-write lag |
| **Scenes / DIY** | v2 API, partial | Unofficial only |
| **Discovery** | Cloud returns device list | UDP multicast |
| **Concurrent users** | Unlimited | One per host IP (fixed ports) |
| **Reliability** | Depends on Govee cloud uptime | Local network only |
| **Setup** | API key request (email) | Per-device opt-in in app |

The local API is strictly better for the primitives it covers. The cloud API is the fallback for
devices that don't support local control, and the authoritative source for device names.

---

## 2. Existing Crates

Three Rust crates in this space were evaluated before deciding to write a new library.

**`govee-api` (mgierada):** Wraps all v1 cloud REST endpoints with typed structs and an async
client. Cloud-only, no LAN support, no v2, no backend abstraction, no device registry. 3 GitHub
stars, 0 forks. The HTTP client layer it provides is ~150 lines — not worth the dependency.

**`cute-lights`:** A multi-brand unified library (Govee local LAN, Philips Hue, TP-Link Kasa,
OpenRGB) behind a single trait. Govee support is LAN-only and one of several backends. The
unified abstraction is necessarily lossy — device-specific capabilities don't survive a
lowest-common-denominator API.

**`rship-govee`:** A connector for the `rship` automation framework. Cloud API only, tightly
coupled to `rship-sdk`. Not a general-purpose library.

**Decision: write `govee` from scratch.** The HTTP client and UDP stack together are ~350 lines.
The value is not saving those lines — it's the architecture around them that doesn't exist
elsewhere: the `GoveeBackend` trait, `DeviceRegistry`, typed errors, and a workflow engine. The
crate name `govee` appears unclaimed on crates.io as of March 2026.

---

## 3. Architecture

### 3.1 Crate structure

```
govee/
├── Cargo.toml
├── src/
│   ├── lib.rs              ← public API surface
│   ├── error.rs            ← GoveeError, Result<T>
│   ├── types.rs            ← Device, DeviceId, DeviceState, Color, BackendType
│   ├── backend/
│   │   ├── mod.rs          ← GoveeBackend trait, BackendSelector
│   │   ├── cloud.rs        ← CloudBackend (reqwest, v1 + v2)
│   │   └── local.rs        ← LocalBackend (tokio UDP, multicast discovery)
│   ├── registry.rs         ← DeviceRegistry
│   ├── scene.rs            ← built-in scenes + user scene loading
│   └── workflow/
│       ├── mod.rs          ← WorkflowEngine, run_workflow()
│       └── types.rs        ← Workflow, Step, Target (stub in v1)
└── tests/
    ├── cloud_mock.rs       ← wiremock-based cloud API tests
    └── local_mock.rs       ← UDP loopback tests
```

### 3.2 Component overview

```
┌──────────────────────────────────────────────────────────────┐
│                         govee crate                          │
│                                                              │
│  ┌────────────────────────────────────────────────────────┐  │
│  │                  DeviceRegistry                        │  │
│  │  name/alias → Device { id, model, backend, state }    │  │
│  │  background task: periodic state reconciliation        │  │
│  └───────────────────────┬────────────────────────────────┘  │
│                          │                                   │
│  ┌───────────────────────▼────────────────────────────────┐  │
│  │                  BackendSelector                       │  │
│  │        per-device: auto | cloud | local                │  │
│  └──────────┬─────────────────────────────┬──────────────┘  │
│             │                             │                  │
│  ┌──────────▼──────────┐     ┌────────────▼────────────┐    │
│  │    CloudBackend     │     │      LocalBackend        │    │
│  │  reqwest, v1 + v2   │     │  tokio UDP, multicast    │    │
│  └─────────────────────┘     └─────────────────────────┘    │
│                                                              │
│  ┌────────────────────────────────────────────────────────┐  │
│  │               WorkflowEngine  (stub v1)                │  │
│  │       YAML → timed sequence of device commands         │  │
│  └────────────────────────────────────────────────────────┘  │
│                                                              │
│  ┌────────────────────────────────────────────────────────┐  │
│  │                   SceneRegistry                        │  │
│  │      built-in presets + user-defined (TOML/YAML)       │  │
│  └────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────┘
         │ HTTPS                           │ UDP
  Govee Cloud API                Govee devices (LAN)
```

---

## 4. Public API

### 4.1 Core types

```rust
/// Opaque device identifier (wraps MAC address string internally).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceId(String);

/// A Govee device as seen by the library.
#[derive(Debug, Clone)]
pub struct Device {
    pub id: DeviceId,
    pub model: String,
    pub name: String,               // from cloud API or local discovery
    pub alias: Option<String>,      // user-defined, from config
    pub backend: BackendType,       // which backend is active for this device
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendType { Cloud, Local }

/// Point-in-time device state.
#[derive(Debug, Clone)]
pub struct DeviceState {
    pub on: bool,
    pub brightness: u8,             // 0–100
    pub color: Color,
    pub color_temp_kelvin: Option<u32>,
    pub stale: bool,                // true if served from cache
}

#[derive(Debug, Clone, Copy)]
pub struct Color { pub r: u8, pub g: u8, pub b: u8 }
```

### 4.2 GoveeBackend trait

```rust
#[async_trait]
pub trait GoveeBackend: Send + Sync {
    async fn list_devices(&self) -> Result<Vec<Device>>;
    async fn get_state(&self, id: &DeviceId) -> Result<DeviceState>;
    async fn set_power(&self, id: &DeviceId, on: bool) -> Result<()>;
    async fn set_brightness(&self, id: &DeviceId, value: u8) -> Result<()>;
    async fn set_color(&self, id: &DeviceId, color: Color) -> Result<()>;
    async fn set_color_temp(&self, id: &DeviceId, kelvin: u32) -> Result<()>;
    fn backend_type(&self) -> BackendType;
}
```

Consumers of the library (`govee-cli`, `govee-mcp`, etc.) program against this trait, not against
concrete backend types. This also makes testing straightforward — mock backends implement the
same trait.

### 4.3 DeviceRegistry

The `DeviceRegistry` is the primary entry point for library consumers. It owns both backends,
handles backend selection per device, and provides name/alias resolution.

```rust
pub struct DeviceRegistry { /* ... */ }

impl DeviceRegistry {
    /// Build from config. Performs cloud device list fetch + local UDP discovery.
    pub async fn new(config: &Config) -> Result<Self>;

    /// Resolve a human name or alias to a DeviceId.
    pub fn resolve(&self, name: &str) -> Result<DeviceId>;

    /// All known devices.
    pub fn devices(&self) -> Vec<Device>;

    /// Current state for a device (may be cached).
    pub async fn get_state(&self, id: &DeviceId) -> Result<DeviceState>;

    pub async fn set_power(&self, id: &DeviceId, on: bool) -> Result<()>;
    pub async fn set_brightness(&self, id: &DeviceId, value: u8) -> Result<()>;
    pub async fn set_color(&self, id: &DeviceId, color: Color) -> Result<()>;
    pub async fn set_color_temp(&self, id: &DeviceId, kelvin: u32) -> Result<()>;

    /// Apply a named scene to one device, a group, or all devices.
    pub async fn apply_scene(&self, scene: &str, target: Target) -> Result<()>;

    /// Report which backend is active per device.
    pub fn backend_status(&self) -> Vec<(Device, BackendType)>;
}
```

### 4.4 Configuration

```rust
pub struct Config {
    pub api_key: Option<String>,        // cloud API key; None = local-only mode
    pub backend: BackendPreference,     // Auto | CloudOnly | LocalOnly
    pub discovery_interval_secs: u64,  // default: 60
    pub aliases: HashMap<String, String>, // alias → canonical device name
    pub groups: HashMap<String, Vec<String>>, // group name → list of aliases/names
}

pub enum BackendPreference { Auto, CloudOnly, LocalOnly }
```

Config is loaded from `~/.config/govee/config.toml` by default, overridable by the caller.
Consumer binaries (CLI, MCP) may layer additional config sources (env vars, CLI flags) on top —
that is their responsibility, not the library's.

### 4.5 Scene registry

```rust
pub struct Scene {
    pub name: String,
    pub brightness: u8,
    pub color: SceneColor,
}

pub enum SceneColor {
    Rgb(Color),
    Temp(u32),  // Kelvin
}
```

Built-in scenes (compiled in):

| Name | Color | Brightness |
|---|---|---|
| `warm` | 2700K | 40% |
| `focus` | 5500K | 80% |
| `night` | Red (255, 0, 0) | 10% |
| `movie` | 2200K | 20% |
| `bright` | 6500K | 100% |

User scenes are loaded from config and extend (or override) the built-ins.

### 4.6 Workflow engine (stub in v1)

The workflow engine is exposed as a single async function. In v1, calling it returns an error
explaining it is not yet implemented. The function signature is stable — consumer crates can call
it, and the stub can be replaced with a real implementation without changing any caller.

```rust
pub async fn run_workflow(path: &Path, registry: &DeviceRegistry) -> Result<()> {
    let _ = (path, registry);
    Err(GoveeError::NotImplemented("workflow engine is not yet implemented".into()))
}
```

The YAML workflow format will be designed separately and implemented in a subsequent release.

---

## 5. Error handling

```rust
#[derive(Debug, thiserror::Error)]
pub enum GoveeError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("UDP error: {0}")]
    Udp(#[from] std::io::Error),

    #[error("API error {code}: {message}")]
    Api { code: u16, message: String },

    #[error("rate limited — retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },

    #[error("device not found: {0}")]
    DeviceNotFound(String),

    #[error("backend unavailable: {0}")]
    BackendUnavailable(String),

    #[error("discovery timeout")]
    DiscoveryTimeout,

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("not implemented: {0}")]
    NotImplemented(String),
}

pub type Result<T> = std::result::Result<T, GoveeError>;
```

The library uses `thiserror` for domain errors. Consumer binaries use `anyhow` at their own
boundaries. The library never panics on bad API responses — all error paths return `Result`.

---

## 6. Rust fit

### 6.1 Advantages

**Type safety at protocol boundaries.** Both APIs are JSON-heavy. `serde` with typed structs
catches shape mismatches at compile time. Python or JS would fail silently at runtime.

**Async without cost.** `tokio` handles concurrent HTTP requests and UDP sockets in a single
thread pool. No GIL, no green thread overhead.

**UDP/multicast is well-supported.** `tokio::net::UdpSocket` with multicast join is clean. The
Python `asyncio` UDP story is historically awkward.

**Single-binary consumers.** Each binary crate that depends on `govee` compiles to a statically
linked binary. No runtime, no virtualenv, no version conflicts.

**Zero-cost abstractions.** The `GoveeBackend` trait dispatches at runtime (via `dyn` or enum
dispatch) with no framework overhead.

### 6.2 Disadvantages

**Slower iteration on protocol changes.** If Govee changes a JSON field name, a recompile is
required to pick up the fix. Acceptable — this is not a hot-reload environment.

**`async-trait` boilerplate.** Until async methods in traits stabilise fully in Rust, the
`async-trait` crate adds a proc macro layer. Minor ergonomic cost.

**No scripting.** The library cannot be used from a REPL or script without a compiled consumer.
The workflow engine (once implemented) partially addresses this for the common case.

### 6.3 Dependencies

| Crate | Role | Notes |
|---|---|---|
| `tokio` | Async runtime | `features = ["full"]` |
| `reqwest` | Cloud HTTP client | `features = ["json"]` |
| `serde` / `serde_json` | JSON serialization | — |
| `serde_yaml` | Workflow file parsing | deferred to workflow engine implementation |
| `thiserror` | Domain error types | — |
| `async-trait` | Async trait methods | until stabilisation |
| `tracing` | Structured logging | consumers attach subscribers |
| `toml` | Config file parsing | — |

Test-only:

| Crate | Role |
|---|---|
| `wiremock` | Mock cloud HTTP server for tests |
| `tokio::test` | Async test runner |

No heavy dependencies. Estimated clean build: ~45–60s. Incremental: ~5–10s.

---

## 7. Design discussions

### 7.1 Backend selection per device

**Option A: Global flag** (`cloud | local`). Simple, but breaks for mixed environments where some
devices don't support LAN.

**Option B: Per-device auto-selection (recommended).** At startup, run local discovery. Devices
that respond get `LocalBackend`; the rest get `CloudBackend`. The `--backend` flag overrides the
default for debugging or forced-cloud scenarios.

**Option C: Local preferred, cloud fallback on write failure.** Adds complexity for marginal gain
— local write failures are network topology issues, not transient errors cloud would recover.

Decision: **Option B**.

### 7.2 State management

**Option A: Live query on every `get_state`.** Accurate, slow on cloud, exposes all API
unreliability directly to callers.

**Option B: Optimistic in-memory cache (recommended).** After a successful write, update the
in-memory state immediately. `DeviceState.stale = false` for optimistically-updated state,
`stale = true` for state not yet reconciled with the device. Background task polls devices
periodically to reconcile.

**Option C: No state tracking.** Simpler to implement, but pushes the staleness problem to
every consumer.

Decision: **Option B**, with `stale: bool` in `DeviceState` to let consumers decide how to
present uncertainty.

### 7.3 Device identity and naming

MAC addresses are the stable identity used internally. The cloud API provides user-assigned names;
local discovery provides only MAC + IP. The `DeviceRegistry` merges both at startup — cloud names
are the canonical human-readable identifiers, local discovery adds IP routing.

User-defined aliases in config are purely additive: they create additional lookup keys, they don't
replace the canonical name. Both `"H6078 Living Room"` and `"bedroom"` resolve to the same
`DeviceId`.

### 7.4 Concurrency and shared state

`DeviceRegistry` is wrapped in `Arc` and designed to be cheaply cloned and shared across async
tasks. Internal mutable state (cached device states) is protected by `RwLock`. The background
reconciliation task holds a `Weak<DeviceRegistry>` to avoid preventing shutdown.

### 7.5 Port conflict mitigation

The local LAN API protocol fixes UDP ports 4001–4003. Only one process per host IP may use the
local backend at a time. The library detects bind failure on port 4002 at startup and returns
`GoveeError::BackendUnavailable` rather than panicking, with a clear message naming the conflict.

In `Auto` mode, if local binding fails, the library logs a warning and falls back to cloud for all
devices rather than failing to start entirely.

### 7.6 Workflow engine scope (deferred)

The workflow format is deliberately left undesigned for v1. The requirements are clear in outline —
timed sequences of device commands, multi-device targets, possibly conditional branches — but the
YAML schema deserves its own design document informed by real usage of the CLI. The v1 stub
preserves the call site so consumers don't need to change when the engine ships.

---

## Appendix: Network requirements for local backend

| Port | Direction | Purpose |
|---|---|---|
| UDP 4001 | outbound (multicast) | Discovery broadcast |
| UDP 4002 | inbound | Discovery responses from devices |
| UDP 4003 | outbound (unicast) | Control commands to devices |

Multicast group: `239.255.255.250`. The host must be on the same L2 segment as the Govee devices,
or multicast routing must be configured. Docker containers require `network_mode: host` or a
macvlan network.

Running the library alongside Home Assistant's Govee local integration, Homebridge's Govee plugin,
or `govee2mqtt` from the same host IP will cause a port 4002 bind conflict. The library detects
this and reports it clearly.
