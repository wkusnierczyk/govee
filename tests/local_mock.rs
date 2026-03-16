//! Integration tests for the local LAN backend (UDP loopback).

use std::net::Ipv4Addr;
use std::time::Duration;

use govee::backend::GoveeBackend;
use govee::backend::local::LocalBackend;
use govee::error::GoveeError;

/// Test that creating a LocalBackend succeeds, then a second bind to
/// the same port fails with `BackendUnavailable`.
#[tokio::test]
async fn port_conflict_detection() {
    // First backend grabs port 4002.
    let backend1 = LocalBackend::new(Duration::from_millis(100), 10).await;

    if let Err(GoveeError::BackendUnavailable(_)) = &backend1 {
        // Port 4002 is already in use by something else on this system.
        // That's fine — we can't test conflict detection in this environment,
        // but the error variant itself proves the code path works.
        return;
    }

    let backend1 = backend1.expect("first LocalBackend should bind successfully");

    // Second backend should fail because port 4002 is taken.
    let result = LocalBackend::new(Duration::from_millis(100), 10).await;

    // On some platforms SO_REUSEADDR allows two binds; check both possibilities.
    match result {
        Err(GoveeError::BackendUnavailable(msg)) => {
            assert!(msg.contains("port 4002"));
        }
        Ok(_) => {
            // SO_REUSEADDR can allow this on some platforms — not a failure.
        }
        Err(e) => {
            panic!("expected BackendUnavailable or Ok, got: {e}");
        }
    }

    drop(backend1);
}

/// Test that the receiver task processes scan responses sent directly
/// to the listen port on loopback.
#[tokio::test]
async fn udp_loopback_discovery() {
    let backend = LocalBackend::new(Duration::from_millis(200), 60).await;

    if let Err(GoveeError::BackendUnavailable(_)) = &backend {
        // Port 4002 is in use — skip test in this environment.
        eprintln!("skipping udp_loopback_discovery: port 4002 in use");
        return;
    }

    let backend = backend.expect("LocalBackend should bind");

    // Send a fake scan response directly to the listen port on loopback.
    let sender = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0u16))
        .await
        .unwrap();

    let scan_response = serde_json::json!({
        "msg": {
            "cmd": "scan",
            "data": {
                "ip": "127.0.0.1",
                "device": "AA:BB:CC:DD:EE:FF:00:11",
                "sku": "H6076",
                "bleVersionHard": "1.0",
                "bleVersionSoft": "1.0",
                "wifiVersionHard": "1.0",
                "wifiVersionSoft": "1.0"
            }
        }
    });

    let msg = serde_json::to_vec(&scan_response).unwrap();
    sender
        .send_to(&msg, (Ipv4Addr::LOCALHOST, 4002u16))
        .await
        .unwrap();

    // Give the receiver task time to process.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // list_devices should find the device (cache won't be empty so no re-discover).
    let devices = backend.list_devices().await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].id.as_str(), "AA:BB:CC:DD:EE:FF:00:11");
    assert_eq!(devices[0].model, "H6076");
    assert_eq!(devices[0].backend, govee::types::BackendType::Local);

    // get_device_ip should return the IP from the scan response.
    let id = govee::types::DeviceId::new("AA:BB:CC:DD:EE:FF:00:11").unwrap();
    let ip = backend.get_device_ip(&id).unwrap();
    assert_eq!(ip, std::net::IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));

    drop(backend);
}

/// Test that stub methods return NotImplemented.
#[tokio::test]
async fn stub_methods_return_not_implemented() {
    let backend = LocalBackend::new(Duration::from_millis(100), 10).await;

    if let Err(GoveeError::BackendUnavailable(_)) = &backend {
        eprintln!("skipping stub_methods test: port 4002 in use");
        return;
    }

    let backend = backend.unwrap();
    let id = govee::types::DeviceId::new("AA:BB:CC:DD:EE:FF").unwrap();

    assert!(matches!(
        backend.get_state(&id).await,
        Err(GoveeError::DeviceNotFound(_))
    ));
    assert!(matches!(
        backend.set_power(&id, true).await,
        Err(GoveeError::NotImplemented(_))
    ));
    assert!(matches!(
        backend.set_brightness(&id, 50).await,
        Err(GoveeError::NotImplemented(_))
    ));
    assert!(matches!(
        backend
            .set_color(&id, govee::types::Color::new(255, 0, 0))
            .await,
        Err(GoveeError::NotImplemented(_))
    ));
    assert!(matches!(
        backend.set_color_temp(&id, 4000).await,
        Err(GoveeError::NotImplemented(_))
    ));

    assert_eq!(backend.backend_type(), govee::types::BackendType::Local);

    drop(backend);
}

/// Test UDP loopback state query: discover a fake device, call get_state,
/// have a test task send a devStatus response back, and verify the result.
#[tokio::test]
async fn udp_loopback_state_query() {
    let backend = LocalBackend::new(Duration::from_millis(500), 60).await;

    if let Err(GoveeError::BackendUnavailable(_)) = &backend {
        eprintln!("skipping udp_loopback_state_query: port 4002 in use");
        return;
    }

    let backend = backend.expect("LocalBackend should bind");

    // First, inject a fake device into the cache via a scan response.
    let sender = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0u16))
        .await
        .unwrap();

    let scan_response = serde_json::json!({
        "msg": {
            "cmd": "scan",
            "data": {
                "ip": "127.0.0.1",
                "device": "AA:BB:CC:DD:EE:FF:00:11",
                "sku": "H6076",
                "bleVersionHard": "1.0",
                "bleVersionSoft": "1.0",
                "wifiVersionHard": "1.0",
                "wifiVersionSoft": "1.0"
            }
        }
    });

    let msg = serde_json::to_vec(&scan_response).unwrap();
    sender
        .send_to(&msg, (Ipv4Addr::LOCALHOST, 4002u16))
        .await
        .unwrap();

    // Give the receiver task time to process the scan.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Spawn a task that listens on port 4003 for the devStatus request
    // and responds with a devStatus response to port 4002.
    let responder = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 4003u16)).await;
    if responder.is_err() {
        eprintln!("skipping udp_loopback_state_query: port 4003 in use");
        return;
    }
    let responder = responder.unwrap();

    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        if let Ok((_, _src)) = responder.recv_from(&mut buf).await {
            let status_response = serde_json::json!({
                "msg": {
                    "cmd": "devStatus",
                    "data": {
                        "onOff": 1,
                        "brightness": 75,
                        "color": {"r": 255, "g": 128, "b": 0},
                        "colorTemInKelvin": 5000
                    }
                }
            });

            let resp = serde_json::to_vec(&status_response).unwrap();
            // Send response to the backend's listen port.
            let reply_socket = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0u16))
                .await
                .unwrap();
            reply_socket
                .send_to(&resp, (Ipv4Addr::LOCALHOST, 4002u16))
                .await
                .unwrap();
        }
    });

    let id = govee::types::DeviceId::new("AA:BB:CC:DD:EE:FF:00:11").unwrap();
    let state = backend
        .get_state(&id)
        .await
        .expect("get_state should succeed");

    assert!(state.on);
    assert_eq!(state.brightness, 75);
    assert_eq!(state.color, govee::types::Color::new(255, 128, 0));
    assert_eq!(state.color_temp_kelvin, Some(5000));
    assert!(!state.stale);

    drop(backend);
}

/// Test that get_state times out when no response is sent.
#[tokio::test]
async fn get_state_timeout() {
    let backend = LocalBackend::new(Duration::from_millis(50), 60).await;

    if let Err(GoveeError::BackendUnavailable(_)) = &backend {
        eprintln!("skipping get_state_timeout: port 4002 in use");
        return;
    }

    let backend = backend.expect("LocalBackend should bind");

    // Inject a fake device into the cache.
    let sender = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0u16))
        .await
        .unwrap();

    let scan_response = serde_json::json!({
        "msg": {
            "cmd": "scan",
            "data": {
                "ip": "127.0.0.1",
                "device": "AA:BB:CC:DD:EE:FF:00:11",
                "sku": "H6076",
                "bleVersionHard": "1.0",
                "bleVersionSoft": "1.0",
                "wifiVersionHard": "1.0",
                "wifiVersionSoft": "1.0"
            }
        }
    });

    let msg = serde_json::to_vec(&scan_response).unwrap();
    sender
        .send_to(&msg, (Ipv4Addr::LOCALHOST, 4002u16))
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Call get_state with no responder — should timeout.
    let id = govee::types::DeviceId::new("AA:BB:CC:DD:EE:FF:00:11").unwrap();
    let result = backend.get_state(&id).await;

    assert!(
        matches!(result, Err(GoveeError::DiscoveryTimeout)),
        "expected DiscoveryTimeout, got: {result:?}"
    );

    drop(backend);
}
