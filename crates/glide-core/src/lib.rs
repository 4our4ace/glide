//! Core platform-agnostic orchestration engine for Glide.

pub mod client;
pub mod config;
pub mod resilience;
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

    #[tokio::test]
    async fn switch_between_peers_releases_previous_peer_and_clears_coalesced_motion() {
        use crate::runtime::RuntimeCommand;

        let identity = ServerIdentity::generate_ephemeral().unwrap();
        let fp = identity.fingerprint.clone();
        let endpoint =
            GlideServerEndpoint::bind("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = endpoint.local_addr();

        let (capture_handle, cap_tx, cap_state) =
            create_mock_capture(DesktopGeometry::fallback());
        let (srv_cmd_tx, srv_cmd_rx) = tokio::sync::mpsc::unbounded_channel();

        let srv_cfg = ServerConfig {
            name: "srv-multi".into(),
            discovery: false,
            clipboard: false,
            // Long coalesce window so queued motion is still pending when we switch peers.
            motion_coalesce_us: 80_000,
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
                Some(srv_cmd_rx),
            )
            .await;
        });

        let (mut inj_a, log_a) = MockInjector::new(DesktopGeometry::fallback());
        let mut cfg_a = ClientConfig {
            name: "client-left".into(),
            server: Some(addr.to_string()),
            edge: Edge::Left,
            trusted_fingerprint: Some(fp.clone()),
            clipboard: false,
            ..Default::default()
        };
        let task_a = tokio::spawn(async move {
            let mut none_clip = None;
            let _ =
                run_client_session(&mut cfg_a, addr, &mut inj_a, &mut none_clip, None, None).await;
        });

        let (mut inj_b, log_b) = MockInjector::new(DesktopGeometry::fallback());
        let mut cfg_b = ClientConfig {
            name: "client-right".into(),
            server: Some(addr.to_string()),
            edge: Edge::Right,
            trusted_fingerprint: Some(fp),
            clipboard: false,
            ..Default::default()
        };
        let task_b = tokio::spawn(async move {
            let mut none_clip = None;
            let _ =
                run_client_session(&mut cfg_b, addr, &mut inj_b, &mut none_clip, None, None).await;
        });

        for _ in 0..40 {
            if cap_state.active_edges.lock().len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(cap_state.active_edges.lock().len(), 2);

        // Focus Left client and hold KeyA.
        srv_cmd_tx.send(RuntimeCommand::SwitchToEdge(Edge::Left)).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        cap_tx
            .send(CaptureEvent::Key {
                usage: HidUsage::keyboard(0x04),
                pressed: true,
            })
            .unwrap();
        for _ in 0..30 {
            if log_a
                .lock()
                .contains(&InjectedAction::Key(HidUsage::keyboard(0x04), true))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(log_a
            .lock()
            .contains(&InjectedAction::Key(HidUsage::keyboard(0x04), true)));

        // Queue a pending coalesced mouse move, let the server ingest it, then switch to Right
        // before the 80ms coalesce timer fires.
        cap_tx
            .send(CaptureEvent::MouseDelta {
                dx: 99.0,
                dy: 77.0,
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(15)).await;
        srv_cmd_tx
            .send(RuntimeCommand::SwitchToEdge(Edge::Right))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;

        // Client A must have had KeyA released and received ReleaseAll.
        {
            let la = log_a.lock();
            assert!(
                la.contains(&InjectedAction::ReleaseAll),
                "switching directly to another peer must release previous peer"
            );
            assert!(
                la.contains(&InjectedAction::Key(HidUsage::keyboard(0x04), false)),
                "held key on previous peer must be released"
            );
        }

        // Client B must be entered and must NOT have received Client A's stale coalesced (99, 77) move!
        {
            let lb = log_b.lock();
            assert!(lb.iter().any(|a| matches!(a, InjectedAction::EnterAt(_, _))));
            assert!(
                !lb.contains(&InjectedAction::Move(99.0, 77.0)),
                "stale coalesced motion from previous peer must not bleed into newly focused peer"
            );
        }

        // Graceful shutdown while Client B is active must restore local capture mode.
        assert!(cap_state.remote_mode.load(Ordering::SeqCst));
        srv_cmd_tx.send(RuntimeCommand::Shutdown).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), srv_task).await;
        assert!(
            !cap_state.remote_mode.load(Ordering::SeqCst),
            "server shutdown must release remote capture mode"
        );

        task_a.abort();
        task_b.abort();
    }

    fn single_monitor_geom(name: &str) -> DesktopGeometry {
        DesktopGeometry::new(vec![MonitorRect {
            name: name.into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            primary: true,
        }])
    }

    #[tokio::test]
    async fn server_and_client_survive_suspend_resume_with_health_probe_and_clean_shutdown() {
        use crate::client::run_client_with_backends;
        use crate::runtime::RuntimeCommand;
        use std::sync::Arc;
        use tokio::sync::mpsc;

        let identity = ServerIdentity::generate_ephemeral().unwrap();
        let fp = identity.fingerprint.clone();
        let endpoint =
            GlideServerEndpoint::bind("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = endpoint.local_addr();

        let (capture_handle, cap_tx, cap_state) = create_mock_capture(single_monitor_geom("Srv"));
        let (mut injector, inj_log) = MockInjector::new(single_monitor_geom("Cli"));
        let resumed_flag = Arc::clone(&injector.resumed_flag);

        let srv_rt = RuntimeState::new();
        let cli_rt = RuntimeState::new();
        let (srv_cmd_tx, srv_cmd_rx) = mpsc::unbounded_channel();
        let (cli_cmd_tx, cli_cmd_rx) = mpsc::unbounded_channel();

        let srv_cfg = ServerConfig {
            name: "srv-resilience".into(),
            discovery: false,
            motion_coalesce_us: 0,
            ..Default::default()
        };
        let cli_cfg = ClientConfig {
            name: "cli-resilience".into(),
            server: Some(addr.to_string()),
            edge: Edge::Right,
            trusted_fingerprint: Some(fp),
            clipboard: false,
            ..Default::default()
        };

        let srv_rt_c = srv_rt.clone();
        let srv_task = tokio::spawn(async move {
            run_server_with_backends(
                srv_cfg,
                endpoint,
                capture_handle,
                None,
                "linux-x11",
                Some(srv_rt_c),
                Some(srv_cmd_rx),
            )
            .await
        });

        let cli_rt_c = cli_rt.clone();
        let cli_task = tokio::spawn(async move {
            let mut no_clip = None;
            run_client_with_backends(
                cli_cfg,
                &mut injector,
                &mut no_clip,
                Some(cli_rt_c),
                Some(cli_cmd_rx),
            )
            .await
        });

        // Wait for initial connection
        for _ in 0..30 {
            if srv_rt.snapshot().peers.len() == 1
                && cli_rt.snapshot().status.starts_with("Connected")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(srv_rt.snapshot().peers.len(), 1);
        assert!(cli_rt.snapshot().status.starts_with("Connected"));

        // Enter remote and hold a key, then simulate OS suspend
        cap_tx
            .send(CaptureEvent::EdgeCrossed {
                edge: Edge::Right,
                normalized_pos: 0.5,
            })
            .unwrap();
        cap_tx
            .send(CaptureEvent::Key {
                usage: HidUsage::CONTROL_LEFT,
                pressed: true,
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(cap_state.remote_mode.load(Ordering::SeqCst));

        cap_tx.send(CaptureEvent::SystemSuspended).unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!cap_state.remote_mode.load(Ordering::SeqCst));
        assert!(inj_log.lock().contains(&InjectedAction::ReleaseAll));
        assert!(inj_log
            .lock()
            .contains(&InjectedAction::Key(HidUsage::CONTROL_LEFT, false)));

        // Trigger SystemResumed on BOTH server and client -> both arm 1.5s HealthProbes and send Ping!
        cap_tx.send(CaptureEvent::SystemResumed).unwrap();
        resumed_flag.store(true, Ordering::SeqCst);

        // Wait > 1.5s HealthProbe timeout to prove Ping/Pong cleared probes on BOTH sides
        tokio::time::sleep(Duration::from_millis(1900)).await;
        assert_eq!(
            srv_rt.snapshot().peers.len(),
            1,
            "healthy peer must survive post-resume health probe"
        );
        assert!(
            cli_rt.snapshot().status.starts_with("Connected"),
            "healthy client must survive post-resume health probe: {}",
            cli_rt.snapshot().status
        );

        // Graceful Shutdown on both ends must finish promptly
        cli_cmd_tx.send(RuntimeCommand::Shutdown).unwrap();
        srv_cmd_tx.send(RuntimeCommand::Shutdown).unwrap();
        let cli_res = tokio::time::timeout(Duration::from_millis(800), cli_task)
            .await
            .expect("client must exit cleanly on Shutdown")
            .unwrap();
        let srv_res = tokio::time::timeout(Duration::from_millis(800), srv_task)
            .await
            .expect("server must exit cleanly on Shutdown")
            .unwrap();
        assert!(cli_res.is_ok());
        assert!(srv_res.is_ok());
    }

    #[tokio::test]
    async fn server_evicts_unresponsive_peer_after_resume_health_probe_timeout() {
        use crate::runtime::RuntimeCommand;
        use glide_net::connect_client;
        use glide_net::tls::TrustMode;
        use glide_proto::{ControlMessage, OsKind, PROTOCOL_VERSION};
        use tokio::sync::mpsc;

        let identity = ServerIdentity::generate_ephemeral().unwrap();
        let fp = identity.fingerprint.clone();
        let endpoint =
            GlideServerEndpoint::bind("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = endpoint.local_addr();

        let (capture_handle, cap_tx, cap_state) = create_mock_capture(single_monitor_geom("Srv"));
        let srv_rt = RuntimeState::new();
        let (srv_cmd_tx, srv_cmd_rx) = mpsc::unbounded_channel();

        let srv_cfg = ServerConfig {
            name: "srv-evict".into(),
            discovery: false,
            ..Default::default()
        };
        let srv_rt_c = srv_rt.clone();
        let srv_task = tokio::spawn(async move {
            run_server_with_backends(
                srv_cfg,
                endpoint,
                capture_handle,
                None,
                "linux-x11",
                Some(srv_rt_c),
                Some(srv_cmd_rx),
            )
            .await
        });

        // Connect a silent client that completes handshake but never answers Ping
        let mut silent_peer = connect_client(addr, TrustMode::Pinned(fp)).await.unwrap();
        silent_peer
            .ctrl_send
            .send(&ControlMessage::Hello {
                version: PROTOCOL_VERSION,
                peer_name: "silent-peer".into(),
                edge_on_server: Edge::Right,
                os: OsKind::LinuxX11,
            })
            .await
            .unwrap();
        let _welcome = silent_peer.ctrl_recv.recv().await.unwrap();

        for _ in 0..20 {
            if srv_rt.snapshot().peers.len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(srv_rt.snapshot().peers.len(), 1);
        assert_eq!(*cap_state.active_edges.lock(), vec![Edge::Right]);

        // Trigger SystemResumed on server -> sends Ping and starts 1.5s HealthProbe
        let started = std::time::Instant::now();
        cap_tx.send(CaptureEvent::SystemResumed).unwrap();

        // Server must evict the silent peer before QUIC's 4s idle timeout
        let mut evicted = false;
        while started.elapsed() < Duration::from_millis(2800) {
            if srv_rt.snapshot().peers.is_empty() {
                evicted = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            evicted,
            "server should evict peer that ignores post-resume Ping within ~1.5-2s"
        );
        assert!(cap_state.active_edges.lock().is_empty());

        drop(silent_peer);
        srv_cmd_tx.send(RuntimeCommand::Shutdown).unwrap();
        let res = tokio::time::timeout(Duration::from_millis(800), srv_task)
            .await
            .expect("server must shut down cleanly")
            .unwrap();
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn client_fails_fast_when_server_ignores_post_resume_health_ping() {
        use glide_proto::{ControlMessage, PROTOCOL_VERSION};
        use std::sync::Arc;

        let identity = ServerIdentity::generate_ephemeral().unwrap();
        let fp = identity.fingerprint.clone();
        let endpoint =
            GlideServerEndpoint::bind("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = endpoint.local_addr();

        // Silent server: finishes Welcome handshake, then holds QUIC alive without answering Ping
        let silent_server = tokio::spawn(async move {
            let mut session = endpoint.accept().await.unwrap();
            let _hello = session.ctrl_recv.recv().await.unwrap();
            session
                .ctrl_send
                .send(&ControlMessage::Welcome {
                    version: PROTOCOL_VERSION,
                    server_name: "silent-srv".into(),
                    accepted: true,
                    reason: String::new(),
                    server_scale: 1.0,
                })
                .await
                .unwrap();
            // Keep connection alive longer than the 1.5s HealthProbe timeout
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(session);
            endpoint.close();
        });

        let (mut injector, _) = MockInjector::new(single_monitor_geom("Cli"));
        let resumed_flag = Arc::clone(&injector.resumed_flag);
        let cli_rt = RuntimeState::new();
        let mut cli_cfg = ClientConfig {
            name: "cli-probe".into(),
            server: Some(addr.to_string()),
            edge: Edge::Right,
            trusted_fingerprint: Some(fp),
            clipboard: false,
            ..Default::default()
        };

        let cli_rt_c = cli_rt.clone();
        let cli_task = tokio::spawn(async move {
            let mut no_clip = None;
            run_client_session(
                &mut cli_cfg,
                addr,
                &mut injector,
                &mut no_clip,
                Some(&cli_rt_c),
                None,
            )
            .await
        });

        for _ in 0..30 {
            if cli_rt.snapshot().status.starts_with("Connected") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        assert!(cli_rt.snapshot().status.starts_with("Connected"));

        let started = std::time::Instant::now();
        resumed_flag.store(true, Ordering::SeqCst);

        let err = tokio::time::timeout(Duration::from_millis(2800), cli_task)
            .await
            .expect("client must fail fast on unanswered post-resume Ping")
            .unwrap()
            .expect_err("expected health probe timeout error");
        assert!(
            err.to_string().contains("health ping"),
            "unexpected error: {err}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(2800),
            "should fail well before 4s QUIC idle timeout"
        );
        silent_server.abort();
    }

    #[tokio::test]
    async fn client_supervisor_recovers_roamed_pinned_server_and_shuts_down_cleanly() {
        use crate::client::run_client_with_backends;
        use crate::runtime::RuntimeCommand;
        use tokio::sync::mpsc;

        // 1. Start the real server with LAN discovery enabled on a dedicated test port
        let real_id = ServerIdentity::generate_ephemeral().unwrap();
        let real_fp = real_id.fingerprint.clone();
        let real_ep =
            GlideServerEndpoint::bind("127.0.0.1:0".parse().unwrap(), &real_id).unwrap();

        // 2. Start a decoy endpoint with a DIFFERENT identity on a stale address so the client's
        //    first explicit attempt fails immediately (simulating old DHCP IP taken by another host).
        let decoy_id = ServerIdentity::generate_ephemeral().unwrap();
        let decoy_ep =
            GlideServerEndpoint::bind("127.0.0.1:0".parse().unwrap(), &decoy_id).unwrap();
        let stale_addr = decoy_ep.local_addr();
        let decoy_ep_clone = decoy_ep.clone();
        let decoy_task = tokio::spawn(async move {
            let _ = decoy_ep_clone.accept().await;
        });

        let discovery_port = 24895;
        let (capture_handle, _cap_tx, _cap_state) =
            create_mock_capture(single_monitor_geom("RealSrv"));
        let srv_rt = RuntimeState::new();
        let (srv_cmd_tx, srv_cmd_rx) = mpsc::unbounded_channel();
        let srv_cfg = ServerConfig {
            name: "roamed-server".into(),
            discovery: true,
            discovery_port,
            ..Default::default()
        };
        let srv_rt_c = srv_rt.clone();
        let srv_task = tokio::spawn(async move {
            run_server_with_backends(
                srv_cfg,
                real_ep,
                capture_handle,
                None,
                "linux-x11",
                Some(srv_rt_c),
                Some(srv_cmd_rx),
            )
            .await
        });

        let (mut injector, _) = MockInjector::new(single_monitor_geom("Cli"));
        let cli_rt = RuntimeState::new();
        let (cli_cmd_tx, cli_cmd_rx) = mpsc::unbounded_channel();
        let cli_cfg = ClientConfig {
            name: "roaming-client".into(),
            // Point explicitly at the stale address, but pin the real server's fingerprint!
            server: Some(stale_addr.to_string()),
            edge: Edge::Right,
            discovery_port,
            trusted_fingerprint: Some(real_fp),
            clipboard: false,
            ..Default::default()
        };

        let cli_rt_c = cli_rt.clone();
        let cli_task = tokio::spawn(async move {
            let mut no_clip = None;
            run_client_with_backends(
                cli_cfg,
                &mut injector,
                &mut no_clip,
                Some(cli_rt_c),
                Some(cli_cmd_rx),
            )
            .await
        });

        // Client should fail on stale_addr, discover real_ep by fingerprint, and connect!
        let mut connected = false;
        for _ in 0..80 {
            if srv_rt.snapshot().peers.len() == 1
                && cli_rt.snapshot().status.starts_with("Connected")
            {
                connected = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            connected,
            "client should follow pinned server via LAN discovery; status={}",
            cli_rt.snapshot().status
        );

        cli_cmd_tx.send(RuntimeCommand::Shutdown).unwrap();
        srv_cmd_tx.send(RuntimeCommand::Shutdown).unwrap();
        let cli_res = tokio::time::timeout(Duration::from_millis(800), cli_task)
            .await
            .expect("client must exit cleanly on Shutdown")
            .unwrap();
        let srv_res = tokio::time::timeout(Duration::from_millis(800), srv_task)
            .await
            .expect("server must exit cleanly on Shutdown")
            .unwrap();
        assert!(cli_res.is_ok());
        assert!(srv_res.is_ok());

        decoy_ep.close();
        decoy_task.abort();
    }

    #[tokio::test]
    async fn client_supervisor_shutdown_interrupts_connect_and_backoff_immediately() {
        use crate::client::run_client_with_backends;
        use crate::runtime::RuntimeCommand;
        use tokio::sync::mpsc;

        // Bind a UDP socket that never answers QUIC handshakes so connect_client waits on CONNECT_TIMEOUT
        let blackhole = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let blackhole_addr = blackhole.local_addr().unwrap();

        let (mut injector, _) = MockInjector::new(single_monitor_geom("Cli"));
        let (cli_cmd_tx, cli_cmd_rx) = mpsc::unbounded_channel();
        let cli_cfg = ClientConfig {
            name: "shutdown-test".into(),
            server: Some(blackhole_addr.to_string()),
            insecure_skip_verify: true,
            clipboard: false,
            ..Default::default()
        };

        let cli_task = tokio::spawn(async move {
            let mut no_clip = None;
            run_client_with_backends(cli_cfg, &mut injector, &mut no_clip, None, Some(cli_cmd_rx))
                .await
        });

        tokio::time::sleep(Duration::from_millis(120)).await;
        let started = std::time::Instant::now();
        cli_cmd_tx.send(RuntimeCommand::Shutdown).unwrap();

        let res = tokio::time::timeout(Duration::from_millis(500), cli_task)
            .await
            .expect("Shutdown must interrupt in-flight connect without waiting 4s")
            .unwrap();
        assert!(res.is_ok());
        assert!(started.elapsed() < Duration::from_millis(400));
        drop(blackhole);
    }
}
