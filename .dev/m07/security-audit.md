# M07 Security Audit

Audit date: 2026-03-20
Branch: `main` (commit f4d4e91)
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

**Status: Not addressed**

- `user_agent()` returns `govee/{version}` using `env!("CARGO_PKG_VERSION")` (`src/backend/cloud.rs:23-25`).
- The User-Agent is built from a compile-time constant, so there is no injection vector from runtime input.
- No custom User-Agent override is exposed to callers, so no validation is needed.
- If a future API adds user-configurable User-Agent suffix, validation would be needed. Currently safe by construction.

### 7. Retry-after DoS vector (RT-M07-02, #34)

**Status: Not addressed**

- `parse_retry_after` parses the `Retry-After` header as `u64` with a fallback of 60s (`src/backend/cloud.rs:313-320`).
- There is no upper-bound cap on the parsed value. A malicious or misconfigured server could return `Retry-After: 999999999` and the library would report `retry_after_secs: 999999999` to the caller.
- The library does not automatically retry or sleep on rate limits -- it returns `GoveeError::RateLimited` to the caller, so the caller controls the backoff behavior. The DoS vector is mitigated by the library not acting on the value automatically, but callers that naively sleep for `retry_after_secs` would be vulnerable.
- A cap (e.g., 3600s) would be a defense-in-depth improvement.

### 8. Cross-trust-boundary fallback (RT-M07-03, #35)

**Status: Not addressed**

- In `DeviceRegistry::start`, when `BackendPreference::Auto` is set and cloud `list_devices` fails, the error is logged and cloud devices are skipped (`src/registry.rs:121-130`). The same pattern applies to local (`src/registry.rs:131-141`).
- There is no explicit cross-trust-boundary fallback logic. If a backend fails at list time, its devices are simply absent. There is no runtime fallback from cloud to local or vice versa for individual commands.
- `backend_for()` returns the statically assigned backend per device; it does not attempt a fallback if that backend fails.
- This is a design choice (fail-fast per device) rather than a vulnerability, but it means a cloud API outage makes cloud-only devices unreachable even if they could theoretically be controlled via LAN.

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
| 6 | User-Agent header injection | Not addressed (safe by construction; no runtime input) |
| 7 | Retry-after DoS vector | Not addressed (no cap; mitigated by caller-controlled backoff) |
| 8 | Cross-trust-boundary fallback | Not addressed (design choice: fail-fast) |
| 9 | Thundering herd on cache refresh | Not addressed (sequential reconciliation; no request coalescing) |
| 10 | Group/device name log injection | Partially addressed (tracing structured fields; no explicit sanitization) |
| 11 | `base_url` SSRF | Addressed |
