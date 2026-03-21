# M07 Security Audit

Audit date: 2026-03-20 (updated 2026-03-21 for Waves 2–4)
Branch: `main` (commit 6504b06 — Wave 4)
Auditor: automated review via Claude

---

## Checklist

### 1. Secrets handling (API key in memory, serialization, debug output)

**Status: Addressed**

- `Config::Debug` redacts the API key as `[REDACTED]` (`src/config.rs:254-257`).
- `Config::Serialize` writes `api_key` as `null` with comment "RT-01: never serialize the API key" (`src/config.rs:60-61`).
- `CloudBackend::Debug` redacts the API key (`src/backend/cloud.rs:449-456`).
- The API key is stored as a plain `String` in `Config.api_key` and `CloudBackend.api_key`. No zeroization on drop; the key may persist in freed memory. This is acceptable for a single-user CLI library but noted for completeness.
- The API key is never included in error messages or log output.

### 2. Input validation boundaries (all `pub` methods)

**Status: Addressed**

- `DeviceId::new` validates MAC format (6 or 8 hex octets, colon-separated) (`src/types.rs:52-65`).
- `DeviceState::new` validates brightness 0-100 (`src/types.rs:117-118`).
- `Config::new` and `Config::validate` check discovery interval >= 5s, scene name charset, scene brightness, color/color_temp mutual exclusivity, and color_temp range 1-10000 (`src/config.rs:111-161`).
- `Scene::new` validates name charset, brightness 0-100, color_temp 1-10000 (`src/scene.rs:43-65`).
- `CloudBackend::set_brightness` rejects > 100 (`src/backend/cloud.rs:413-415`).
- `LocalBackend::set_brightness` rejects > 100 (`src/backend/local.rs:487-489`).
- `CloudBackend::set_color_temp` and `LocalBackend::set_color_temp` reject 0 (`src/backend/cloud.rs:434-438`, `src/backend/local.rs:523-526`).
- Deserialization paths re-validate via custom `Deserialize` impls for `Config`, `DeviceState`, and `DeviceId`.

### 3. Dependency audit (cargo-deny, advisory DB)

**Status: Addressed**

- `deny.toml` is configured with:
  - `[advisories]` section with empty ignore list (all advisories active).
  - `[licenses]` allowlist (MIT, Apache-2.0, BSD-2/3, ISC, Unicode-3.0, OpenSSL, CDLA-Permissive-2.0).
  - `[bans]` wildcards denied, multiple versions warned.
  - `[sources]` unknown registry/git denied, only crates.io allowed.
- CI should run `cargo deny check` (not verified in this file-level audit).

### 4. LAN protocol threat surface (RT-02/RT-03)

**Status: Addressed (documented, mitigated where possible)**

- `LocalBackend` doc comment documents the unauthenticated plaintext UDP protocol and all attack vectors (RT-02, RT-03, RT-10, RT-11) (`src/backend/local.rs:62-90`).
- Source IP is used instead of payload IP to mitigate scan response IP spoofing (`src/backend/local.rs:311-323`).
- `validate_local_ip()` prevents commands from being sent to non-local IPs (`src/backend/local.rs:31-42`), called before every `send_command` and `get_state`.
- Device TTL expiry limits the duration of injected fake devices.
- `SECURITY.md` documents these as platform limitations.
- No authentication/encryption is possible without Govee protocol changes; this is a known platform limitation.

### 5. Config file permissions (RT-04)

**Status: Addressed**

- `Config::load` checks file permissions on Unix and warns if group/other bits are set (`src/config.rs:170-183`).
- Warning recommends `0600`.
- No enforcement (warning only) -- appropriate for a library (enforcement is the caller's responsibility).

### 6. User-Agent header injection (RT-M07-01, #73)

**Status: Addressed** (Wave 2, #93)

- `CloudBackend::new` now accepts an optional `user_agent: Option<String>` parameter (`src/backend/cloud.rs:112`).
- Validation rejects any string containing control characters (bytes < 0x20 or DEL 0x7f) and returns `GoveeError::InvalidConfig` (`src/backend/cloud.rs:114-120`).
- When `None`, the default `govee/{version}` string is used (compile-time constant, no injection vector).
- `build_client` passes the validated string directly to `reqwest`'s `user_agent()` builder (`src/backend/cloud.rs:56-61`).

### 7. Retry-after DoS vector (RT-M07-02, #34)

**Status: Addressed** (Wave 3, #99)

- `MAX_RETRY_AFTER_SECS = 300` is defined as a compile-time constant (`src/backend/cloud.rs:24`).
- The internal retry delay for `RateLimited` errors is capped at 300s via `.min(MAX_RETRY_AFTER_SECS)` in `retry_delay` (`src/backend/cloud.rs:266`).
- `parse_retry_after` still returns the raw parsed value and `GoveeError::RateLimited { retry_after_secs }` carries the uncapped value (so callers that naively sleep for `retry_after_secs` remain responsible for their own capping). The library's internal behavior is bounded.
- Residual risk: callers reading `GoveeError::RateLimited::retry_after_secs` directly and sleeping for that duration can still be DoS'd by a malicious server. This is documented as a caller responsibility.

### 8. Cross-trust-boundary fallback (RT-M07-03, #35)

**Status: Addressed** (Wave 4, #100)

- `DeviceRegistry` now attempts a fallback backend when the primary backend fails with a transport error (`BackendUnavailable`, `Request`, `DiscoveryTimeout`, `RateLimited`, `Api` 5xx, `Io`, `Protocol`) (`src/registry.rs:is_transport_error`).
- Fallback is gated on two conditions: (1) the error is a transport error (validation errors like `InvalidBrightness` are not retried), and (2) the device is known to be present on the fallback backend (tracked via `has_cloud`/`has_local` per `RegisteredDevice`).
- CloudOnly and LocalOnly modes never fall back.
- Trust boundary documentation added to `SECURITY.md` (§ "Auto-mode backend fallback"): a cloud-to-local fallback inherits the LAN trust model; commands are sent unencrypted over UDP; a `warn!` tracing event is emitted on every fallback so operators can detect unexpected backend switches.

### 9. Thundering herd on cache refresh (RT-M07-04, #71)

**Status: Not addressed**

- `reconciliation_loop` iterates all devices sequentially and queries each backend (`src/registry.rs:700-792`). This is sequential per device, so there is no thundering herd within the loop itself.
- `DeviceRegistry::get_state` checks cache first, then queries the backend on miss (`src/registry.rs:395-425`). Multiple concurrent callers could all miss the cache simultaneously and all query the backend, causing a burst. There is no lock or request coalescing to prevent duplicate backend queries for the same device.
- For typical single-user CLI usage this is not a practical concern, but for server-mode usage with many concurrent requests, request coalescing would be beneficial.

### 10. Group/device name log injection (RT-M07-05, #33)

**Status: Partially addressed**

- Scene names are validated to alphanumeric + `-` + `_` only (`src/scene.rs:56-64`), preventing log injection via scene names.
- Device names come from the Govee cloud API and are logged via `tracing::debug!`/`tracing::warn!` using structured fields (`device = %reg.device.id`, `name = %reg.device.name`). Structured logging (tracing) formats fields safely, preventing classic log injection (newline injection that creates fake log entries).
- However, device names are user-controlled strings from the Govee API. If a tracing subscriber uses an unstructured text formatter, special characters in device names could cause visual confusion (though not code injection).
- Group member names from config are also logged with `%member` formatting in warnings. Config values are locally controlled, so this is lower risk.
- No explicit sanitization is applied to device names or group names before logging; the mitigation relies entirely on `tracing`'s structured field formatting.

### 11. `base_url` SSRF (M06 RT-09)

**Status: Addressed**

- `CloudBackend::new` validates `base_url`: it must be a valid URL, must use HTTPS unless the host is a loopback address (`src/backend/cloud.rs:94-102`).
- The `is_loopback` function checks for `localhost`, `127.0.0.1`, and `[::1]`, plus any IP that `IpAddr::is_loopback()` returns true for (`src/backend/cloud.rs:33-46`).
- HTTPS enforcement prevents credential exfiltration to arbitrary HTTP endpoints.
- The doc comment explicitly warns that `base_url` is a privileged parameter and must not be derived from untrusted input (`src/backend/cloud.rs:92-93`).
- `SECURITY.md` documents this as well.

---

## Summary

| # | Item | Status |
|---|------|--------|
| 1 | Secrets handling | Addressed |
| 2 | Input validation boundaries | Addressed |
| 3 | Dependency audit (cargo-deny) | Addressed |
| 4 | LAN protocol threat surface | Addressed |
| 5 | Config file permissions | Addressed |
| 6 | User-Agent header injection | Addressed (Wave 2: optional custom UA with control-char validation) |
| 7 | Retry-after DoS vector | Addressed (Wave 3: internal retry capped at 300s; raw value still in error) |
| 8 | Cross-trust-boundary fallback | Addressed (Wave 4: transport-error gated fallback; trust boundary documented) |
| 9 | Thundering herd on cache refresh | Not addressed (sequential reconciliation; no request coalescing) |
| 10 | Group/device name log injection | Partially addressed (tracing structured fields; no explicit sanitization) |
| 11 | `base_url` SSRF | Addressed |
