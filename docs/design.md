# `govee` — Library Design Document

The `govee` crate is a Rust library for controlling Govee smart lighting devices. It provides
idiomatic async access to both the Govee cloud API and the local LAN API, a unified backend
abstraction, device registry, and a workflow engine for scripting complex multi-device behaviours.

> Up to date with version 0.4.0


## Table of Contents

- [References](#references)
- [1. Govee APIs](#1-govee-apis)
  - [1.1 Cloud API](#11-cloud-api)
  - [1.2 Local LAN API](#12-local-lan-api)
  - [1.3 API Comparison](#13-api-comparison)
- [2. Existing Crates](#2-existing-crates)
- [3. Architecture](#3-architecture)
  - [3.1 Crate structure](#31-crate-structure)
  - [3.2 Component overview](#32-component-overview)
- [4. Public API](#4-public-api)
  - [4.1 Core types](#41-core-types)
  - [4.2 GoveeBackend trait](#42-goveebackend-trait)
  - [4.3 DeviceRegistry](#43-deviceregistry)
  - [4.4 Configuration](#44-configuration)
  - [4.5 Scene registry](#45-scene-registry)
  - [4.6 Workflow engine [stub in v1]](#46-workflow-engine-stub-in-v1)
- [5. Error handling](#5-error-handling)
- [6. Rust fit](#6-rust-fit)
  - [6.1 Advantages](#61-advantages)
  - [6.2 Disadvantages](#62-disadvantages)
  - [6.3 Dependencies](#63-dependencies)
- [7. Design discussions](#7-design-discussions)
  - [7.1 Backend selection per device](#71-backend-selection-per-device)
  - [7.2 State management](#72-state-management)
  - [7.3 Device identity and naming](#73-device-identity-and-naming)
  - [7.4 Concurrency and shared state](#74-concurrency-and-shared-state)
  - [7.5 Port conflict mitigation](#75-port-conflict-mitigation)
  - [7.6 Workflow engine scope [deferred]](#76-workflow-engine-scope-deferred)
- [Appendix A: Network requirements for local backend](#appendix-a-network-requirements-for-local-backend)
- [Appendix B: Supported devices](#appendix-b-supported-devices)

---

## References

| Resource | URL |
|---|---|
| Govee Developer Platform | https://developer.govee.com/ |
| Cloud API v1 Reference | https://developer.govee.com/reference |
| Supported Product Models | https://developer.govee.com/docs/support-product-model |
| LAN API User Manual | https://app-h5.govee.com/user-manual/wlan-guide |
| Govee Developer API v2 Reference (PDF) | https://govee-public.s3.amazonaws.com/developer-docs/GoveeDeveloperAPIReference.pdf |

---

## 1. Govee APIs
<sub>[↑ TOC](#table-of-contents) · [1.1 Cloud API →](#11-cloud-api)</sub>


Govee exposes two independent control planes: a cloud REST API and a local LAN API over UDP. They
are not mirrors of each other — they differ in authentication model, latency, feature surface, and
reliability characteristics.

### 1.1 Cloud API
<sub>[↑ TOC](#table-of-contents) · [← 1. Govee APIs](#1-govee-apis) · [1.2 Local LAN API →](#12-local-lan-api)</sub>


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
<sub>[↑ TOC](#table-of-contents) · [← 1.1 Cloud API](#11-cloud-api) · [1.3 API Comparison →](#13-api-comparison)</sub>


Communicates directly with devices on the same LAN segment over UDP. Must be explicitly enabled
per device in the Govee Home app (Settings → LAN Control).

**Devices tested for this project:** H6076, H6078, H6079. See [Appendix B](#appendix-b-supported-devices)
for the full list of devices supporting cloud and LAN APIs.

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
<sub>[↑ TOC](#table-of-contents) · [← 1.2 Local LAN API](#12-local-lan-api) · [2. Existing Crates →](#2-existing-crates)</sub>


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
<sub>[↑ TOC](#table-of-contents) · [← 1.3 API Comparison](#13-api-comparison) · [3. Architecture →](#3-architecture)</sub>


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
<sub>[↑ TOC](#table-of-contents) · [← 2. Existing Crates](#2-existing-crates) · [3.1 Crate structure →](#31-crate-structure)</sub>


### 3.1 Crate structure
<sub>[↑ TOC](#table-of-contents) · [← 3. Architecture](#3-architecture) · [3.2 Component overview →](#32-component-overview)</sub>


```
govee/
├── Cargo.toml
├── src/
│   ├── lib.rs              ← public API surface (re-exports modules)
│   ├── error.rs            ← GoveeError (#[non_exhaustive]), Result<T>
│   ├── types.rs            ← Device, DeviceId, DeviceState, Color, BackendType
│   ├── config.rs           ← Config, BackendPreference, validation
│   ├── backend/
│   │   ├── mod.rs          ← GoveeBackend trait
│   │   ├── cloud.rs        ← CloudBackend (reqwest, v1)
│   │   └── local.rs        ← LocalBackend (tokio UDP, multicast discovery)
│   ├── registry.rs         ← DeviceRegistry [stub]
│   ├── scene.rs            ← Scene [stub]
│   └── workflow/
│       ├── mod.rs          ← (re-exports types)
│       └── types.rs        ← Workflow, Step, Target [stubs]
└── tests/
    ├── cloud_mock.rs       ← wiremock-based cloud API tests
    └── local_mock.rs       ← UDP loopback tests
```

### 3.2 Component overview
<sub>[↑ TOC](#table-of-contents) · [← 3.1 Crate structure](#31-crate-structure) · [4. Public API →](#4-public-api)</sub>


```
┌──────────────────────────────────────────────────────────────┐
│                         govee crate                          │
│                                                              │
│  ┌────────────────────────────────────────────────────────┐  │
│  │              DeviceRegistry  [stub]                    │  │
│  │  name/alias → Device { id, model, backend, state }     │  │
│  │  background task: periodic state reconciliation        │  │
│  └───────────────────────┬────────────────────────────────┘  │
│                          │                                   │
│  ┌───────────────────────▼────────────────────────────────┐  │
│  │                  BackendSelector                       │  │
│  │        per-device: auto | cloud | local                │  │
│  └──────────┬─────────────────────────────┬───────────────┘  │
│             │                             │                  │
│  ┌──────────▼──────────┐     ┌────────────▼────────────┐     │
│  │    CloudBackend     │     │      LocalBackend       │     │
│  │   reqwest, v1       │     │  tokio UDP, multicast   │     │
│  │   ✓ implemented     │     │  socket2, cancellation  │     │
│  └─────────────────────┘     │  ✓ implemented          │     │
│                              └─────────────────────────┘     │
│  ┌────────────────────────────────────────────────────────┐  │
│  │            Config  ✓ implemented                       │  │
│  │  TOML loading, validation, API key redaction           │  │
│  └────────────────────────────────────────────────────────┘  │
│                                                              │
│  ┌────────────────────────────────────────────────────────┐  │
│  │            WorkflowEngine  [stub]                      │  │
│  │       YAML → timed sequence of device commands         │  │
│  └────────────────────────────────────────────────────────┘  │
│                                                              │
│  ┌────────────────────────────────────────────────────────┐  │
│  │              SceneRegistry  [stub]                     │  │
│  │      built-in presets + user-defined (TOML/YAML)       │  │
│  └────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────┘
         │ HTTPS                           │ UDP
  Govee Cloud API (v1)          Govee devices (LAN)
```

---

## 4. Public API
<sub>[↑ TOC](#table-of-contents) · [← 3.2 Component overview](#32-component-overview) · [4.1 Core types →](#41-core-types)</sub>


### 4.1 Core types
<sub>[↑ TOC](#table-of-contents) · [← 4. Public API](#4-public-api) · [4.2 GoveeBackend trait →](#42-goveebackend-trait)</sub>


```rust
/// Opaque device identifier (wraps MAC address string internally).
/// Validates colon-separated hex MAC addresses with 6 or 8 octets.
/// Normalises to uppercase on construction.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DeviceId(pub(crate) String);

impl DeviceId {
    pub fn new(mac: &str) -> Result<Self>;   // validates format
    pub fn as_str(&self) -> &str;
}
// Also implements: Display, FromStr, TryFrom<String>, From<DeviceId> for String

/// A Govee device as seen by the library.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: DeviceId,
    pub model: String,
    pub name: String,               // from cloud API or local discovery
    pub alias: Option<String>,      // user-defined, from config
    pub backend: BackendType,       // which backend is active for this device
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendType { Cloud, Local }
// Also implements: Display

/// Point-in-time device state.
/// Constructed via `DeviceState::new()`, which validates brightness is 0–100.
/// Custom `Deserialize` impl also validates on deserialization.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceState {
    pub on: bool,
    pub brightness: u8,             // 0–100 (validated)
    pub color: Color,
    pub color_temp_kelvin: Option<u32>,
    pub stale: bool,                // true if served from cache
}

impl DeviceState {
    pub fn new(on: bool, brightness: u8, color: Color,
               color_temp_kelvin: Option<u32>, stale: bool) -> Result<Self>;
}

/// RGB color value (sRGB, each component 0–255).
/// Display format: `#RRGGBB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Color { pub r: u8, pub g: u8, pub b: u8 }

impl Color {
    pub fn new(r: u8, g: u8, b: u8) -> Self;
}
```

### 4.2 GoveeBackend trait
<sub>[↑ TOC](#table-of-contents) · [← 4.1 Core types](#41-core-types) · [4.3 DeviceRegistry →](#43-deviceregistry)</sub>


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

### 4.3 DeviceRegistry [stub]
<sub>[↑ TOC](#table-of-contents) · [← 4.2 GoveeBackend trait](#42-goveebackend-trait) · [4.4 Configuration →](#44-configuration)</sub>


> **Status:** Stub only as of v0.4.0. The struct is declared as `#[non_exhaustive]` with no
> fields or methods yet. The design below is the planned API.

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
<sub>[↑ TOC](#table-of-contents) · [← 4.3 DeviceRegistry](#43-deviceregistry) · [4.5 Scene registry →](#45-scene-registry)</sub>


```rust
/// Fields are private; accessed via getter methods.
/// Custom Debug impl redacts api_key as "[REDACTED]".
pub struct Config { /* ... */ }

pub const MIN_DISCOVERY_INTERVAL_SECS: u64 = 5;

impl Config {
    /// Validates discovery_interval_secs >= 5.
    pub fn new(api_key: Option<String>, backend: BackendPreference,
               discovery_interval_secs: u64, aliases: HashMap<String, String>,
               groups: HashMap<String, Vec<String>>) -> Result<Self>;

    pub fn load(path: &Path) -> Result<Self>;    // TOML file
    pub fn validate(&self) -> Result<()>;

    pub fn api_key(&self) -> Option<&str>;
    pub fn backend(&self) -> BackendPreference;
    pub fn discovery_interval_secs(&self) -> u64;  // default: 60
    pub fn aliases(&self) -> &HashMap<String, String>;
    pub fn groups(&self) -> &HashMap<String, Vec<String>>;
}

#[derive(Default)]
pub enum BackendPreference {
    #[default]
    Auto,
    CloudOnly,
    LocalOnly,
}
```

Config is loaded from `~/.config/govee/config.toml` by default, overridable by the caller.
Consumer binaries (CLI, MCP) may layer additional config sources (env vars, CLI flags) on top —
that is their responsibility, not the library's. The custom `Deserialize` implementation validates
on deserialization, so invalid TOML files fail early with `GoveeError::InvalidConfig`.

### 4.5 Scene registry [stub]
<sub>[↑ TOC](#table-of-contents) · [← 4.4 Configuration](#44-configuration) · [4.6 Workflow engine [stub in v1] →](#46-workflow-engine-stub-in-v1)</sub>


> **Status:** Stub only as of v0.4.0. The `Scene` struct is declared as `#[non_exhaustive]`
> with no fields or methods yet. The design below is the planned API.

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

### 4.6 Workflow engine [stub]
<sub>[↑ TOC](#table-of-contents) · [← 4.5 Scene registry](#45-scene-registry) · [5. Error handling →](#5-error-handling)</sub>


> **Status:** Stub types only as of v0.4.0. `Workflow`, `Step`, and `Target` are declared as
> `#[non_exhaustive]` structs with no fields or methods. The `run_workflow()` function is not
> yet exposed. The design below is the planned API.

```rust
pub async fn run_workflow(path: &Path, registry: &DeviceRegistry) -> Result<()>;
```

The YAML workflow format will be designed separately and implemented in a subsequent release.

---

## 5. Error handling
<sub>[↑ TOC](#table-of-contents) · [← 4.6 Workflow engine [stub in v1]](#46-workflow-engine-stub-in-v1) · [6. Rust fit →](#6-rust-fit)</sub>


```rust
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GoveeError {
    #[error("request error: {0}")]
    Request(#[from] reqwest::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

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

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("config error: {0}")]
    Config(#[from] toml::de::Error),

    #[error("not implemented: {0}")]
    NotImplemented(String),

    #[error("invalid device ID: {0}")]
    InvalidDeviceId(String),

    #[error("brightness must be 0–100, got {0}")]
    InvalidBrightness(u8),

    #[error("invalid config: {0}")]
    InvalidConfig(String),
}

pub type Result<T> = std::result::Result<T, GoveeError>;
```

The enum is `#[non_exhaustive]` so new variants can be added without breaking downstream matches.
The library uses `thiserror` for domain errors. Consumer binaries use `anyhow` at their own
boundaries. The library never panics on bad API responses — all error paths return `Result`.

---

## 6. Rust fit
<sub>[↑ TOC](#table-of-contents) · [← 5. Error handling](#5-error-handling) · [6.1 Advantages →](#61-advantages)</sub>


### 6.1 Advantages
<sub>[↑ TOC](#table-of-contents) · [← 6. Rust fit](#6-rust-fit) · [6.2 Disadvantages →](#62-disadvantages)</sub>


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
<sub>[↑ TOC](#table-of-contents) · [← 6.1 Advantages](#61-advantages) · [6.3 Dependencies →](#63-dependencies)</sub>


**Slower iteration on protocol changes.** If Govee changes a JSON field name, a recompile is
required to pick up the fix. Acceptable — this is not a hot-reload environment.

**`async-trait` boilerplate.** Until async methods in traits stabilise fully in Rust, the
`async-trait` crate adds a proc macro layer. Minor ergonomic cost.

**No scripting.** The library cannot be used from a REPL or script without a compiled consumer.
The workflow engine (once implemented) partially addresses this for the common case.

### 6.3 Dependencies
<sub>[↑ TOC](#table-of-contents) · [← 6.2 Disadvantages](#62-disadvantages) · [7. Design discussions →](#7-design-discussions)</sub>


| Crate | Role | Notes |
|---|---|---|
| `tokio` | Async runtime | `features = ["full"]` |
| `reqwest` | Cloud HTTP client | `features = ["json"]` |
| `serde` / `serde_json` | JSON serialization | — |
| `thiserror` | Domain error types | v2.0 |
| `async-trait` | Async trait methods | until stabilisation |
| `tracing` | Structured logging | consumers attach subscribers |
| `toml` | Config file parsing | — |
| `socket2` | Low-level socket options | `SO_REUSEADDR`, multicast join |
| `tokio-util` | `CancellationToken` | graceful background task shutdown |

Test-only:

| Crate | Role |
|---|---|
| `wiremock` | Mock cloud HTTP server for tests |
| `tokio::test` | Async test runner |

No heavy dependencies. `serde_yaml` will be added when the workflow engine is implemented.

---

## 7. Design discussions
<sub>[↑ TOC](#table-of-contents) · [← 6.3 Dependencies](#63-dependencies) · [7.1 Backend selection per device →](#71-backend-selection-per-device)</sub>


### 7.1 Backend selection per device
<sub>[↑ TOC](#table-of-contents) · [← 7. Design discussions](#7-design-discussions) · [7.2 State management →](#72-state-management)</sub>


**Option A: Global flag** (`cloud | local`). Simple, but breaks for mixed environments where some
devices don't support LAN.

**Option B: Per-device auto-selection (recommended).** At startup, run local discovery. Devices
that respond get `LocalBackend`; the rest get `CloudBackend`. The `--backend` flag overrides the
default for debugging or forced-cloud scenarios.

**Option C: Local preferred, cloud fallback on write failure.** Adds complexity for marginal gain
— local write failures are network topology issues, not transient errors cloud would recover.

Decision: **Option B**.

### 7.2 State management
<sub>[↑ TOC](#table-of-contents) · [← 7.1 Backend selection per device](#71-backend-selection-per-device) · [7.3 Device identity and naming →](#73-device-identity-and-naming)</sub>


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
<sub>[↑ TOC](#table-of-contents) · [← 7.2 State management](#72-state-management) · [7.4 Concurrency and shared state →](#74-concurrency-and-shared-state)</sub>


MAC addresses are the stable identity used internally. The cloud API provides user-assigned names;
local discovery provides only MAC + IP. The `DeviceRegistry` merges both at startup — cloud names
are the canonical human-readable identifiers, local discovery adds IP routing.

User-defined aliases in config are purely additive: they create additional lookup keys, they don't
replace the canonical name. Both `"H6078 Living Room"` and `"bedroom"` resolve to the same
`DeviceId`.

### 7.4 Concurrency and shared state
<sub>[↑ TOC](#table-of-contents) · [← 7.3 Device identity and naming](#73-device-identity-and-naming) · [7.5 Port conflict mitigation →](#75-port-conflict-mitigation)</sub>


`DeviceRegistry` is wrapped in `Arc` and designed to be cheaply cloned and shared across async
tasks. Internal mutable state (cached device states) is protected by `RwLock`. The background
reconciliation task holds a `Weak<DeviceRegistry>` to avoid preventing shutdown.

### 7.5 Port conflict mitigation
<sub>[↑ TOC](#table-of-contents) · [← 7.4 Concurrency and shared state](#74-concurrency-and-shared-state) · [7.6 Workflow engine scope [deferred] →](#76-workflow-engine-scope-deferred)</sub>


The local LAN API protocol fixes UDP ports 4001–4003. Only one process per host IP may use the
local backend at a time. The library detects bind failure on port 4002 at startup and returns
`GoveeError::BackendUnavailable` rather than panicking, with a clear message naming the conflict.

In `Auto` mode, if local binding fails, the library logs a warning and falls back to cloud for all
devices rather than failing to start entirely.

### 7.6 Workflow engine scope (deferred)
<sub>[↑ TOC](#table-of-contents) · [← 7.5 Port conflict mitigation](#75-port-conflict-mitigation) · [Appendix A: Network requirements →](#appendix-a-network-requirements-for-local-backend)</sub>


The workflow format is deliberately left undesigned for v1. The requirements are clear in outline —
timed sequences of device commands, multi-device targets, possibly conditional branches — but the
YAML schema deserves its own design document informed by real usage of the CLI. The v1 stub
preserves the call site so consumers don't need to change when the engine ships.

---

## Appendix A: Network requirements for local backend
<sub>[↑ TOC](#table-of-contents) · [← 7.6 Workflow engine scope [deferred]](#76-workflow-engine-scope-deferred) · [Appendix B: Supported devices →](#appendix-b-supported-devices)</sub>


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

---

## Appendix B: Supported devices
<sub>[↑ TOC](#table-of-contents) · [← Appendix A: Network requirements](#appendix-a-network-requirements-for-local-backend)</sub>

The cloud API (v1 and v2) supports any WiFi-connected Govee device registered to an account.
The local LAN API supports a subset of devices — those with the "LAN Control" toggle in the
Govee Home app. The authoritative list is maintained at
[developer.govee.com/docs/support-product-model](https://developer.govee.com/docs/support-product-model).

**Devices tested for this project:** H6076, H6078, H6079.

### Cloud API supported models

Source: [Govee Supported Product Models](https://developer.govee.com/docs/support-product-model)

**H5xxx series:**
H5051, H5071, H5080, H5081, H5082, H5083, H5086, H5100, H5103, H5127, H5160, H5161, H5179

**H60xx:**
H6002, H6003, H6004, H6006, H6008, H6009, H600A, H600D, H6010, H6011, H601A, H601B, H601C,
H601D, H6020, H6022, H6038, H6039, H6042, H6043, H6046, H6047, H6049, H604A, H604B, H604C,
H604D, H6050, H6051, H6052, H6054, H6056, H6057, H6058, H6059, H605A, H605B, H605C, H605D,
H6061, H6062, H6063, H6065, H6066, H6067, H6069, H606A, H6071, H6072, H6073, H6075, H6076,
H6078, H6079, H607C, H6085, H6086, H6087, H6088, H6089, H608A, H608B, H608C, H608D, H6091,
H6092, H6093, H6097, H6098, H6099, H60A0, H60A1

**H61xx:**
H6104, H6109, H610A, H610B, H6110, H6117, H611A, H611B, H611Z, H6121, H612A, H612B, H612C,
H612D, H612E, H612F, H6135, H6137, H613G, H6141, H6142, H6143, H6144, H6148, H6149, H614A,
H614B, H614C, H614E, H6154, H6159, H615A, H615B, H615C, H615D, H615E, H6160, H6163, H6167,
H6168, H6169, H6172, H6173, H6175, H6176, H6182, H6188, H618A, H618C, H618E, H618F, H6195,
H6198, H6199, H619A, H619B, H619C, H619D, H619E, H619Z, H61A0, H61A1, H61A2, H61A3, H61A5,
H61A8, H61A9, H61B1, H61B2, H61B3, H61B5, H61B6, H61BA, H61BC, H61BE, H61C2, H61C3, H61C5,
H61D3, H61D5, H61E0, H61E1, H61E5, H61E6, H61F2, H61F5, H61F6

**H66xx:**
H6601, H6602, H6603, H6604, H6608, H6609, H6611, H6630, H6631, H6640, H6641

**H68xx:**
H6800, H6810, H6811, H6820, H6821, H6822, H6840

**H70xx:**
H7005, H7007, H7008, H7012, H7013, H7014, H7020, H7021, H7022, H7028, H7031, H7032, H7033,
H7037, H7038, H7039, H703A, H703B, H7041, H7042, H7050, H7051, H7052, H7053, H7055, H7057,
H7058, H705A, H705B, H705C, H705D, H705E, H705F, H7060, H7061, H7062, H7063, H7065, H7066,
H7067, H7068, H7069, H706A, H706B, H706C, H7070, H7072, H7075, H7078, H7086, H70A1, H70A2,
H70A3, H70B1, H70B3, H70B4, H70B5, H70C1, H70C2, H70C4, H70C5, H70C6, H70C7, H70C8, H70C9,
H70CB, H70D1, H70D2, H70D3

**H71xx:**
H7100, H7101, H7102, H7103, H7106, H7111, H7112, H7120, H7121, H7122, H7123, H7124, H7126,
H7127, H7128, H7129, H712C, H7130, H7131, H7132, H7133, H7134, H7135, H7136, H7137, H7138,
H713A, H713B, H713C, H713D, H713E, H7140, H7141, H7142, H7143, H7145, H7147, H7148, H7149,
H714E, H7150, H7151, H7160, H7161, H7162, H7170, H7171, H7172, H7173, H7175, H7178, H717A,
H717C, H717D, H7184

**H8xxx:**
H801B, H801C, H8057, H805A, H805B, H805C, H8069, H8072, H8076, H807C, H808A, H80C4, H80D1

### LAN API supported models

The following models support the local LAN API (UDP multicast discovery and control). Each device
must have "LAN Control" enabled in the Govee Home app (Settings → LAN Control). Source:
[Home Assistant Govee Local integration](https://www.home-assistant.io/integrations/govee_light_local/).

**H60xx:**
H6008, H6020, H6022, H6039, H6042, H6046, H6047, H6048, H6051, H6052, H6056, H6059, H605D,
H6061, H6062, H6063, H6065, H6066, H6067, H6069, H606A, H6072, H6073, H6076, H6078, H6079,
H607C, H6087, H6088, H608A, H608B, H608D, H60A1, H60A4, H60A6, H60B1, H60B2

**H61xx:**
H610A, H610B, H6110, H6117, H612A, H612B, H612C, H612D, H612F, H6143, H6144, H6159, H615A,
H615B, H615C, H615D, H615E, H6163, H6167, H6168, H6169, H6172, H6173, H6175, H6176, H618A,
H618C, H618E, H618F, H619A, H619B, H619C, H619D, H619E, H619Z, H61A0, H61A1, H61A2, H61A3,
H61A5, H61A8, H61B2, H61B3, H61B5, H61B6, H61B9, H61BA, H61BC, H61BE, H61C2, H61C3, H61C5,
H61D3, H61D5, H61D6, H61E0, H61E1, H61E5, H61E6, H61F2, H61F5, H61F6

**H66xx:**
H6609, H6640, H6641

**H68xx:**
H6810, H6871

**H70xx:**
H7012, H7013, H7020, H7021, H7025, H7026, H7028, H702B, H7033, H7037, H7038, H7039, H7041,
H7042, H7050, H7051, H7052, H7053, H7055, H7057, H7058, H705A, H705B, H705C, H705D, H705E,
H705F, H7060, H7061, H7062, H7063, H7065, H7066, H7067, H706A, H706B, H706C, H7075, H7076,
H7086, H7093, H70A1, H70A2, H70A3, H70B1, H70B3, H70B6, H70BC, H70C1, H70C2, H70C4, H70C5,
H70D1

**H80xx:**
H8022, H805A, H805C, H8072, H80C5
