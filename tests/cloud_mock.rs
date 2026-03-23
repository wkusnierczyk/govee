//! Integration tests for CloudBackend using wiremock.

use govee::backend::GoveeBackend;
use govee::backend::cloud::CloudBackend;
use govee::error::GoveeError;
use govee::types::{Color, DeviceId};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Create a CloudBackend pointing at the mock server.
///
/// `CloudBackend::new` allows HTTP for loopback addresses (wiremock binds to 127.0.0.1).
fn backend_for(server: &MockServer, api_key: &str) -> CloudBackend {
    CloudBackend::new(api_key.to_string(), Some(server.uri()), None).unwrap()
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

// --- new_api_post / new_api_get tests ---

/// Build a CloudBackend whose new-API base URL points at the mock server.
fn new_api_backend_for(server: &MockServer, api_key: &str) -> CloudBackend {
    CloudBackend::new(api_key.to_string(), None, None)
        .unwrap()
        .with_new_api_base(&server.uri())
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
            assert_eq!(message, "unauthorized");
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
            assert_eq!(message, "bad request");
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
            assert_eq!(message, "not found");
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
            assert_eq!(message, "internal error");
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
            assert_eq!(message, "request failed");
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
    let result: serde_json::Value = backend.new_api_get("/v2/test/resource").await.unwrap();

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
    let result: Result<serde_json::Value, _> = backend.new_api_get("/v2/test/resource").await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GoveeError::Api { code, message } => {
            assert_eq!(code, 401);
            assert_eq!(message, "unauthorized");
        }
        other => panic!("expected GoveeError::Api(401), got: {other:?}"),
    }
}
