# Security

This document describes the known security properties, limitations, and threat
model for the `govee` library. Some issues are inherent to the Govee platform
and cannot be resolved at the library level.

---

## Trust boundaries

The library interacts with four trust boundaries:

| Boundary | Trust level | Notes |
|----------|-------------|-------|
| **Govee cloud API** | Remote, authenticated | HTTPS with system CA; API key sent in every request header |
| **LAN network** | Local, unauthenticated | Plaintext UDP; any host on the LAN segment can participate |
| **Configuration file** | Local filesystem | Controls API key, backend selection, `base_url` override |
| **Caller (library consumer)** | In-process | Full access to all library types; responsible for input validation at the application boundary |

The library assumes a **single-user, trusted-caller** model. It does not
enforce access control between callers, nor does it sandbox backends from
each other. If the caller is compromised, all backends and credentials are
exposed.

---

## Threat model

The `govee` library controls consumer smart lighting devices. The primary
assets are:

- **The Govee cloud API key** — grants full control over all registered
  devices in the account.
- **Physical device control** — the ability to turn devices on/off and
  change their state.

The library is designed for trusted environments: it is a single-user library
intended to run under the same account as its configuration file. It is not
designed for multi-tenant or adversarial-user scenarios.

### Threats per backend

#### Cloud backend

| Threat | Description |
|--------|-------------|
| **API key exposure** | Key leaked via logs, serialized config, or process memory dump grants full account control with no revocation mechanism |
| **Man-in-the-middle** | Attacker with a trusted CA (corporate proxy, compromised CA store) can intercept HTTPS traffic and capture the API key |
| **`base_url` redirection** | Attacker who controls the config file can redirect API calls (including the key header) to an arbitrary server |

#### LAN backend

| Threat | Description |
|--------|-------------|
| **Unauthenticated UDP** | Any host on the LAN can send control commands to any Govee device — no credentials required |
| **Device spoofing** | A malicious host can respond to multicast scans with crafted payloads, injecting fake devices into the registry |
| **State poisoning** | A malicious host can send unsolicited state-update packets, causing the library to cache incorrect device state |
| **Device enumeration** | Multicast scans reveal all Govee devices on the LAN, including firmware version strings useful for fingerprinting |

### Mitigation summary

What the library does:

- Redacts the API key from `Debug` output and error messages
- Warns at load time if the config file has overly broad permissions
- Enforces HTTPS for all cloud API requests (HTTP allowed only on loopback)
- Uses UDP source IP instead of payload-claimed IP for LAN device addresses
- Expires LAN device entries via TTL to limit the window for spoofed devices
- Restricts scene names to `[a-zA-Z0-9_-]` to prevent log injection
- Caps color temperature at 10000K to avoid undefined firmware behavior

What the library cannot mitigate:

- The Govee LAN protocol has no authentication or encryption (platform limitation)
- The Govee cloud API has no key rotation or revocation endpoint (platform limitation)
- An attacker with a trusted CA can intercept HTTPS traffic (OS/network-level issue)
- The API key is stored in memory in plaintext (inherent to the design)
- `Config`'s `Serialize` implementation does not redact the key (intended for config round-trips)

---

## Cloud API key

### Storage

The API key is read from the config file (`~/.config/govee/config.toml` by
convention). Keep this file readable only by the owning user (`chmod 600`).
The library warns at load time if the file has broader permissions.

The key is redacted from `Debug` output and never included in error messages.
Do not serialize `Config` to untrusted output — the `Serialize` implementation
is provided for config round-trips and does not redact the key.

### No revocation mechanism

The Govee v1 API does not provide a key rotation or revocation endpoint. A
compromised key grants full device control with no time limit and no way to
invalidate it short of regenerating it in the Govee Home app. This is a
**platform limitation**; it has been [filed with Govee](#govee-platform-issues).

### HTTPS enforcement

All cloud API requests use HTTPS. HTTP is rejected unless the target host is a
loopback address (to support local test servers such as wiremock). The `reqwest`
client uses the system CA bundle; there is no certificate pinning.

A MITM attacker who can install a trusted CA (e.g. a corporate proxy or
CA-installing malware) can intercept requests and capture the API key. Combined
with the absence of key revocation, this is a significant risk in managed
environments. Keep this in mind when deploying on corporate networks.

### `base_url` is a privileged parameter

`CloudBackend::new` accepts an optional `base_url` override (intended for test
servers). If an attacker can control the config file, they can redirect all API
calls — including the API key header — to an arbitrary HTTPS server. Never
derive `base_url` from untrusted input.

---

## Local LAN backend

### No authentication or encryption

The Govee LAN protocol is unauthenticated, unencrypted UDP. This is a
**platform limitation** with no workaround at the library level.

Consequences:

- **Any host on the same LAN can control any Govee device directly** by
  sending UDP commands to port 4003 — regardless of whether this library is
  running.
- **Any host on the same LAN can enumerate all Govee devices** by sending a
  multicast scan to `239.255.255.250:4001` and observing responses. Scan
  responses also include firmware version strings (`bleVersionHard`,
  `wifiVersionSoft`, etc.), which enable device fingerprinting.
- **Scan responses are not authenticated.** A host on the LAN can send a
  crafted scan response to inject a fake device into the registry. The library
  mitigates this by using the UDP source IP rather than the IP claimed in the
  response payload, and by expiring device entries via TTL, but it cannot
  prevent MAC address spoofing in the payload.

This has been [filed with Govee](#govee-platform-issues). The fix requires
Govee to add per-device authentication to the LAN protocol (similar to the
username/token model used by Philips Hue).

**Recommendation**: Run the local backend only on networks you control and
trust. Do not expose UDP port 4002 to untrusted network segments.

---

## Auto-mode backend fallback (RT-M07-03)

When backend preference is `Auto`, the library attempts to use the device's
primary backend (typically local for devices discovered on the LAN, cloud
otherwise). If the primary backend call fails, the library automatically
retries the command on the other backend.

**Trust boundary implication:** If the primary backend is cloud and the
fallback is local, the command is sent over the unauthenticated, unencrypted
LAN protocol (see "Local LAN backend" above). This means:

- A cloud-to-local fallback inherits the LAN trust model: any host on the
  same network can observe and interfere with the fallback command.
- The library logs a warning with the device ID and original error whenever
  a fallback occurs, so operators can detect unexpected backend switches.

If strict cloud-only or local-only behavior is required, use `CloudOnly` or
`LocalOnly` backend preference — these modes never fall back.

---

## `apply_scene` scope

`apply_scene` with `SceneTarget::All` sends commands to every device in the
registry concurrently. For cloud-backend devices, this also multiplies the
API call count (`2 × N_devices`), which can exhaust the Govee v1 rate limit
for several minutes.

Callers that expose `apply_scene` via an external interface (HTTP, RPC, etc.)
should be careful about who can invoke the `All` target.

---

## Partial failure and device state

`apply_scene` (and group operations generally) send commands to multiple
devices concurrently. If a command fails mid-way — for example, if the color
command succeeds but the brightness command fails — the device is reported as
failed in the returned `PartialFailure` error, but **the partial state change
is not rolled back**. The device may remain in an intermediate state. There is
no automatic retry.

---

## Govee platform issues

The following limitations are inherent to the Govee platform and have been
reported upstream. They are tracked here for transparency.

| Issue | Description | Status |
|-------|-------------|--------|
| LAN-01 | LAN protocol has no authentication — any LAN host can control devices or inject fake responses | Filed |
| LAN-02 | LAN protocol has no encryption — traffic is observable on the LAN | Filed |
| API-01 | Cloud API key has no rotation or revocation endpoint | Filed |

---

## Reporting vulnerabilities in this library

If you discover a security issue in the `govee` library itself (not in the
Govee platform), please open a [GitHub issue](https://github.com/wkusnierczyk/govee/issues)
with the `security` label. For sensitive issues, use GitHub's private
vulnerability reporting.
