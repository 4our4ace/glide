//! Core platform-agnostic orchestration engine for Glide.

pub mod client;
pub mod config;
pub mod runtime;
pub mod server;

#[cfg(test)]
mod tests {
    use crate::client::run_client_session;
    use crate::config::{ClientConfig, KeyRemapRule, ServerConfig};
    use crate::runtime::RuntimeState;
    use crate::server::run_server_with_backends;
    use bytes::Bytes;
    use glide_backend::geometry::{DesktopGeometry, MonitorRect};
    use glide_backend::mock::{
        create_mock_capture, create_mock_clipboard, InjectedAction, MockInjector,
    };
    use glide_backend::CaptureEvent;
    use glide_hid::{HidUsage, MouseButton};
    use glide_net::sha256_bytes;
    use glide_net::tls::ServerIdentity;
    use glide_net::GlideServerEndpoint;
    use glide_proto::{ClipboardFormat, ClipboardPayload, Edge, LockState};
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    #[tokio::test]
    async fn full_v2_flow_german_keys_remaps_lock_sync_lazy_clipboard_and_hotkeys() {
        let identity = ServerIdentity::generate_ephemeral().unwrap();
        let fp = identity.fingerprint.clone();
        let endpoint =
            GlideServerEndpoint::bind("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = endpoint.local_addr();

        let win_geom = DesktopGeometry::new(vec![MonitorRect {
            name: "Win-4K".into(),
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
            scale: 1.5,
            primary: true,
        }]);
        let linux_geom = DesktopGeometry::new(vec![MonitorRect {
            name: "Ubuntu-1080p".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            primary: true,
        }]);

        let (capture_handle, cap_tx, cap_state) = create_mock_capture(win_geom);
        *cap_state.lock_state.lock() = LockState {
            caps_lock: true,
            num_lock: true,
        };
        let (srv_clip, srv_clip_tx, srv_clip_rx) = create_mock_clipboard();
        let (cli_clip, cli_clip_tx, cli_clip_rx) = create_mock_clipboard();
        let (mut injector, inj_log) = MockInjector::new(linux_geom);
        let srv_rt = RuntimeState::new();
        let cli_rt = RuntimeState::new();

        let srv_cfg = ServerConfig {
            name: "win11-test".into(),
            discovery: false,
            motion_coalesce_us: 0,
            ..Default::default()
        };
        let mut cli_cfg = ClientConfig {
            name: "ubuntu-test".into(),
            server: Some(addr.to_string()),
            edge: Edge::Right,
            trusted_fingerprint: Some(fp),
            sensitivity: 1.0,
            edge_threshold_px: 4,
            dead_corner_px: 24,
            key_remap: vec![KeyRemapRule {
                from: "CapsLock".into(),
                to: "ControlLeft".into(),
            }],
            ..Default::default()
        };

        let srv_rt_c = srv_rt.clone();
        let srv_task = tokio::spawn(async move {
            let _ = run_server_with_backends(
                srv_cfg,
                endpoint,
                capture_handle,
                Some(srv_clip),
                "windows",
                Some(srv_rt_c),
                None,
            )
            .await;
        });

        let cli_rt_c = cli_rt.clone();
        let cli_task = tokio::spawn(async move {
            let mut opt_clip = Some(cli_clip);
            let _ = run_client_session(
                &mut cli_cfg,
                addr,
                &mut injector,
                &mut opt_clip,
                Some(&cli_rt_c),
                None,
            )
            .await;
        });

        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(*cap_state.active_edges.lock(), vec![Edge::Right]);

        // 1. Lazy large clipboard (> 256 KiB): offered while client is unfocused — NOT transferred yet!
        let big_img = Bytes::from(vec![0x42u8; 300 * 1024]);
        srv_clip_tx
            .send(ClipboardPayload {
                format: ClipboardFormat::ImagePng,
                sha256: sha256_bytes(&big_img),
                data: big_img.clone(),
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(
            cli_clip_rx.lock().len(),
            0,
            "lazy clipboard must not transfer large payload until client gains focus"
        );

        // 2. Cross right edge of server at y=50% -> triggers Enter + LockSync + lazy clipboard fetch!
        cap_tx
            .send(CaptureEvent::EdgeCrossed {
                edge: Edge::Right,
                normalized_pos: 0.5,
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(cap_state.remote_mode.load(Ordering::SeqCst));
        assert_eq!(
            cli_clip_rx.lock().len(),
            1,
            "lazy clipboard should transfer immediately on Enter"
        );
        assert_eq!(cli_clip_rx.lock()[0].data.len(), 300 * 1024);

        // 3. Send motion, scroll, German AltGr + ISO < > |, and remapped CapsLock -> ControlLeft!
        cap_tx
            .send(CaptureEvent::MouseDelta {
                dx: 25.0,
                dy: -10.0,
            })
            .unwrap();
        cap_tx
            .send(CaptureEvent::MouseScroll {
                dx_120: 0,
                dy_120: -120,
            })
            .unwrap();
        cap_tx
            .send(CaptureEvent::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            })
            .unwrap();
        cap_tx
            .send(CaptureEvent::Key {
                usage: HidUsage::ALT_RIGHT,
                pressed: true,
            })
            .unwrap();
        cap_tx
            .send(CaptureEvent::Key {
                usage: HidUsage::ISO_BACKSLASH,
                pressed: true,
            })
            .unwrap();
        cap_tx
            .send(CaptureEvent::Key {
                usage: HidUsage::keyboard(0x39), // CapsLock -> remapped to ControlLeft!
                pressed: true,
            })
            .unwrap();

        // 4. Client -> Server small UTF-8 clipboard
        let from_cli = Bytes::from_static("Grüße aus Ubuntu: äöüß€@|".as_bytes());
        cli_clip_tx
            .send(ClipboardPayload {
                format: ClipboardFormat::TextUtf8,
                sha256: sha256_bytes(&from_cli),
                data: from_cli.clone(),
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;

        {
            let log = inj_log.lock();
            assert!(log.contains(&InjectedAction::Enter(Edge::Left, 0.5)));
            assert!(log.contains(&InjectedAction::LockSync(LockState {
                caps_lock: true,
                num_lock: true,
            })));
            assert!(log.contains(&InjectedAction::Move(25.0, -10.0)));
            assert!(log.contains(&InjectedAction::Key(HidUsage::ALT_RIGHT, true)));
            assert!(log.contains(&InjectedAction::Key(HidUsage::ISO_BACKSLASH, true)));
            assert!(log.contains(&InjectedAction::Key(HidUsage::CONTROL_LEFT, true)));
        }
        assert_eq!(srv_clip_rx.lock().len(), 1);
        assert_eq!(srv_clip_rx.lock()[0].data, from_cli);

        // 5. Push cursor left past client's Left edge -> client auto-releases held keys & returns focus!
        cap_tx
            .send(CaptureEvent::MouseDelta { dx: -60.0, dy: 0.0 })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!cap_state.remote_mode.load(Ordering::SeqCst));
        {
            let log = inj_log.lock();
            assert!(log.contains(&InjectedAction::ReleaseAll));
            assert!(log.contains(&InjectedAction::Key(HidUsage::ALT_RIGHT, false)));
            assert!(log.contains(&InjectedAction::Button(MouseButton::Left, false)));
        }

        // 6. Hotkey jump right (Ctrl+Alt+Right) -> EnterAt cursor memory!
        cap_tx
            .send(CaptureEvent::HotkeySwitch {
                direction: Edge::Right,
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(cap_state.remote_mode.load(Ordering::SeqCst));
        assert!(inj_log
            .lock()
            .iter()
            .any(|a| matches!(a, InjectedAction::EnterAt(_, _))));

        cli_task.abort();
        srv_task.abort();
    }

    #[tokio::test]
    async fn server_accept_loop_survives_failed_client_handshake() {
        let identity = ServerIdentity::generate_ephemeral().unwrap();
        let good_fp = identity.fingerprint.clone();
        let endpoint =
            GlideServerEndpoint::bind("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = endpoint.local_addr();

        let (capture_handle, _cap_tx, cap_state) =
            create_mock_capture(DesktopGeometry::fallback());
        let srv_cfg = ServerConfig {
            name: "srv-resilience".into(),
            discovery: false,
            clipboard: false,
            ..Default::default()
        };
        let srv_task = tokio::spawn(async move {
            let _ = run_server_with_backends(
                srv_cfg,
                endpoint,
                capture_handle,
                None,
                "linux-x11",
                None,
                None,
            )
            .await;
        });

        // 1. Connect with an intentionally mismatched pinned fingerprint -> TLS handshake fails.
        let bad_fp = "00:".repeat(31) + "00";
        let mut bad_cfg = ClientConfig {
            name: "bad-client".into(),
            server: Some(addr.to_string()),
            edge: Edge::Right,
            trusted_fingerprint: Some(bad_fp),
            clipboard: false,
            ..Default::default()
        };
        let (mut bad_inj, _) = MockInjector::new(DesktopGeometry::fallback());
        let mut none_clip = None;
        let bad_res =
            run_client_session(&mut bad_cfg, addr, &mut bad_inj, &mut none_clip, None, None).await;
        assert!(
            bad_res.is_err(),
            "mismatched fingerprint must fail handshake"
        );

        // 2. Now connect a valid client — server accept loop must still be alive!
        let mut good_cfg = ClientConfig {
            name: "good-client".into(),
            server: Some(addr.to_string()),
            edge: Edge::Left,
            trusted_fingerprint: Some(good_fp),
            clipboard: false,
            ..Default::default()
        };
        let (mut good_inj, _) = MockInjector::new(DesktopGeometry::fallback());
        let cli_task = tokio::spawn(async move {
            let mut none_clip = None;
            let _ = run_client_session(
                &mut good_cfg,
                addr,
                &mut good_inj,
                &mut none_clip,
                None,
                None,
            )
            .await;
        });

        let mut connected = false;
        for _ in 0..30 {
            if *cap_state.active_edges.lock() == vec![Edge::Left] {
                connected = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            connected,
            "valid client must connect after an earlier failed client TLS handshake"
        );

        cli_task.abort();
        srv_task.abort();
    }

    #[tokio::test]
    async fn large_bulk_clipboard_survives_concurrent_mouse_datagrams() {
        let identity = ServerIdentity::generate_ephemeral().unwrap();
        let fp = identity.fingerprint.clone();
        let endpoint =
            GlideServerEndpoint::bind("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = endpoint.local_addr();

        let (capture_handle, cap_tx, cap_state) =
            create_mock_capture(DesktopGeometry::fallback());
        let (srv_clip, srv_clip_tx, _) = create_mock_clipboard();
        let (cli_clip, _, cli_clip_rx) = create_mock_clipboard();
        let (mut injector, _) = MockInjector::new(DesktopGeometry::fallback());

        let srv_cfg = ServerConfig {
            name: "srv-bulk".into(),
            discovery: false,
            motion_coalesce_us: 0,
            ..Default::default()
        };
        let mut cli_cfg = ClientConfig {
            name: "cli-bulk".into(),
            server: Some(addr.to_string()),
            edge: Edge::Right,
            trusted_fingerprint: Some(fp),
            ..Default::default()
        };

        let srv_task = tokio::spawn(async move {
            let _ = run_server_with_backends(
                srv_cfg,
                endpoint,
                capture_handle,
                Some(srv_clip),
                "linux-x11",
                None,
                None,
            )
            .await;
        });

        let cli_task = tokio::spawn(async move {
            let mut opt_clip = Some(cli_clip);
            let _ = run_client_session(
                &mut cli_cfg,
                addr,
                &mut injector,
                &mut opt_clip,
                None,
                None,
            )
            .await;
        });

        for _ in 0..30 {
            if *cap_state.active_edges.lock() == vec![Edge::Right] {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(*cap_state.active_edges.lock(), vec![Edge::Right]);

        // Enter remote mode so mouse datagrams are actively processed by client select!.
        cap_tx
            .send(CaptureEvent::EdgeCrossed {
                edge: Edge::Right,
                normalized_pos: 0.5,
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;

        // Send a 4 MiB clipboard payload (multi-packet QUIC uni stream) while blasting
        // rapid mouse deltas that would previously cancel recv_bulk_on inside select!.
        let big = Bytes::from(vec![0xABu8; 4 * 1024 * 1024]);
        let expected_hash = sha256_bytes(&big);
        srv_clip_tx
            .send(ClipboardPayload {
                format: ClipboardFormat::ImagePng,
                sha256: expected_hash,
                data: big,
            })
            .unwrap();

        for _ in 0..200 {
            let _ = cap_tx.send(CaptureEvent::MouseDelta { dx: 1.0, dy: 0.0 });
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        let mut received = false;
        for _ in 0..50 {
            if cli_clip_rx
                .lock()
                .iter()
                .any(|p| p.sha256 == expected_hash && p.data.len() == 4 * 1024 * 1024)
            {
                received = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            received,
            "4 MiB clipboard transfer must complete intact despite concurrent mouse datagrams"
        );

        cli_task.abort();
        srv_task.abort();
    }
}
