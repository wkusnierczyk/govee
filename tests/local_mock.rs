//! Integration tests for the local LAN backend (UDP loopback).
//!
//! All tests bind to fixed port 4002 and must not run in parallel.
//! A shared mutex serializes access to prevent port contention.

use std::net::Ipv4Addr;
use std::sync::LazyLock;
use std::time::Duration;

use govee::backend::GoveeBackend;
use govee::backend::local::LocalBackend;
use govee::error::GoveeError;
use tokio::sync::Mutex;

/// Serializes all tests that bind port 4002.
static PORT_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Test that creating a LocalBackend succeeds, then a second bind to
/// the same port fails with `BackendUnavailable`.
#[tokio::test]
async fn port_conflict_detection() {
    let _lock = PORT_LOCK.lock().await;
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
    let _lock = PORT_LOCK.lock().await;
    let backend = LocalBackend::new(Duration::from_millis(200), 60).await;

    if let Err(GoveeError::BackendUnavailable(_)) = &backend {
        // Port 4002 is in use — skip test in this environment.
        eprintln!("skipping udp_loopback_discovery: port 4002 in use");
        return;
    }

    let backend = backend.expect("LocalBackend should bind");

    // Send a fake scan response directly to the listen port on loopback.
    let sender = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0u16))
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
    let ip = backend.get_device_ip(&id).await.unwrap();
    assert_eq!(ip, std::net::IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));

    drop(backend);
}

/// Test that get_state returns DeviceNotFound for an uncached device, and
/// control commands also return DeviceNotFound when the device is not in the cache.
#[tokio::test]
async fn stub_and_control_errors_without_device() {
    let _lock = PORT_LOCK.lock().await;
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
        Err(GoveeError::DeviceNotFound(_))
    ));
    assert!(matches!(
        backend.set_brightness(&id, 50).await,
        Err(GoveeError::DeviceNotFound(_))
    ));
    assert!(matches!(
        backend
            .set_color(&id, govee::types::Color::new(255, 0, 0))
            .await,
        Err(GoveeError::DeviceNotFound(_))
    ));
    assert!(matches!(
        backend.set_color_temp(&id, 4000).await,
        Err(GoveeError::DeviceNotFound(_))
    ));

    assert_eq!(backend.backend_type(), govee::types::BackendType::Local);

    drop(backend);
}

/// Test that set_brightness rejects values > 100.
#[tokio::test]
async fn set_brightness_rejects_invalid_value() {
    let _lock = PORT_LOCK.lock().await;
    let backend = LocalBackend::new(Duration::from_millis(100), 10).await;

    if let Err(GoveeError::BackendUnavailable(_)) = &backend {
        eprintln!("skipping test: port 4002 in use");
        return;
    }

    let backend = backend.unwrap();

    // Inject a fake device at 127.0.0.1 via scan response.
    let sender = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0u16))
        .await
        .unwrap();

    let scan_response = serde_json::json!({
        "msg": {
            "cmd": "scan",
            "data": {
                "ip": "127.0.0.1",
                "device": "AA:BB:CC:DD:EE:FF:00:11",
                "sku": "H6076"
            }
        }
    });
    sender
        .send_to(
            &serde_json::to_vec(&scan_response).unwrap(),
            (Ipv4Addr::LOCALHOST, 4002u16),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let id = govee::types::DeviceId::new("AA:BB:CC:DD:EE:FF:00:11").unwrap();
    assert!(matches!(
        backend.set_brightness(&id, 101).await,
        Err(GoveeError::InvalidBrightness(101))
    ));

    drop(backend);
}

/// Test UDP loopback state query: discover a fake device, call get_state,
/// have a test task send a devStatus response back, and verify the result.
#[tokio::test]
async fn udp_loopback_state_query() {
    let _lock = PORT_LOCK.lock().await;
    let backend = LocalBackend::new(Duration::from_millis(500), 60).await;

    if let Err(GoveeError::BackendUnavailable(_)) = &backend {
        eprintln!("skipping udp_loopback_state_query: port 4002 in use");
        return;
    }

    let backend = backend.expect("LocalBackend should bind");

    // First, inject a fake device into the cache via a scan response.
    let sender = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0u16))
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
            let reply_socket = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0u16))
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

/// Test that set_color_temp rejects 0K.
#[tokio::test]
async fn set_color_temp_rejects_zero() {
    let _lock = PORT_LOCK.lock().await;
    let backend = LocalBackend::new(Duration::from_millis(100), 10).await;

    if let Err(GoveeError::BackendUnavailable(_)) = &backend {
        eprintln!("skipping test: port 4002 in use");
        return;
    }

    let backend = backend.unwrap();

    let sender = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0u16))
        .await
        .unwrap();

    let scan_response = serde_json::json!({
        "msg": {
            "cmd": "scan",
            "data": {
                "ip": "127.0.0.1",
                "device": "AA:BB:CC:DD:EE:FF:00:11",
                "sku": "H6076"
            }
        }
    });
    sender
        .send_to(
            &serde_json::to_vec(&scan_response).unwrap(),
            (Ipv4Addr::LOCALHOST, 4002u16),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let id = govee::types::DeviceId::new("AA:BB:CC:DD:EE:FF:00:11").unwrap();
    assert!(matches!(
        backend.set_color_temp(&id, 0).await,
        Err(GoveeError::InvalidConfig(_))
    ));

    drop(backend);
}

/// Test that get_state times out when no response is sent.
#[tokio::test]
async fn get_state_timeout() {
    let _lock = PORT_LOCK.lock().await;
    let backend = LocalBackend::new(Duration::from_millis(50), 60).await;

    if let Err(GoveeError::BackendUnavailable(_)) = &backend {
        eprintln!("skipping get_state_timeout: port 4002 in use");
        return;
    }

    let backend = backend.expect("LocalBackend should bind");

    // Inject a fake device into the cache.
    let sender = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0u16))
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

/// UDP loopback test for set_power command.
#[tokio::test]
async fn udp_loopback_set_power() {
    let _lock = PORT_LOCK.lock().await;
    let backend = LocalBackend::new(Duration::from_millis(200), 60).await;

    if let Err(GoveeError::BackendUnavailable(_)) = &backend {
        eprintln!("skipping test: port 4002 in use");
        return;
    }

    let backend = backend.unwrap();

    // Bind a test socket on port 4003 to receive commands.
    let receiver = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 4003u16)).await;
    let receiver = match receiver {
        Ok(r) => r,
        Err(_) => {
            eprintln!("skipping test: port 4003 in use");
            return;
        }
    };

    // Inject a fake device at 127.0.0.1.
    let sender = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0u16))
        .await
        .unwrap();
    let scan_response = serde_json::json!({
        "msg": {
            "cmd": "scan",
            "data": {
                "ip": "127.0.0.1",
                "device": "AA:BB:CC:DD:EE:FF:00:11",
                "sku": "H6076"
            }
        }
    });
    sender
        .send_to(
            &serde_json::to_vec(&scan_response).unwrap(),
            (Ipv4Addr::LOCALHOST, 4002u16),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let id = govee::types::DeviceId::new("AA:BB:CC:DD:EE:FF:00:11").unwrap();

    // Send set_power(on).
    backend.set_power(&id, true).await.unwrap();

    let mut buf = [0u8; 4096];
    let (len, _) = tokio::time::timeout(Duration::from_secs(2), receiver.recv_from(&mut buf))
        .await
        .expect("timed out waiting for UDP packet")
        .unwrap();

    let received: serde_json::Value = serde_json::from_slice(&buf[..len]).unwrap();
    assert_eq!(received["msg"]["cmd"], "turn");
    assert_eq!(received["msg"]["data"]["value"], 1);

    drop(backend);
}

/// UDP loopback test for set_brightness command.
#[tokio::test]
async fn udp_loopback_set_brightness() {
    let _lock = PORT_LOCK.lock().await;
    let backend = LocalBackend::new(Duration::from_millis(200), 60).await;

    if let Err(GoveeError::BackendUnavailable(_)) = &backend {
        eprintln!("skipping test: port 4002 in use");
        return;
    }

    let backend = backend.unwrap();

    let receiver = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 4003u16)).await;
    let receiver = match receiver {
        Ok(r) => r,
        Err(_) => {
            eprintln!("skipping test: port 4003 in use");
            return;
        }
    };

    let sender = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0u16))
        .await
        .unwrap();
    let scan_response = serde_json::json!({
        "msg": {
            "cmd": "scan",
            "data": {
                "ip": "127.0.0.1",
                "device": "AA:BB:CC:DD:EE:FF:00:11",
                "sku": "H6076"
            }
        }
    });
    sender
        .send_to(
            &serde_json::to_vec(&scan_response).unwrap(),
            (Ipv4Addr::LOCALHOST, 4002u16),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let id = govee::types::DeviceId::new("AA:BB:CC:DD:EE:FF:00:11").unwrap();

    backend.set_brightness(&id, 75).await.unwrap();

    let mut buf = [0u8; 4096];
    let (len, _) = tokio::time::timeout(Duration::from_secs(2), receiver.recv_from(&mut buf))
        .await
        .expect("timed out waiting for UDP packet")
        .unwrap();

    let received: serde_json::Value = serde_json::from_slice(&buf[..len]).unwrap();
    assert_eq!(received["msg"]["cmd"], "brightness");
    assert_eq!(received["msg"]["data"]["value"], 75);

    drop(backend);
}

/// UDP loopback test for set_color command.
#[tokio::test]
async fn udp_loopback_set_color() {
    let _lock = PORT_LOCK.lock().await;
    let backend = LocalBackend::new(Duration::from_millis(200), 60).await;

    if let Err(GoveeError::BackendUnavailable(_)) = &backend {
        eprintln!("skipping test: port 4002 in use");
        return;
    }

    let backend = backend.unwrap();

    let receiver = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 4003u16)).await;
    let receiver = match receiver {
        Ok(r) => r,
        Err(_) => {
            eprintln!("skipping test: port 4003 in use");
            return;
        }
    };

    let sender = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0u16))
        .await
        .unwrap();
    let scan_response = serde_json::json!({
        "msg": {
            "cmd": "scan",
            "data": {
                "ip": "127.0.0.1",
                "device": "AA:BB:CC:DD:EE:FF:00:11",
                "sku": "H6076"
            }
        }
    });
    sender
        .send_to(
            &serde_json::to_vec(&scan_response).unwrap(),
            (Ipv4Addr::LOCALHOST, 4002u16),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let id = govee::types::DeviceId::new("AA:BB:CC:DD:EE:FF:00:11").unwrap();

    backend
        .set_color(&id, govee::types::Color::new(255, 128, 0))
        .await
        .unwrap();

    let mut buf = [0u8; 4096];
    let (len, _) = tokio::time::timeout(Duration::from_secs(2), receiver.recv_from(&mut buf))
        .await
        .expect("timed out waiting for UDP packet")
        .unwrap();

    let received: serde_json::Value = serde_json::from_slice(&buf[..len]).unwrap();
    assert_eq!(received["msg"]["cmd"], "colorwc");
    assert_eq!(received["msg"]["data"]["color"]["r"], 255);
    assert_eq!(received["msg"]["data"]["color"]["g"], 128);
    assert_eq!(received["msg"]["data"]["color"]["b"], 0);
    assert_eq!(received["msg"]["data"]["colorTemInKelvin"], 0);

    drop(backend);
}

/// UDP loopback test for set_color_temp command.
#[tokio::test]
async fn udp_loopback_set_color_temp() {
    let _lock = PORT_LOCK.lock().await;
    let backend = LocalBackend::new(Duration::from_millis(200), 60).await;

    if let Err(GoveeError::BackendUnavailable(_)) = &backend {
        eprintln!("skipping test: port 4002 in use");
        return;
    }

    let backend = backend.unwrap();

    let receiver = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 4003u16)).await;
    let receiver = match receiver {
        Ok(r) => r,
        Err(_) => {
            eprintln!("skipping test: port 4003 in use");
            return;
        }
    };

    let sender = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0u16))
        .await
        .unwrap();
    let scan_response = serde_json::json!({
        "msg": {
            "cmd": "scan",
            "data": {
                "ip": "127.0.0.1",
                "device": "AA:BB:CC:DD:EE:FF:00:11",
                "sku": "H6076"
            }
        }
    });
    sender
        .send_to(
            &serde_json::to_vec(&scan_response).unwrap(),
            (Ipv4Addr::LOCALHOST, 4002u16),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let id = govee::types::DeviceId::new("AA:BB:CC:DD:EE:FF:00:11").unwrap();

    backend.set_color_temp(&id, 4500).await.unwrap();

    let mut buf = [0u8; 4096];
    let (len, _) = tokio::time::timeout(Duration::from_secs(2), receiver.recv_from(&mut buf))
        .await
        .expect("timed out waiting for UDP packet")
        .unwrap();

    let received: serde_json::Value = serde_json::from_slice(&buf[..len]).unwrap();
    assert_eq!(received["msg"]["cmd"], "colorwc");
    assert_eq!(received["msg"]["data"]["color"]["r"], 0);
    assert_eq!(received["msg"]["data"]["color"]["g"], 0);
    assert_eq!(received["msg"]["data"]["color"]["b"], 0);
    assert_eq!(received["msg"]["data"]["colorTemInKelvin"], 4500);

    drop(backend);
}
