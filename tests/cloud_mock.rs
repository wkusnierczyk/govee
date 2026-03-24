//! Integration tests for CloudBackend using wiremock.

use govee::backend::GoveeBackend;
use govee::backend::cloud::CloudBackend;
use govee::error::GoveeError;
use govee::types::{Color, DeviceId};
use wiremock::matchers::{body_json, body_partial_json, header, method, path, query_param};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

/// Assert that a JSON request body contains a non-empty `requestId` string field.
///
/// Used in v2 API tests alongside `body_partial_json` to verify the request
/// envelope is complete even when `requestId` is a random UUID.
struct HasRequestId;

impl Match for HasRequestId {
    fn matches(&self, request: &Request) -> bool {
        let Ok(body) = serde_json::from_slice::<serde_json::Value>(&request.body) else {
            return false;
        };
        body.get("requestId")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty())
    }
}

/// Create a CloudBackend pointing at the mock server for both the legacy and new APIs.
///
/// `CloudBackend::new` allows HTTP for loopback addresses (wiremock binds to 127.0.0.1).
/// Both the legacy v1 base URL and the new OpenAPI base URL are pointed at the mock server
/// so that tests do not make real network requests. Unregistered paths return 404 from
/// wiremock, which causes `list_devices` to fall back to the legacy device list gracefully.
fn backend_for(server: &MockServer, api_key: &str) -> CloudBackend {
    CloudBackend::new(api_key.to_string(), Some(server.uri()), None)
        .unwrap()
        .with_new_api_base(&server.uri())
        .unwrap()
}

const HAPPY_RESPONSE: &str = r#"{
    "data": {
        "devices": [
            {
                "device": "AA:BB:CC:DD:EE:FF",
                "model": "H6076",
                "deviceName": "Kitchen Light",
                "controllable": true,
                "retrievable": true,
                "supportCmds": ["turn", "brightness", "color", "colorTem"]
            },
            {
                "device": "11:22:33:44:55:66",
                "model": "H6078",
                "deviceName": "Bedroom Strip",
                "controllable": true,
                "retrievable": true,
                "supportCmds": ["turn", "brightness", "color"]
            }
        ]
    },
    "code": 200,
    "message": "Success"
}"#;

#[tokio::test]
async fn list_devices_happy_path() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/devices"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(HAPPY_RESPONSE, "application/json"))
        .mount(&server)
        .await;

    let backend = backend_for(&server, "test-key");
    let devices = backend.list_devices().await.unwrap();

    assert_eq!(devices.len(), 2);
    assert_eq!(devices[0].id.as_str(), "AA:BB:CC:DD:EE:FF");
    assert_eq!(devices[0].model, "H6076");
    assert_eq!(devices[0].name, "Kitchen Light");
    assert_eq!(devices[1].id.as_str(), "11:22:33:44:55:66");
    assert_eq!(devices[1].model, "H6078");
}

#[tokio::test]
async fn requests_include_user_agent() {
    let server = MockServer::start().await;

    let expected_ua = format!("govee/{}", env!("CARGO_PKG_VERSION"));
    Mock::given(method("GET"))
        .and(path("/v1/devices"))
        .and(header("Govee-API-Key", "test-key"))
        .and(header("user-agent", expected_ua.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_raw(HAPPY_RESPONSE, "application/json"))
        .mount(&server)
        .await;

    let backend = backend_for(&server, "test-key");
    // If User-Agent doesn't match, wiremock returns 404 and the call fails.
    backend.list_devices().await.unwrap();
}

#[tokio::test]
async fn list_devices_auth_failure() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/devices"))
        .and(header("Govee-API-Key", "bad-key"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
        .mount(&server)
        .await;

    let backend = backend_for(&server, "bad-key");
    let result = backend.list_devices().await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    match &err {
        GoveeError::Api { code, message } => {
            assert_eq!(*code, 401);
            assert!(message.contains("Unauthorized"));
        }
        other => panic!("expected GoveeError::Api, got: {other:?}"),
    }
}

#[tokio::test]
async fn list_devices_malformed_response() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/devices"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("{not valid json", "application/json"),
        )
        .mount(&server)
        .await;

    let backend = backend_for(&server, "test-key");
    let result = backend.list_devices().await;

    assert!(result.is_err());
    // reqwest JSON parse error surfaces as GoveeError::Request
    assert!(matches!(result.unwrap_err(), GoveeError::Request(_)));
}

#[tokio::test]
async fn list_devices_empty() {
    let response = r#"{
        "data": { "devices": [] },
        "code": 200,
        "message": "Success"
    }"#;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/devices"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(response, "application/json"))
        .mount(&server)
        .await;

    let backend = backend_for(&server, "test-key");
    let devices = backend.list_devices().await.unwrap();
    assert!(devices.is_empty());
}

#[tokio::test]
async fn list_devices_api_error_code_in_body() {
    let response = r#"{
        "data": { "devices": [] },
        "code": 401,
        "message": "Invalid API key"
    }"#;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/devices"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(response, "application/json"))
        .mount(&server)
        .await;

    let backend = backend_for(&server, "test-key");
    let result = backend.list_devices().await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GoveeError::Api { code, message } => {
            assert_eq!(code, 401);
            assert_eq!(message, "Invalid API key");
        }
        other => panic!("expected GoveeError::Api, got: {other:?}"),
    }
}

#[tokio::test]
async fn list_devices_rate_limited() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/devices"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "120")
                .set_body_string("Too Many Requests"),
        )
        .mount(&server)
        .await;

    let backend = backend_for(&server, "test-key");
    let result = backend.list_devices().await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GoveeError::RateLimited { retry_after_secs } => {
            assert_eq!(retry_after_secs, 120);
        }
        other => panic!("expected GoveeError::RateLimited, got: {other:?}"),
    }
}

#[tokio::test]
async fn list_devices_rate_limited_no_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/devices"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(429).set_body_string("Too Many Requests"))
        .mount(&server)
        .await;

    let backend = backend_for(&server, "test-key");
    let result = backend.list_devices().await;

    match result.unwrap_err() {
        GoveeError::RateLimited { retry_after_secs } => {
            assert_eq!(retry_after_secs, 60); // default fallback
        }
        other => panic!("expected GoveeError::RateLimited, got: {other:?}"),
    }
}

// --- get_state tests ---

const STATE_RESPONSE: &str = r#"{
    "data": {
        "device": "AA:BB:CC:DD:EE:FF",
        "model": "H6076",
        "properties": [
            { "online": true },
            { "powerState": "on" },
            { "brightness": 75 },
            { "color": { "r": 255, "g": 128, "b": 0 } },
            { "colorTem": 5000 }
        ]
    },
    "code": 200,
    "message": "Success"
}"#;

/// Helper: mount list_devices mock and call it to populate device cache.
async fn populate_device_cache(server: &MockServer, backend: &CloudBackend) {
    Mock::given(method("GET"))
        .and(path("/v1/devices"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(HAPPY_RESPONSE, "application/json"))
        .mount(server)
        .await;
    backend.list_devices().await.unwrap();
}

#[tokio::test]
async fn get_state_happy_path() {
    let server = MockServer::start().await;
    let backend = backend_for(&server, "test-key");
    populate_device_cache(&server, &backend).await;

    Mock::given(method("GET"))
        .and(path("/v1/devices/state"))
        .and(header("Govee-API-Key", "test-key"))
        .and(query_param("device", "AA:BB:CC:DD:EE:FF"))
        .and(query_param("model", "H6076"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(STATE_RESPONSE, "application/json"))
        .mount(&server)
        .await;

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    let state = backend.get_state(&id).await.unwrap();
    assert!(state.on);
    assert_eq!(state.brightness, 75);
    assert_eq!(state.color, govee::types::Color::new(255, 128, 0));
    assert_eq!(state.color_temp_kelvin, Some(5000));
    assert!(!state.stale);
}

#[tokio::test]
async fn get_state_device_not_found_without_cache() {
    let server = MockServer::start().await;
    let backend = backend_for(&server, "test-key");

    // Auto-refresh will call list_devices — mock an empty device list.
    Mock::given(method("GET"))
        .and(path("/v1/devices"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "devices": [] },
            "message": "Success",
            "code": 200
        })))
        .mount(&server)
        .await;

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    let result = backend.get_state(&id).await;

    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), GoveeError::DeviceNotFound(_)));
}

#[tokio::test]
async fn get_state_stale_when_offline() {
    let offline_response = r#"{
        "data": {
            "device": "AA:BB:CC:DD:EE:FF",
            "model": "H6076",
            "properties": [
                { "online": false },
                { "powerState": "off" },
                { "brightness": 50 },
                { "color": { "r": 0, "g": 0, "b": 0 } }
            ]
        },
        "code": 200,
        "message": "Success"
    }"#;

    let server = MockServer::start().await;
    let backend = backend_for(&server, "test-key");
    populate_device_cache(&server, &backend).await;

    Mock::given(method("GET"))
        .and(path("/v1/devices/state"))
        .and(header("Govee-API-Key", "test-key"))
        .and(query_param("device", "AA:BB:CC:DD:EE:FF"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(offline_response, "application/json"))
        .mount(&server)
        .await;

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    let state = backend.get_state(&id).await.unwrap();
    assert!(state.stale);
    assert!(!state.on);
    assert_eq!(state.brightness, 50);
}

#[tokio::test]
async fn get_state_rate_limited() {
    let server = MockServer::start().await;
    let backend = backend_for(&server, "test-key");
    populate_device_cache(&server, &backend).await;

    Mock::given(method("GET"))
        .and(path("/v1/devices/state"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "30")
                .set_body_string("Too Many Requests"),
        )
        .mount(&server)
        .await;

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    let result = backend.get_state(&id).await;

    match result.unwrap_err() {
        GoveeError::RateLimited { retry_after_secs } => {
            assert_eq!(retry_after_secs, 30);
        }
        other => panic!("expected GoveeError::RateLimited, got: {other:?}"),
    }
}

// --- control command tests ---

const CONTROL_SUCCESS: &str = r#"{"code": 200, "message": "Success"}"#;

#[tokio::test]
async fn set_power_on() {
    let server = MockServer::start().await;
    let backend = backend_for(&server, "test-key");
    populate_device_cache(&server, &backend).await;

    Mock::given(method("PUT"))
        .and(path("/v1/devices/control"))
        .and(header("Govee-API-Key", "test-key"))
        .and(body_json(serde_json::json!({
            "device": "AA:BB:CC:DD:EE:FF",
            "model": "H6076",
            "cmd": { "name": "turn", "value": "on" }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_raw(CONTROL_SUCCESS, "application/json"))
        .mount(&server)
        .await;

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    backend.set_power(&id, true).await.unwrap();
}

#[tokio::test]
async fn set_power_off() {
    let server = MockServer::start().await;
    let backend = backend_for(&server, "test-key");
    populate_device_cache(&server, &backend).await;

    Mock::given(method("PUT"))
        .and(path("/v1/devices/control"))
        .and(header("Govee-API-Key", "test-key"))
        .and(body_json(serde_json::json!({
            "device": "AA:BB:CC:DD:EE:FF",
            "model": "H6076",
            "cmd": { "name": "turn", "value": "off" }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_raw(CONTROL_SUCCESS, "application/json"))
        .mount(&server)
        .await;

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    backend.set_power(&id, false).await.unwrap();
}

#[tokio::test]
async fn set_brightness_valid() {
    let server = MockServer::start().await;
    let backend = backend_for(&server, "test-key");
    populate_device_cache(&server, &backend).await;

    Mock::given(method("PUT"))
        .and(path("/v1/devices/control"))
        .and(header("Govee-API-Key", "test-key"))
        .and(body_json(serde_json::json!({
            "device": "AA:BB:CC:DD:EE:FF",
            "model": "H6076",
            "cmd": { "name": "brightness", "value": 75 }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_raw(CONTROL_SUCCESS, "application/json"))
        .mount(&server)
        .await;

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    backend.set_brightness(&id, 75).await.unwrap();
}

#[tokio::test]
async fn set_brightness_over_100_rejected() {
    let server = MockServer::start().await;
    let backend = backend_for(&server, "test-key");
    populate_device_cache(&server, &backend).await;

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    let result = backend.set_brightness(&id, 101).await;
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        GoveeError::InvalidBrightness(101)
    ));
}

#[tokio::test]
async fn set_color_rgb() {
    let server = MockServer::start().await;
    let backend = backend_for(&server, "test-key");
    populate_device_cache(&server, &backend).await;

    Mock::given(method("PUT"))
        .and(path("/v1/devices/control"))
        .and(header("Govee-API-Key", "test-key"))
        .and(body_json(serde_json::json!({
            "device": "AA:BB:CC:DD:EE:FF",
            "model": "H6076",
            "cmd": { "name": "color", "value": {"r": 255, "g": 0, "b": 128} }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_raw(CONTROL_SUCCESS, "application/json"))
        .mount(&server)
        .await;

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    backend
        .set_color(&id, Color::new(255, 0, 128))
        .await
        .unwrap();
}

#[tokio::test]
async fn set_color_temp_valid() {
    let server = MockServer::start().await;
    let backend = backend_for(&server, "test-key");
    populate_device_cache(&server, &backend).await;

    Mock::given(method("PUT"))
        .and(path("/v1/devices/control"))
        .and(header("Govee-API-Key", "test-key"))
        .and(body_json(serde_json::json!({
            "device": "AA:BB:CC:DD:EE:FF",
            "model": "H6076",
            "cmd": { "name": "colorTem", "value": 5000 }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_raw(CONTROL_SUCCESS, "application/json"))
        .mount(&server)
        .await;

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    backend.set_color_temp(&id, 5000).await.unwrap();
}

#[tokio::test]
async fn control_command_device_not_found() {
    let server = MockServer::start().await;
    let backend = backend_for(&server, "test-key");

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    let result = backend.set_power(&id, true).await;
    assert!(matches!(result.unwrap_err(), GoveeError::DeviceNotFound(_)));
}

#[tokio::test]
async fn control_command_rate_limited() {
    let server = MockServer::start().await;
    let backend = backend_for(&server, "test-key");
    populate_device_cache(&server, &backend).await;

    Mock::given(method("PUT"))
        .and(path("/v1/devices/control"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "1")
                .set_body_string("Too Many Requests"),
        )
        .mount(&server)
        .await;

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    let result = backend.set_power(&id, true).await;
    match result.unwrap_err() {
        GoveeError::RateLimited { retry_after_secs } => assert_eq!(retry_after_secs, 1),
        other => panic!("expected RateLimited, got: {other:?}"),
    }
}

#[tokio::test]
async fn control_command_api_error() {
    let server = MockServer::start().await;
    let backend = backend_for(&server, "test-key");
    populate_device_cache(&server, &backend).await;

    Mock::given(method("PUT"))
        .and(path("/v1/devices/control"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .mount(&server)
        .await;

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    let result = backend.set_brightness(&id, 50).await;
    match result.unwrap_err() {
        GoveeError::Api { code, .. } => assert_eq!(code, 500),
        other => panic!("expected Api error, got: {other:?}"),
    }
}

#[tokio::test]
async fn set_brightness_boundary_zero() {
    let server = MockServer::start().await;
    let backend = backend_for(&server, "test-key");
    populate_device_cache(&server, &backend).await;

    Mock::given(method("PUT"))
        .and(path("/v1/devices/control"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(CONTROL_SUCCESS, "application/json"))
        .mount(&server)
        .await;

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    backend.set_brightness(&id, 0).await.unwrap();
}

#[tokio::test]
async fn set_brightness_boundary_100() {
    let server = MockServer::start().await;
    let backend = backend_for(&server, "test-key");
    populate_device_cache(&server, &backend).await;

    Mock::given(method("PUT"))
        .and(path("/v1/devices/control"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(CONTROL_SUCCESS, "application/json"))
        .mount(&server)
        .await;

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    backend.set_brightness(&id, 100).await.unwrap();
}

#[tokio::test]
async fn control_command_api_error_in_body() {
    let error_response = r#"{"code": 400, "message": "Device offline"}"#;

    let server = MockServer::start().await;
    let backend = backend_for(&server, "test-key");
    populate_device_cache(&server, &backend).await;

    Mock::given(method("PUT"))
        .and(path("/v1/devices/control"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(error_response, "application/json"))
        .mount(&server)
        .await;

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    let result = backend.set_power(&id, true).await;
    match result.unwrap_err() {
        GoveeError::Api { code, message } => {
            assert_eq!(code, 400);
            assert_eq!(message, "Device offline");
        }
        other => panic!("expected Api error, got: {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn rate_limit_warning_logged() {
    use std::sync::{Arc, Mutex, OnceLock};
    use tracing_subscriber::layer::SubscriberExt;

    // Callsite interest is only rebuilt when a global dispatcher is registered.
    // Install an empty registry once so all callsites become "always interested",
    // enabling per-test thread-local subscribers to capture events.
    static GLOBAL_INIT: OnceLock<()> = OnceLock::new();
    GLOBAL_INIT.get_or_init(|| {
        let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry());
    });

    // Per-test subscriber installed on this thread only.
    let buf: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let buf2 = buf.clone();
    struct TestLayer(Arc<Mutex<Vec<String>>>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for TestLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut msg = String::new();
            event.record(
                &mut |field: &tracing::field::Field, value: &dyn std::fmt::Debug| {
                    if field.name() == "message" {
                        use std::fmt::Write;
                        let _ = write!(msg, "{value:?}");
                    }
                },
            );
            self.0.lock().unwrap().push(msg);
        }
    }
    let subscriber = tracing_subscriber::registry().with(TestLayer(buf2));
    let _guard = tracing::subscriber::set_default(subscriber);

    let server = MockServer::start().await;
    let backend = backend_for(&server, "test-key");
    populate_device_cache(&server, &backend).await;

    Mock::given(method("PUT"))
        .and(path("/v1/devices/control"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "1")
                .set_body_string("Too Many Requests"),
        )
        .mount(&server)
        .await;

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    let result = backend.set_power(&id, true).await;
    assert!(result.is_err());

    let logs = buf.lock().unwrap().join("\n");
    assert!(
        logs.contains("rate limited"),
        "expected 'rate limited' in logs, got: {logs}"
    );
}

#[tokio::test]
async fn set_color_temp_zero_rejected() {
    let server = MockServer::start().await;
    let backend = backend_for(&server, "test-key");
    populate_device_cache(&server, &backend).await;

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    let result = backend.set_color_temp(&id, 0).await;
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), GoveeError::InvalidConfig(_)));
}

// --- new_api_post and new_api_get tests ---

/// Build a CloudBackend whose new-API base URL points at the mock server.
fn new_api_backend_for(server: &MockServer, api_key: &str) -> CloudBackend {
    CloudBackend::new(api_key.to_string(), None, None)
        .unwrap()
        .with_new_api_base(&server.uri())
        .unwrap()
}

#[tokio::test]
async fn new_api_post_success() {
    let server = MockServer::start().await;

    let response_body = serde_json::json!({
        "requestId": "test-req-id",
        "msg": "success",
        "code": 200,
        "payload": { "value": 42 }
    });

    Mock::given(method("POST"))
        .and(path("/v2/test/endpoint"))
        .and(header("Govee-API-Key", "test-key"))
        // Assert the request envelope has a requestId field and the expected payload.
        .and(body_partial_json(serde_json::json!({
            "payload": { "cmd": "ping" }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
        .mount(&server)
        .await;

    let backend = new_api_backend_for(&server, "test-key");
    let result: serde_json::Value = backend
        .new_api_post("/v2/test/endpoint", serde_json::json!({"cmd": "ping"}))
        .await
        .unwrap();

    assert_eq!(result["value"], 42);
}

#[tokio::test]
async fn new_api_post_http_401() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v2/test/endpoint"))
        .and(header("Govee-API-Key", "bad-key"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
        .mount(&server)
        .await;

    let backend = new_api_backend_for(&server, "bad-key");
    let result: Result<serde_json::Value, _> = backend
        .new_api_post("/v2/test/endpoint", serde_json::json!({}))
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GoveeError::Api { code, message } => {
            assert_eq!(code, 401);
            assert!(message.contains("Unauthorized"), "message was: {message}");
        }
        other => panic!("expected GoveeError::Api(401), got: {other:?}"),
    }
}

#[tokio::test]
async fn new_api_post_http_429() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v2/test/endpoint"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(429).set_body_string("Too Many Requests"))
        .mount(&server)
        .await;

    let backend = new_api_backend_for(&server, "test-key");
    let result: Result<serde_json::Value, _> = backend
        .new_api_post("/v2/test/endpoint", serde_json::json!({}))
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GoveeError::RateLimited { retry_after_secs } => {
            assert_eq!(retry_after_secs, 60);
        }
        other => panic!("expected GoveeError::RateLimited, got: {other:?}"),
    }
}

#[tokio::test]
async fn new_api_post_http_400() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v2/test/endpoint"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(400).set_body_string("Bad Request"))
        .mount(&server)
        .await;

    let backend = new_api_backend_for(&server, "test-key");
    let result: Result<serde_json::Value, _> = backend
        .new_api_post("/v2/test/endpoint", serde_json::json!({}))
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GoveeError::Api { code, message } => {
            assert_eq!(code, 400);
            assert!(message.contains("Bad Request"), "message was: {message}");
        }
        other => panic!("expected GoveeError::Api(400), got: {other:?}"),
    }
}

#[tokio::test]
async fn new_api_post_envelope_code_400() {
    let server = MockServer::start().await;

    // HTTP 200 but the response envelope carries code 400.
    let response_body = serde_json::json!({
        "requestId": "test-req-id",
        "msg": "Device offline",
        "code": 400,
        "payload": {}
    });

    Mock::given(method("POST"))
        .and(path("/v2/test/endpoint"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
        .mount(&server)
        .await;

    let backend = new_api_backend_for(&server, "test-key");
    let result: Result<serde_json::Value, _> = backend
        .new_api_post("/v2/test/endpoint", serde_json::json!({}))
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GoveeError::Api { code, message } => {
            assert_eq!(code, 400);
            assert_eq!(message, "Device offline");
        }
        other => panic!("expected GoveeError::Api(400), got: {other:?}"),
    }
}

#[tokio::test]
async fn new_api_post_envelope_code_401() {
    let server = MockServer::start().await;

    let response_body = serde_json::json!({
        "requestId": "test-req-id",
        "msg": "Invalid API key",
        "code": 401,
        "payload": {}
    });

    Mock::given(method("POST"))
        .and(path("/v2/test/endpoint"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
        .mount(&server)
        .await;

    let backend = new_api_backend_for(&server, "test-key");
    let result: Result<serde_json::Value, _> = backend
        .new_api_post("/v2/test/endpoint", serde_json::json!({}))
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GoveeError::Api { code, message } => {
            assert_eq!(code, 401);
            assert_eq!(message, "Invalid API key");
        }
        other => panic!("expected GoveeError::Api(401), got: {other:?}"),
    }
}

#[tokio::test]
async fn new_api_post_envelope_code_404() {
    let server = MockServer::start().await;

    let response_body = serde_json::json!({
        "requestId": "test-req-id",
        "msg": "Device not found",
        "code": 404,
        "payload": {}
    });

    Mock::given(method("POST"))
        .and(path("/v2/test/endpoint"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
        .mount(&server)
        .await;

    let backend = new_api_backend_for(&server, "test-key");
    let result: Result<serde_json::Value, _> = backend
        .new_api_post("/v2/test/endpoint", serde_json::json!({}))
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GoveeError::Api { code, message } => {
            assert_eq!(code, 404);
            assert!(
                message.contains("Device not found"),
                "message was: {message}"
            );
        }
        other => panic!("expected GoveeError::Api(404), got: {other:?}"),
    }
}

#[tokio::test]
async fn new_api_post_envelope_code_429() {
    let server = MockServer::start().await;

    let response_body = serde_json::json!({
        "requestId": "test-req-id",
        "msg": "Too many requests",
        "code": 429,
        "payload": {}
    });

    Mock::given(method("POST"))
        .and(path("/v2/test/endpoint"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
        .mount(&server)
        .await;

    let backend = new_api_backend_for(&server, "test-key");
    let result: Result<serde_json::Value, _> = backend
        .new_api_post("/v2/test/endpoint", serde_json::json!({}))
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GoveeError::RateLimited { retry_after_secs } => {
            assert_eq!(retry_after_secs, 60);
        }
        other => panic!("expected GoveeError::RateLimited, got: {other:?}"),
    }
}

#[tokio::test]
async fn new_api_post_envelope_code_other() {
    let server = MockServer::start().await;

    // HTTP 200 but envelope carries an "other" code (503).
    let response_body = serde_json::json!({
        "requestId": "test-req-id",
        "msg": "Service unavailable",
        "code": 503,
        "payload": {}
    });

    Mock::given(method("POST"))
        .and(path("/v2/test/endpoint"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
        .mount(&server)
        .await;

    let backend = new_api_backend_for(&server, "test-key");
    let result: Result<serde_json::Value, _> = backend
        .new_api_post("/v2/test/endpoint", serde_json::json!({}))
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GoveeError::Api { code, message } => {
            assert_eq!(code, 503);
            assert_eq!(message, "Service unavailable");
        }
        other => panic!("expected GoveeError::Api(503), got: {other:?}"),
    }
}

#[tokio::test]
async fn new_api_post_http_404() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v2/test/endpoint"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
        .mount(&server)
        .await;

    let backend = new_api_backend_for(&server, "test-key");
    let result: Result<serde_json::Value, _> = backend
        .new_api_post("/v2/test/endpoint", serde_json::json!({}))
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GoveeError::Api { code, message } => {
            assert_eq!(code, 404);
            assert!(message.contains("Not Found"), "message was: {message}");
        }
        other => panic!("expected GoveeError::Api(404), got: {other:?}"),
    }
}

#[tokio::test]
async fn new_api_post_http_500() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v2/test/endpoint"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .mount(&server)
        .await;

    let backend = new_api_backend_for(&server, "test-key");
    let result: Result<serde_json::Value, _> = backend
        .new_api_post("/v2/test/endpoint", serde_json::json!({}))
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GoveeError::Api { code, message } => {
            assert_eq!(code, 500);
            assert!(
                message.contains("Internal Server Error"),
                "message was: {message}"
            );
        }
        other => panic!("expected GoveeError::Api(500), got: {other:?}"),
    }
}

#[tokio::test]
async fn new_api_post_http_503() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v2/test/endpoint"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(503).set_body_string("Service Unavailable"))
        .mount(&server)
        .await;

    let backend = new_api_backend_for(&server, "test-key");
    let result: Result<serde_json::Value, _> = backend
        .new_api_post("/v2/test/endpoint", serde_json::json!({}))
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GoveeError::Api { code, message } => {
            assert_eq!(code, 503);
            assert!(
                message.contains("Service Unavailable"),
                "message was: {message}"
            );
        }
        other => panic!("expected GoveeError::Api(503), got: {other:?}"),
    }
}

#[tokio::test]
async fn new_api_get_success() {
    let server = MockServer::start().await;

    let response_body = serde_json::json!({
        "requestId": "test-req-id",
        "msg": "success",
        "code": 200,
        "payload": { "count": 7 }
    });

    Mock::given(method("GET"))
        .and(path("/v2/test/resource"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
        .mount(&server)
        .await;

    let backend = new_api_backend_for(&server, "test-key");
    let result: serde_json::Value = backend
        .new_api_get("/v2/test/resource", None::<&()>)
        .await
        .unwrap();

    assert_eq!(result["count"], 7);
}

#[tokio::test]
async fn new_api_get_http_401() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v2/test/resource"))
        .and(header("Govee-API-Key", "bad-key"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
        .mount(&server)
        .await;

    let backend = new_api_backend_for(&server, "bad-key");
    let result: Result<serde_json::Value, _> =
        backend.new_api_get("/v2/test/resource", None::<&()>).await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GoveeError::Api { code, message } => {
            assert_eq!(code, 401);
            assert!(message.contains("Unauthorized"), "message was: {message}");
        }
        other => panic!("expected GoveeError::Api(401), got: {other:?}"),
    }
}

/// Build a CloudBackend where both the v1 base URL and the new-API base URL
/// point at the same mock server.  This is needed for tests that exercise
/// the v2 control path (which calls new_api_post) as well as the v1 legacy
/// fallback (which calls send_control via PUT /v1/devices/control).
fn combined_backend_for(server: &MockServer, api_key: &str) -> CloudBackend {
    CloudBackend::new(api_key.to_string(), Some(server.uri()), None)
        .unwrap()
        .with_new_api_base(&server.uri())
        .unwrap()
}

const NEW_API_CONTROL_SUCCESS: &str = r#"{
    "requestId": "test-req-id",
    "msg": "success",
    "code": 200,
    "payload": {}
}"#;

/// Helper: mount list_devices + v2 control mock, returning a combined backend.
async fn setup_v2_control(server: &MockServer) -> (CloudBackend, DeviceId) {
    let backend = combined_backend_for(server, "test-key");
    // Populate device cache via v1 list endpoint.
    Mock::given(method("GET"))
        .and(path("/v1/devices"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(HAPPY_RESPONSE, "application/json"))
        .mount(server)
        .await;
    backend.list_devices().await.unwrap();
    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    (backend, id)
}

// --- get_state_v2 tests ---

/// Build a CloudBackend where both the legacy base URL and the new API base URL
/// point at the same mock server.
fn v2_state_backend_for(server: &MockServer) -> CloudBackend {
    let uri = server.uri();
    CloudBackend::new("test-key".to_string(), Some(uri.clone()), None)
        .unwrap()
        .with_new_api_base(&uri)
        .unwrap()
}

/// Helper: populate device cache via a list_devices mock for v2 tests.
async fn populate_v2_device_cache(server: &MockServer, backend: &CloudBackend) {
    Mock::given(method("GET"))
        .and(path("/v1/devices"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(HAPPY_RESPONSE, "application/json"))
        .mount(server)
        .await;
    backend.list_devices().await.unwrap();
}

const V2_STATE_RESPONSE: &str = r#"{
    "requestId": "test-req-id",
    "msg": "success",
    "code": 200,
    "payload": {
        "sku": "H6076",
        "device": "AA:BB:CC:DD:EE:FF",
        "capabilities": [
            { "type": "devices.capabilities.on_off", "instance": "powerSwitch", "state": { "value": 1 } },
            { "type": "devices.capabilities.range", "instance": "brightness", "state": { "value": 80 } },
            { "type": "devices.capabilities.color_setting", "instance": "colorRgb", "state": { "value": 16744448 } },
            { "type": "devices.capabilities.color_setting", "instance": "colorTemperatureK", "state": { "value": 4000 } }
        ]
    }
}"#;

#[tokio::test]
async fn control_v2_set_power_on() {
    let server = MockServer::start().await;
    let (backend, id) = setup_v2_control(&server).await;

    Mock::given(method("POST"))
        .and(path("/router/api/v1/device/control"))
        .and(header("Govee-API-Key", "test-key"))
        .and(HasRequestId)
        .and(body_partial_json(serde_json::json!({
            "payload": {
                "sku": "H6076",
                "device": "AA:BB:CC:DD:EE:FF",
                "capability": {
                    "type": "devices.capabilities.on_off",
                    "instance": "powerSwitch",
                    "value": 1
                }
            }
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(NEW_API_CONTROL_SUCCESS, "application/json"),
        )
        .mount(&server)
        .await;

    backend.set_power(&id, true).await.unwrap();
}

#[tokio::test]
async fn control_v2_set_brightness() {
    let server = MockServer::start().await;
    let (backend, id) = setup_v2_control(&server).await;

    Mock::given(method("POST"))
        .and(path("/router/api/v1/device/control"))
        .and(header("Govee-API-Key", "test-key"))
        .and(HasRequestId)
        .and(body_partial_json(serde_json::json!({
            "payload": {
                "sku": "H6076",
                "device": "AA:BB:CC:DD:EE:FF",
                "capability": {
                    "type": "devices.capabilities.range",
                    "instance": "brightness",
                    "value": 80
                }
            }
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(NEW_API_CONTROL_SUCCESS, "application/json"),
        )
        .mount(&server)
        .await;

    backend.set_brightness(&id, 80).await.unwrap();
}

#[tokio::test]
async fn control_v2_set_color() {
    let server = MockServer::start().await;
    let (backend, id) = setup_v2_control(&server).await;

    // Color::new(255, 128, 0) packed = 0xFF8000 = 16744448
    Mock::given(method("POST"))
        .and(path("/router/api/v1/device/control"))
        .and(header("Govee-API-Key", "test-key"))
        .and(HasRequestId)
        .and(body_partial_json(serde_json::json!({
            "payload": {
                "sku": "H6076",
                "device": "AA:BB:CC:DD:EE:FF",
                "capability": {
                    "type": "devices.capabilities.color_setting",
                    "instance": "colorRgb",
                    "value": 0xFF8000u32
                }
            }
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(NEW_API_CONTROL_SUCCESS, "application/json"),
        )
        .mount(&server)
        .await;

    backend
        .set_color(&id, Color::new(255, 128, 0))
        .await
        .unwrap();
}

#[tokio::test]
async fn control_v2_fallback() {
    let server = MockServer::start().await;
    let (backend, id) = setup_v2_control(&server).await;

    // v2 endpoint returns 404 (API error) → should fall back to legacy PUT.
    Mock::given(method("POST"))
        .and(path("/router/api/v1/device/control"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
        .mount(&server)
        .await;

    // Legacy PUT endpoint should be called as fallback.
    Mock::given(method("PUT"))
        .and(path("/v1/devices/control"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(CONTROL_SUCCESS, "application/json"))
        .mount(&server)
        .await;

    backend.set_power(&id, true).await.unwrap();
}

#[tokio::test]
async fn get_state_v2_success() {
    let server = MockServer::start().await;
    let backend = v2_state_backend_for(&server);
    populate_v2_device_cache(&server, &backend).await;

    Mock::given(method("POST"))
        .and(path("/router/api/v1/device/state"))
        .and(header("Govee-API-Key", "test-key"))
        .and(HasRequestId)
        .and(body_partial_json(serde_json::json!({
            "payload": { "sku": "H6076", "device": "AA:BB:CC:DD:EE:FF" }
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(V2_STATE_RESPONSE, "application/json"),
        )
        .mount(&server)
        .await;

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    let state = backend.get_state(&id).await.unwrap();

    assert!(state.on);
    assert_eq!(state.brightness, 80);
    // 0xFF8000 = 16744448 → r=255, g=128, b=0
    assert_eq!(state.color, govee::types::Color::new(255, 128, 0));
    assert_eq!(state.color_temp_kelvin, Some(4000));
    assert!(state.raw.is_empty());
}

#[tokio::test]
async fn get_state_v2_unknown_capability_in_raw() {
    let server = MockServer::start().await;
    let backend = v2_state_backend_for(&server);
    populate_v2_device_cache(&server, &backend).await;

    let response_with_unknown = r#"{
        "requestId": "test-req-id",
        "msg": "success",
        "code": 200,
        "payload": {
            "sku": "H6076",
            "device": "AA:BB:CC:DD:EE:FF",
            "capabilities": [
                { "type": "devices.capabilities.on_off", "instance": "powerSwitch", "state": { "value": 1 } },
                { "type": "devices.capabilities.range", "instance": "brightness", "state": { "value": 80 } },
                { "type": "devices.capabilities.color_setting", "instance": "colorRgb", "state": { "value": 16744448 } },
                { "type": "devices.capabilities.color_setting", "instance": "colorTemperatureK", "state": { "value": 4000 } },
                { "type": "devices.capabilities.music_mode", "instance": "musicMode", "state": { "value": 2 } }
            ]
        }
    }"#;

    Mock::given(method("POST"))
        .and(path("/router/api/v1/device/state"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(response_with_unknown, "application/json"),
        )
        .mount(&server)
        .await;

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    let state = backend.get_state(&id).await.unwrap();

    assert!(state.on);
    assert_eq!(state.brightness, 80);
    assert_eq!(state.color, govee::types::Color::new(255, 128, 0));
    assert_eq!(state.color_temp_kelvin, Some(4000));

    let raw_key = "devices.capabilities.music_mode/musicMode";
    assert!(
        state.raw.contains_key(raw_key),
        "expected key {raw_key:?} in raw, got: {:?}",
        state.raw.keys().collect::<Vec<_>>()
    );
    assert_eq!(state.raw[raw_key], serde_json::json!(2));
}

#[tokio::test]
async fn get_state_v2_fallback() {
    let server = MockServer::start().await;
    let backend = v2_state_backend_for(&server);
    populate_v2_device_cache(&server, &backend).await;

    // v2 endpoint returns 500 → should fall back to legacy v1 endpoint.
    Mock::given(method("POST"))
        .and(path("/router/api/v1/device/state"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v1/devices/state"))
        .and(header("Govee-API-Key", "test-key"))
        .and(query_param("device", "AA:BB:CC:DD:EE:FF"))
        .and(query_param("model", "H6076"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(STATE_RESPONSE, "application/json"))
        .mount(&server)
        .await;

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    let state = backend.get_state(&id).await.unwrap();

    // Values from the legacy STATE_RESPONSE
    assert!(state.on);
    assert_eq!(state.brightness, 75);
    assert_eq!(state.color, govee::types::Color::new(255, 128, 0));
    assert_eq!(state.color_temp_kelvin, Some(5000));
}

/// Concrete payload type used in the envelope error test below.
#[derive(Debug, serde::Deserialize)]
struct ConcretePayload {
    #[allow(dead_code)]
    value: String,
}

/// When the envelope carries a non-200 code, the error must be returned even
/// if the payload cannot be deserialized into the concrete `Res` type.
/// This verifies that envelope errors take precedence over serde failures.
#[tokio::test]
async fn new_api_post_envelope_error_with_concrete_type() {
    let server = MockServer::start().await;

    // HTTP 200, envelope code 400, payload is an empty object (not a valid ConcretePayload).
    let response_body = serde_json::json!({
        "requestId": "x",
        "msg": "bad",
        "code": 400,
        "payload": {}
    });

    Mock::given(method("POST"))
        .and(path("/v2/test/endpoint"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
        .mount(&server)
        .await;

    let backend = new_api_backend_for(&server, "test-key");
    let result: Result<ConcretePayload, _> = backend
        .new_api_post("/v2/test/endpoint", serde_json::json!({}))
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GoveeError::Api { code, message } => {
            assert_eq!(code, 400);
            assert_eq!(message, "bad");
        }
        other => panic!("expected GoveeError::Api(400), got: {other:?}"),
    }
}

// --- list_devices_v2 tests ---

/// Build a CloudBackend whose both legacy and new-API base URLs point at the mock server.
fn full_backend_for(server: &MockServer, api_key: &str) -> CloudBackend {
    CloudBackend::new(api_key.to_string(), Some(server.uri()), None)
        .unwrap()
        .with_new_api_base(&server.uri())
        .unwrap()
}

const V2_DEVICES_RESPONSE: &str = r#"{
    "code": 200,
    "msg": "success",
    "requestId": "x",
    "data": [
        {
            "sku": "H6076",
            "device": "AA:BB:CC:DD:EE:FF",
            "deviceName": "Kitchen",
            "capabilities": [
                {
                    "type": "devices.capabilities.on_off",
                    "instance": "powerSwitch",
                    "parameters": {
                        "dataType": "ENUM",
                        "options": []
                    }
                }
            ]
        }
    ]
}"#;

#[tokio::test]
async fn list_devices_v2_success() {
    let server = MockServer::start().await;

    // v2 endpoint returns one device with capabilities.
    Mock::given(method("GET"))
        .and(path("/router/api/v1/user/devices"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(V2_DEVICES_RESPONSE, "application/json"),
        )
        .mount(&server)
        .await;

    let backend = full_backend_for(&server, "test-key");
    let devices = backend.list_devices().await.unwrap();

    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].id.as_str(), "AA:BB:CC:DD:EE:FF");
    assert_eq!(devices[0].model, "H6076");
    assert_eq!(devices[0].name, "Kitchen");

    // Capabilities should be stored and retrievable.
    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    let caps = backend.get_capabilities(&id).unwrap();
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0].type_, "devices.capabilities.on_off");
    assert_eq!(caps[0].instance, "powerSwitch");
}

#[tokio::test]
async fn list_devices_v2_unknown_capability() {
    let response = r#"{
        "code": 200,
        "msg": "success",
        "requestId": "x",
        "data": [
            {
                "sku": "H6076",
                "device": "AA:BB:CC:DD:EE:FF",
                "deviceName": "Kitchen",
                "capabilities": [
                    {
                        "type": "devices.capabilities.future",
                        "instance": "futureSwitch",
                        "parameters": {
                            "dataType": "UNKNOWN_FUTURE_TYPE",
                            "someField": 99
                        }
                    }
                ]
            }
        ]
    }"#;

    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/devices"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "devices": [] },
            "code": 200,
            "message": "Success"
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/router/api/v1/user/devices"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(response, "application/json"))
        .mount(&server)
        .await;

    let backend = full_backend_for(&server, "test-key");
    // Must succeed — unknown capability deserialized as Unknown variant.
    let devices = backend.list_devices().await.unwrap();
    assert_eq!(devices.len(), 1);

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    let caps = backend.get_capabilities(&id).unwrap();
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0].type_, "devices.capabilities.future");
    // Verify it deserialized as the Unknown variant.
    match &caps[0].parameters {
        govee::capability::CapabilityParameters::Unknown(v) => {
            assert_eq!(v["dataType"], "UNKNOWN_FUTURE_TYPE");
            assert_eq!(v["someField"], 99);
        }
        other => panic!("expected Unknown capability parameters, got: {other:?}"),
    }
}

#[tokio::test]
async fn list_devices_fallback_to_legacy() {
    let server = MockServer::start().await;

    // Legacy v1 endpoint returns one device.
    Mock::given(method("GET"))
        .and(path("/v1/devices"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(HAPPY_RESPONSE, "application/json"))
        .mount(&server)
        .await;

    // v2 endpoint returns 500 — should trigger fallback to legacy.
    Mock::given(method("GET"))
        .and(path("/router/api/v1/user/devices"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .mount(&server)
        .await;

    let backend = full_backend_for(&server, "test-key");
    let devices = backend.list_devices().await.unwrap();

    // Should fall back to legacy device list (2 devices from HAPPY_RESPONSE).
    assert_eq!(devices.len(), 2);
    assert_eq!(devices[0].id.as_str(), "AA:BB:CC:DD:EE:FF");
    assert_eq!(devices[1].id.as_str(), "11:22:33:44:55:66");
}

// --- work mode tests ---

const V2_WORK_MODE_DEVICES_RESPONSE: &str = r#"{
    "code": 200, "msg": "success", "requestId": "x",
    "data": [{
        "sku": "H6076",
        "device": "AA:BB:CC:DD:EE:FF",
        "deviceName": "Test",
        "capabilities": [{
            "type": "devices.capabilities.work_mode",
            "instance": "workMode",
            "parameters": {
                "dataType": "ENUM",
                "options": [
                    { "name": "Music", "value": 1 },
                    { "name": "Scene", "value": 2 }
                ]
            }
        }]
    }]
}"#;

#[tokio::test]
async fn list_work_modes_returns_modes() {
    let server = MockServer::start().await;

    // Mount v2 device list with work_mode capability.
    Mock::given(method("GET"))
        .and(path("/router/api/v1/user/devices"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(V2_WORK_MODE_DEVICES_RESPONSE, "application/json"),
        )
        .mount(&server)
        .await;

    // Mount v1 device list (needed for list_devices to succeed).
    Mock::given(method("GET"))
        .and(path("/v1/devices"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(HAPPY_RESPONSE, "application/json"))
        .mount(&server)
        .await;

    let backend = full_backend_for(&server, "test-key");
    backend.list_devices().await.unwrap();

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    let modes = backend.list_work_modes(&id).await.unwrap();

    assert_eq!(modes.len(), 2);
    assert_eq!(modes[0].id, 1);
    assert_eq!(modes[0].name, "Music");
    assert!(modes[0].sub_modes.is_empty());
    assert_eq!(modes[1].id, 2);
    assert_eq!(modes[1].name, "Scene");
    assert!(modes[1].sub_modes.is_empty());
}

#[tokio::test]
async fn list_work_modes_empty_when_no_cap() {
    let server = MockServer::start().await;

    // Mount v2 device list with no work_mode capability.
    Mock::given(method("GET"))
        .and(path("/router/api/v1/user/devices"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(V2_DEVICES_RESPONSE, "application/json"),
        )
        .mount(&server)
        .await;

    // Mount v1 device list.
    Mock::given(method("GET"))
        .and(path("/v1/devices"))
        .and(header("Govee-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(HAPPY_RESPONSE, "application/json"))
        .mount(&server)
        .await;

    let backend = full_backend_for(&server, "test-key");
    backend.list_devices().await.unwrap();

    let id = DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();
    let modes = backend.list_work_modes(&id).await.unwrap();
    assert!(modes.is_empty());
}

#[tokio::test]
async fn set_work_mode_sends_correct_payload() {
    let server = MockServer::start().await;
    let (backend, id) = setup_v2_control(&server).await;

    Mock::given(method("POST"))
        .and(path("/router/api/v1/device/control"))
        .and(header("Govee-API-Key", "test-key"))
        .and(HasRequestId)
        .and(body_partial_json(serde_json::json!({
            "payload": {
                "sku": "H6076",
                "device": "AA:BB:CC:DD:EE:FF",
                "capability": {
                    "type": "devices.capabilities.work_mode",
                    "instance": "workMode",
                    "value": { "workMode": 1, "modeValue": 3 }
                }
            }
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(NEW_API_CONTROL_SUCCESS, "application/json"),
        )
        .mount(&server)
        .await;

    backend.set_work_mode(&id, 1, Some(3)).await.unwrap();
}
