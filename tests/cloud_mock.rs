//! Integration tests for CloudBackend using wiremock.

use govee::backend::GoveeBackend;
use govee::backend::cloud::CloudBackend;
use govee::error::GoveeError;
use govee::types::DeviceId;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Create a CloudBackend pointing at the mock server.
///
/// Uses `new_for_testing` because `CloudBackend::new` rejects non-HTTPS URLs.
fn backend_for(server: &MockServer, api_key: &str) -> CloudBackend {
    CloudBackend::new_for_testing(api_key.to_string(), server.uri())
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
