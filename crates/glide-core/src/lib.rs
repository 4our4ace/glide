//! Core platform-agnostic orchestration engine for Glide.

pub mod client;
pub mod config;
pub mod dnd;
pub mod ipc;
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
        let known_path = std::env::temp_dir().join(format!(
            "glide-e2e-known-servers-{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&known_path);

        let mut cli_cfg = ClientConfig {
            name: "ubuntu-test".into(),
            server: Some(addr.to_string()),
            edge: Edge::Right,
            trusted_fingerprint: Some(fp.clone()),
            sensitivity: 1.0,
            edge_threshold_px: 4,
            dead_corner_px: 24,
            key_remap: vec![KeyRemapRule {
                from: "CapsLock".into(),
                to: "ControlLeft".into(),
            }],
            known_servers_path: Some(known_path.clone()),
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
        let known = crate::config::list_known_servers(&known_path).unwrap();
        assert_eq!(known.len(), 1);
        assert_eq!(known[0].name, "win11-test");
        assert_eq!(known[0].address, addr.to_string());
        assert_eq!(known[0].fingerprint.as_deref(), Some(fp.as_str()));
        assert_eq!(known[0].edge, Edge::Right);
        let _ = std::fs::remove_file(&known_path);

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

        // Send a leading-edge move (flushes immediately to Client A), then queue a trailing
        // coalesced mouse move (buffered for the 80ms coalesce window) and switch to Right
        // before the timer fires.
        cap_tx
            .send(CaptureEvent::MouseDelta { dx: 1.0, dy: 0.0 })
            .unwrap();
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
            GlideServerEndpoint::bind("0.0.0.0:0".parse().unwrap(), &real_id).unwrap();

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

    #[tokio::test]
    async fn cross_edge_dnd_transfer_drop_cancel_and_bidirectional_flow() {
        use crate::client::run_client_session_with_dnd;
        use crate::runtime::RuntimeCommand;
        use crate::server::run_server_with_dnd;
        use glide_backend::mock::create_mock_dnd;
        use std::sync::Arc;

        let unique = format!(
            "glide-dnd-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root_tmp = std::env::temp_dir().join(unique);
        let src_dir = root_tmp.join("src");
        let cli_recv_dir = root_tmp.join("client-recv");
        let srv_recv_dir = root_tmp.join("server-recv");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&cli_recv_dir).unwrap();
        std::fs::create_dir_all(&srv_recv_dir).unwrap();

        let file1 = src_dir.join("Büro Grüße äöüß.txt");
        let file2 = src_dir.join("design notes.md");
        let file3 = src_dir.join("cancel-me.bin");
        let file4 = src_dir.join("from-client.txt");
        std::fs::write(&file1, b"hello from server with umlauts \xc3\xa4\xc3\xb6\xc3\xbc\xc3\x9f")
            .unwrap();
        std::fs::write(&file2, b"# Glide DnD\nsecond file").unwrap();
        std::fs::write(&file3, b"should never be written on cancel").unwrap();
        std::fs::write(&file4, b"dragged back from client to server!").unwrap();

        let identity = ServerIdentity::generate_ephemeral().unwrap();
        let fp = identity.fingerprint.clone();
        let endpoint =
            GlideServerEndpoint::bind("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = endpoint.local_addr();

        let geom = DesktopGeometry::new(vec![MonitorRect {
            name: "Main".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            primary: true,
        }]);

        let (capture_handle, cap_tx, cap_state) = create_mock_capture(geom.clone());
        let (mut injector, _inj_log) = MockInjector::new(geom);
        let (srv_dnd, srv_dnd_state) = create_mock_dnd();
        let (cli_dnd, cli_dnd_state) = create_mock_dnd();
        let srv_rt = RuntimeState::new();
        let cli_rt = RuntimeState::new();
        let (srv_cmd_tx, srv_cmd_rx) = tokio::sync::mpsc::unbounded_channel();

        let srv_cfg = ServerConfig {
            name: "win11-srv".into(),
            discovery: false,
            motion_coalesce_us: 0,
            drag_drop_files: true,
            ..Default::default()
        };
        let mut cli_cfg = ClientConfig {
            name: "ubuntu-cli".into(),
            server: Some(addr.to_string()),
            edge: Edge::Right,
            trusted_fingerprint: Some(fp),
            edge_threshold_px: 4,
            dead_corner_px: 24,
            drag_drop_files: true,
            ..Default::default()
        };

        let srv_rt_c = srv_rt.clone();
        let srv_recv_c = srv_recv_dir.clone();
        let srv_task = tokio::spawn(async move {
            let _ = run_server_with_dnd(
                srv_cfg,
                endpoint,
                capture_handle,
                None,
                srv_dnd,
                Some(srv_recv_c),
                "windows",
                Some(srv_rt_c),
                Some(srv_cmd_rx),
            )
            .await;
        });

        let cli_rt_c = cli_rt.clone();
        let cli_recv_c = cli_recv_dir.clone();
        let cli_task = tokio::spawn(async move {
            let mut opt_clip = None;
            let _ = run_client_session_with_dnd(
                &mut cli_cfg,
                addr,
                &mut injector,
                &mut opt_clip,
                cli_dnd,
                Some(cli_recv_c),
                Some(&cli_rt_c),
                None,
            )
            .await;
        });

        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(*srv_dnd_state.active_edges.lock(), vec![Edge::Right]);
        assert_eq!(*cli_dnd_state.active_edges.lock(), vec![Edge::Left]);

        // --- Scenario 1: Server -> Client drag across edge -> parallel QUIC transfer -> drop ---
        cap_tx
            .send(CaptureEvent::LocalLeftButton { pressed: true })
            .unwrap();
        cap_tx.send(CaptureEvent::DragStripArmed(true)).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(srv_dnd_state.armed.load(Ordering::SeqCst));

        // Simulate OS edge strip staging two local files (including umlauts + spaces)
        srv_dnd_state.stage_files(Edge::Right, vec![file1.clone(), file2.clone()]);
        cap_tx
            .send(CaptureEvent::EdgeCrossed {
                edge: Edge::Right,
                normalized_pos: 0.5,
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(cap_state.remote_mode.load(Ordering::SeqCst));
        assert_eq!(
            srv_dnd_state.cancelled_local_drags.load(Ordering::SeqCst),
            1,
            "local OS drag must be cancelled cleanly on handoff"
        );

        // Move across client desktop, then release LMB to drop!
        cap_tx
            .send(CaptureEvent::MouseDelta { dx: 120.0, dy: 40.0 })
            .unwrap();
        cap_tx
            .send(CaptureEvent::MouseButton {
                button: MouseButton::Left,
                pressed: false,
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;

        {
            let drops = cli_dnd_state.delivered_drops.lock();
            assert_eq!(drops.len(), 1, "expected exactly 1 drop handoff on client");
            assert_eq!(drops[0].len(), 2, "expected both dragged files delivered");
            for p in &drops[0] {
                assert!(p.exists(), "dropped file {} must exist on disk", p.display());
            }
        }
        assert_eq!(
            std::fs::read(cli_recv_dir.join("Büro Grüße äöüß.txt")).unwrap(),
            b"hello from server with umlauts \xc3\xa4\xc3\xb6\xc3\xbc\xc3\x9f"
        );
        assert_eq!(
            std::fs::read(cli_recv_dir.join("design notes.md")).unwrap(),
            b"# Glide DnD\nsecond file"
        );
        let cli_recent = cli_rt.snapshot().recent_files;
        assert!(
            cli_recent
                .iter()
                .any(|f| f.filename == "Büro Grüße äöüß.txt" && f.direction == "received"),
            "received drag file should appear in RuntimeState::recent_files"
        );
        let srv_recent = srv_rt.snapshot().recent_files;
        assert!(
            srv_recent
                .iter()
                .any(|f| f.filename == "Büro Grüße äöüß.txt" && f.direction == "sent"),
            "sent drag file should appear in server RuntimeState::recent_files"
        );

        // Return focus back to server for Scenario 2
        cap_tx
            .send(CaptureEvent::MouseDelta { dx: -200.0, dy: 0.0 })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!cap_state.remote_mode.load(Ordering::SeqCst));

        // --- Scenario 2: Drag across edge -> return to origin before releasing LMB -> cancel! ---
        cap_tx
            .send(CaptureEvent::LocalLeftButton { pressed: true })
            .unwrap();
        srv_dnd_state.stage_files(Edge::Right, vec![file3.clone()]);
        cap_tx
            .send(CaptureEvent::EdgeCrossed {
                edge: Edge::Right,
                normalized_pos: 0.5,
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(cap_state.remote_mode.load(Ordering::SeqCst));

        // Push cursor right back past client's Left return edge WITHOUT releasing LMB
        cap_tx
            .send(CaptureEvent::MouseDelta { dx: -40.0, dy: 0.0 })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(!cap_state.remote_mode.load(Ordering::SeqCst));
        assert!(
            !cli_recv_dir.join("cancel-me.bin").exists(),
            "cancelled drag must not leave files in Downloads/Glide"
        );
        assert_eq!(
            cli_dnd_state.delivered_drops.lock().len(),
            1,
            "cancelled drag must not trigger deliver_drop"
        );
        assert!(
            cli_dnd_state.incoming_cancels.load(Ordering::SeqCst) >= 1,
            "client DnD backend must be notified of cancellation"
        );

        // --- Scenario 3: Client -> Server cross-edge drag-and-drop -> release LMB on server ---
        // Enter client cleanly
        cap_tx
            .send(CaptureEvent::EdgeCrossed {
                edge: Edge::Right,
                normalized_pos: 0.5,
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        // Move inward (allow QUIC datagram Lane A to land before clicking on Lane B)
        cap_tx
            .send(CaptureEvent::MouseDelta { dx: 80.0, dy: 0.0 })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
        cap_tx
            .send(CaptureEvent::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(cli_dnd_state.armed.load(Ordering::SeqCst));

        // Client edge strip stages file4, then cursor pushes back across return edge (Left)
        cli_dnd_state.stage_files(Edge::Left, vec![file4.clone()]);
        cap_tx
            .send(CaptureEvent::MouseDelta { dx: -150.0, dy: 0.0 })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(!cap_state.remote_mode.load(Ordering::SeqCst));

        // User releases physical LMB on the Server desktop -> commits drop on Server!
        cap_tx
            .send(CaptureEvent::LocalLeftButton { pressed: false })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;

        {
            let srv_drops = srv_dnd_state.delivered_drops.lock();
            assert_eq!(srv_drops.len(), 1, "expected 1 drop handoff on server");
            assert_eq!(srv_drops[0].len(), 1);
            assert!(srv_drops[0][0].exists());
        }
        assert_eq!(
            std::fs::read(srv_recv_dir.join("from-client.txt")).unwrap(),
            b"dragged back from client to server!"
        );

        // --- Scenario 4: Disabling drag_drop_files live disables edge DnD while normal crossing works ---
        srv_cmd_tx
            .send(RuntimeCommand::UpdateServerTuning {
                edge_threshold_px: 4,
                dead_corner_px: 24,
                edge_dwell_ms: 0,
                auto_lock_fullscreen: true,
                locked: false,
                sync_lock_keys: true,
                pin_media_keys_local: true,
                clipboard: true,
                drag_drop_files: false,
                hotkeys: glide_backend::CaptureHotkeys::default(),
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!srv_dnd_state.enabled.load(Ordering::SeqCst));

        srv_dnd_state.stage_files(Edge::Right, vec![file3.clone()]);
        cap_tx
            .send(CaptureEvent::EdgeCrossed {
                edge: Edge::Right,
                normalized_pos: 0.5,
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            cap_state.remote_mode.load(Ordering::SeqCst),
            "normal edge crossing must still work when drag_drop_files is disabled"
        );
        cap_tx
            .send(CaptureEvent::MouseButton {
                button: MouseButton::Left,
                pressed: false,
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(cli_dnd_state.delivered_drops.lock().len(), 1);

        let _ = Arc::strong_count(&srv_dnd_state);
        cli_task.abort();
        srv_task.abort();
        let _ = std::fs::remove_dir_all(&root_tmp);
    }

    #[tokio::test]
    async fn server_to_client_file_transfer_survives_active_datagrams_and_rejects_oversized_without_recording_sent(
    ) {
        let identity = ServerIdentity::generate_ephemeral().unwrap();
        let fp = identity.fingerprint.clone();
        let endpoint =
            GlideServerEndpoint::bind("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = endpoint.local_addr();

        let geom = DesktopGeometry::new(vec![MonitorRect {
            name: "Disp-1".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            primary: true,
        }]);

        let (capture_handle, cap_tx, _cap_state) = create_mock_capture(geom.clone());
        let (mut injector, _inj_log) = MockInjector::new(geom);
        let srv_rt = RuntimeState::new();
        let cli_rt = RuntimeState::new();
        let (srv_cmd_tx, srv_cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (cli_cmd_tx, mut cli_cmd_rx) = tokio::sync::mpsc::unbounded_channel();

        let recv_dir = std::env::temp_dir().join(format!(
            "glide-e2e-bulk-cancel-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&recv_dir);

        let srv_cfg = ServerConfig {
            name: "bulk-srv".into(),
            discovery: false,
            motion_coalesce_us: 0,
            max_file_bytes: Some(4 * 1024 * 1024),
            ..Default::default()
        };
        let mut cli_cfg = ClientConfig {
            name: "bulk-cli".into(),
            server: Some(addr.to_string()),
            edge: Edge::Right,
            trusted_fingerprint: Some(fp),
            max_file_bytes: Some(128 * 1024), // start low, raise live via UpdateMaxFileBytes!
            receive_dir: Some(recv_dir.display().to_string()),
            ..Default::default()
        };

        let srv_rt_c = srv_rt.clone();
        let srv_task = tokio::spawn(async move {
            let _ = run_server_with_backends(
                srv_cfg,
                endpoint,
                capture_handle,
                None,
                "linux-x11",
                Some(srv_rt_c),
                Some(srv_cmd_rx),
            )
            .await;
        });

        let cli_rt_c = cli_rt.clone();
        let cli_task = tokio::spawn(async move {
            let mut opt_clip = None;
            let _ = run_client_session(
                &mut cli_cfg,
                addr,
                &mut injector,
                &mut opt_clip,
                Some(&cli_rt_c),
                Some(&mut cli_cmd_rx),
            )
            .await;
        });

        tokio::time::sleep(Duration::from_millis(120)).await;

        // 1. Live-update client max_file_bytes to 4 MiB so the dedicated bulk task accepts 2 MiB
        cli_cmd_tx
            .send(crate::runtime::RuntimeCommand::UpdateMaxFileBytes(
                4 * 1024 * 1024,
            ))
            .unwrap();
        // Enter remote mode so high-frequency datagrams stream continuously during the file transfer
        cap_tx
            .send(CaptureEvent::EdgeCrossed {
                edge: Edge::Right,
                normalized_pos: 0.5,
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // 2. Oversized SendFile (> 4 MiB) must NOT be recorded as "sent" in server RuntimeState
        srv_cmd_tx
            .send(crate::runtime::RuntimeCommand::SendFile {
                filename: "too-big.bin".into(),
                data: Bytes::from(vec![0xFFu8; 5 * 1024 * 1024]),
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(
            srv_rt.snapshot().recent_files.is_empty(),
            "oversized rejected transfer must not be recorded as sent"
        );

        // 3. Send a 2 MiB file from server -> client while hammering 1000Hz datagrams!
        let file_data: Vec<u8> = (0..(2 * 1024 * 1024)).map(|i| (i % 251) as u8).collect();
        srv_cmd_tx
            .send(crate::runtime::RuntimeCommand::SendFile {
                filename: "large-payload.bin".into(),
                data: Bytes::from(file_data.clone()),
            })
            .unwrap();

        for _ in 0..80 {
            let _ = cap_tx.send(CaptureEvent::MouseDelta { dx: 1.0, dy: 0.0 });
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        // Wait up to 2s for the 2 MiB file to arrive intact despite datagram + timer traffic
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if !cli_rt.snapshot().recent_files.is_empty() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for server->client 2 MiB file transfer"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        let cli_files = cli_rt.snapshot().recent_files;
        assert_eq!(cli_files.len(), 1);
        assert_eq!(cli_files[0].direction, "received");
        assert_eq!(cli_files[0].filename, "large-payload.bin");
        assert_eq!(cli_files[0].bytes, file_data.len());
        let saved = std::path::PathBuf::from(cli_files[0].saved_path.as_ref().unwrap());
        assert_eq!(std::fs::read(&saved).unwrap(), file_data);

        // Server recorded the clean sent transfer
        let srv_files = srv_rt.snapshot().recent_files;
        assert_eq!(srv_files.len(), 1);
        assert_eq!(srv_files[0].direction, "sent");
        assert_eq!(srv_files[0].filename, "large-payload.bin");

        cli_task.abort();
        srv_task.abort();
        let _ = std::fs::remove_dir_all(&recv_dir);
    }

    #[tokio::test]
    async fn switch_target_transitions_guard_enter_leave_and_noop_idempotency() {
        let identity = ServerIdentity::generate_ephemeral().unwrap();
        let fp = identity.fingerprint.clone();
        let endpoint =
            GlideServerEndpoint::bind("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = endpoint.local_addr();

        let geom = DesktopGeometry::new(vec![MonitorRect {
            name: "Disp-1".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            primary: true,
        }]);

        let (capture_handle, _cap_tx, cap_state) = create_mock_capture(geom.clone());
        let (mut right_inj, right_log) = MockInjector::new(geom.clone());
        let (mut left_inj, left_log) = MockInjector::new(geom);
        let srv_rt = RuntimeState::new();
        let (srv_cmd_tx, srv_cmd_rx) = tokio::sync::mpsc::unbounded_channel();

        let srv_cfg = ServerConfig {
            name: "switch-srv".into(),
            discovery: false,
            motion_coalesce_us: 0,
            ..Default::default()
        };
        let mut right_cfg = ClientConfig {
            name: "peer-right".into(),
            server: Some(addr.to_string()),
            edge: Edge::Right,
            trusted_fingerprint: Some(fp.clone()),
            ..Default::default()
        };
        let mut left_cfg = ClientConfig {
            name: "peer-left".into(),
            server: Some(addr.to_string()),
            edge: Edge::Left,
            trusted_fingerprint: Some(fp),
            ..Default::default()
        };

        let srv_rt_c = srv_rt.clone();
        let srv_task = tokio::spawn(async move {
            let _ = run_server_with_backends(
                srv_cfg,
                endpoint,
                capture_handle,
                None,
                "windows",
                Some(srv_rt_c),
                Some(srv_cmd_rx),
            )
            .await;
        });
        let right_task = tokio::spawn(async move {
            let mut opt_clip = None;
            let _ =
                run_client_session(&mut right_cfg, addr, &mut right_inj, &mut opt_clip, None, None)
                    .await;
        });
        let left_task = tokio::spawn(async move {
            let mut opt_clip = None;
            let _ =
                run_client_session(&mut left_cfg, addr, &mut left_inj, &mut opt_clip, None, None)
                    .await;
        });

        // Wait until both peers are registered
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while srv_rt.snapshot().peers.len() < 2 {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // 1. SwitchTarget(None) while already local must NOT invoke leave_remote_at
        srv_cmd_tx
            .send(crate::runtime::RuntimeCommand::SwitchTarget(None))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(cap_state.leave_remote_at_calls.load(Ordering::SeqCst), 0);
        assert_eq!(cap_state.enter_remote_calls.load(Ordering::SeqCst), 0);

        // 2. Local -> Right calls enter_remote exactly once and sends EnterAt to Right
        srv_cmd_tx
            .send(crate::runtime::RuntimeCommand::SwitchTarget(Some(
                Edge::Right,
            )))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(cap_state.remote_mode.load(Ordering::SeqCst));
        assert_eq!(cap_state.enter_remote_calls.load(Ordering::SeqCst), 1);
        assert_eq!(srv_rt.snapshot().active_target, "peer-right");
        let right_enters_before = right_log
            .lock()
            .iter()
            .filter(|a| matches!(a, InjectedAction::EnterAt(_, _)))
            .count();
        assert_eq!(right_enters_before, 1);

        // 3. Switching to Right again while already on Right is a no-op
        srv_cmd_tx
            .send(crate::runtime::RuntimeCommand::SwitchTarget(Some(
                Edge::Right,
            )))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(cap_state.enter_remote_calls.load(Ordering::SeqCst), 1);
        let right_enters_after = right_log
            .lock()
            .iter()
            .filter(|a| matches!(a, InjectedAction::EnterAt(_, _)))
            .count();
        assert_eq!(right_enters_after, 1);

        // 4. Right -> Left (remote -> remote) must NOT call enter_remote again!
        srv_cmd_tx
            .send(crate::runtime::RuntimeCommand::SwitchTarget(Some(
                Edge::Left,
            )))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(
            cap_state.enter_remote_calls.load(Ordering::SeqCst),
            1,
            "remote -> remote switch must not re-call enter_remote()"
        );
        assert_eq!(srv_rt.snapshot().active_target, "peer-left");
        assert!(right_log.lock().contains(&InjectedAction::ReleaseAll));
        assert!(left_log
            .lock()
            .iter()
            .any(|a| matches!(a, InjectedAction::EnterAt(_, _))));

        // 5. ReturnLocal transitions remote -> local and calls leave_remote_at once
        srv_cmd_tx
            .send(crate::runtime::RuntimeCommand::ReturnLocal)
            .unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(!cap_state.remote_mode.load(Ordering::SeqCst));
        assert_eq!(cap_state.leave_remote_at_calls.load(Ordering::SeqCst), 1);
        assert_eq!(srv_rt.snapshot().active_target, "Local");
        assert!(left_log.lock().contains(&InjectedAction::ReleaseAll));

        // 6. Second ReturnLocal while already local is a no-op
        srv_cmd_tx
            .send(crate::runtime::RuntimeCommand::ReturnLocal)
            .unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(cap_state.leave_remote_at_calls.load(Ordering::SeqCst), 1);

        right_task.abort();
        left_task.abort();
        srv_task.abort();
    }

    #[tokio::test]
    async fn live_server_tuning_hot_reloads_hotkeys_and_toggles_hotkeys_suspended() {
        use crate::runtime::RuntimeCommand;
        use glide_backend::{CaptureHotkeys, HotkeyChord, HotkeyMods};

        let identity = ServerIdentity::generate_ephemeral().unwrap();
        let endpoint =
            GlideServerEndpoint::bind("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let (capture_handle, _cap_tx, cap_state) =
            create_mock_capture(single_monitor_geom("HotkeysSrv"));
        let (srv_cmd_tx, srv_cmd_rx) = tokio::sync::mpsc::unbounded_channel();

        let srv_cfg = ServerConfig {
            name: "hotkeys-srv".into(),
            discovery: false,
            clipboard: false,
            ..Default::default()
        };
        let srv_task = tokio::spawn(async move {
            run_server_with_backends(
                srv_cfg,
                endpoint,
                capture_handle,
                None,
                "linux-x11",
                None,
                Some(srv_cmd_rx),
            )
            .await
        });

        // 1. Live UpdateServerTuning updates MockCaptureState.hotkeys immediately
        let custom_hotkeys = CaptureHotkeys {
            switch_mods: Some(HotkeyMods::CTRL | HotkeyMods::SHIFT),
            lock: Some(HotkeyChord::new(
                HotkeyMods::EMPTY,
                HidUsage::from_name("F9").unwrap(),
            )),
            escape: Some(HotkeyChord::new(
                HotkeyMods::CTRL | HotkeyMods::ALT,
                HidUsage::ESCAPE,
            )),
        };
        srv_cmd_tx
            .send(RuntimeCommand::UpdateServerTuning {
                edge_threshold_px: 4,
                dead_corner_px: 24,
                edge_dwell_ms: 0,
                auto_lock_fullscreen: true,
                locked: false,
                sync_lock_keys: true,
                pin_media_keys_local: true,
                clipboard: false,
                drag_drop_files: true,
                hotkeys: custom_hotkeys,
            })
            .unwrap();

        let mut updated = false;
        for _ in 0..30 {
            if *cap_state.hotkeys.lock() == custom_hotkeys {
                updated = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(updated, "UpdateServerTuning must hot-reload CaptureHotkeys");

        // 2. Disabling all shortcuts via UpdateServerTuning propagates cleanly
        srv_cmd_tx
            .send(RuntimeCommand::UpdateServerTuning {
                edge_threshold_px: 4,
                dead_corner_px: 24,
                edge_dwell_ms: 0,
                auto_lock_fullscreen: true,
                locked: false,
                sync_lock_keys: true,
                pin_media_keys_local: true,
                clipboard: false,
                drag_drop_files: true,
                hotkeys: CaptureHotkeys::disabled(),
            })
            .unwrap();
        let mut disabled = false;
        for _ in 0..30 {
            if *cap_state.hotkeys.lock() == CaptureHotkeys::disabled() {
                disabled = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            disabled,
            "UpdateServerTuning must support disabling all shortcuts live"
        );

        // 3. SetHotkeysSuspended toggles hotkeys_suspended on and off live
        assert!(!cap_state.hotkeys_suspended.load(Ordering::SeqCst));
        srv_cmd_tx
            .send(RuntimeCommand::SetHotkeysSuspended(true))
            .unwrap();
        for _ in 0..30 {
            if cap_state.hotkeys_suspended.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            cap_state.hotkeys_suspended.load(Ordering::SeqCst),
            "SetHotkeysSuspended(true) must suspend capture hotkeys"
        );

        srv_cmd_tx
            .send(RuntimeCommand::SetHotkeysSuspended(false))
            .unwrap();
        for _ in 0..30 {
            if !cap_state.hotkeys_suspended.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !cap_state.hotkeys_suspended.load(Ordering::SeqCst),
            "SetHotkeysSuspended(false) must resume capture hotkeys"
        );

        srv_cmd_tx.send(RuntimeCommand::Shutdown).unwrap();
        let res = tokio::time::timeout(Duration::from_millis(800), srv_task)
            .await
            .expect("server must shut down cleanly")
            .unwrap();
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn leading_edge_coalesce_and_cancel_safe_control_stream_under_datagram_burst() {
        let identity = ServerIdentity::generate_ephemeral().unwrap();
        let fp = identity.fingerprint.clone();
        let endpoint =
            GlideServerEndpoint::bind("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = endpoint.local_addr();

        let (capture_handle, cap_tx, cap_state) =
            create_mock_capture(single_monitor_geom("BurstSrv"));
        let (mut injector, inj_log) = MockInjector::new(single_monitor_geom("BurstCli"));

        let srv_cfg = ServerConfig {
            name: "burst-srv".into(),
            discovery: false,
            clipboard: false,
            // Long 250ms coalesce window to verify leading-edge 0ms flush with generous CI headroom!
            motion_coalesce_us: 250_000,
            ..Default::default()
        };
        let mut cli_cfg = ClientConfig {
            name: "burst-cli".into(),
            server: Some(addr.to_string()),
            edge: Edge::Right,
            trusted_fingerprint: Some(fp),
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
        let cli_task = tokio::spawn(async move {
            let mut opt_clip = None;
            let _ = run_client_session(&mut cli_cfg, addr, &mut injector, &mut opt_clip, None, None)
                .await;
        });

        for _ in 0..30 {
            if *cap_state.active_edges.lock() == vec![Edge::Right] {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(*cap_state.active_edges.lock(), vec![Edge::Right]);

        cap_tx
            .send(CaptureEvent::EdgeCrossed {
                edge: Edge::Right,
                normalized_pos: 0.5,
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;

        // 1. Leading-edge flush: first MouseDelta must arrive well BEFORE the 250ms coalesce window!
        let t0 = std::time::Instant::now();
        cap_tx
            .send(CaptureEvent::MouseDelta { dx: 15.0, dy: 5.0 })
            .unwrap();
        let mut got_leading = false;
        while t0.elapsed() < Duration::from_millis(150) {
            if inj_log.lock().contains(&InjectedAction::Move(15.0, 5.0)) {
                got_leading = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            got_leading,
            "first MouseDelta must flush on leading edge without waiting for the 250ms coalesce timer"
        );

        // 2. Interleave 100 rapid mouse datagrams + 30 control keystrokes to prove the dedicated
        //    ctrl_recv task never drops or corrupts framed control messages under datagram load.
        for i in 0..30u16 {
            let _ = cap_tx.send(CaptureEvent::MouseDelta { dx: 1.0, dy: 0.0 });
            let _ = cap_tx.send(CaptureEvent::Key {
                usage: HidUsage::keyboard(0x04 + (i % 10)),
                pressed: true,
            });
            let _ = cap_tx.send(CaptureEvent::MouseDelta { dx: 1.0, dy: 0.0 });
            let _ = cap_tx.send(CaptureEvent::Key {
                usage: HidUsage::keyboard(0x04 + (i % 10)),
                pressed: false,
            });
        }

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let key_events = inj_log
                .lock()
                .iter()
                .filter(|a| matches!(a, InjectedAction::Key(_, _)))
                .count();
            if key_events == 60 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "expected all 60 control key events intact under concurrent datagrams, got {key_events}"
            );
            tokio::time::sleep(Duration::from_millis(15)).await;
        }

        cli_task.abort();
        srv_task.abort();
    }

    #[tokio::test]
    async fn pending_drag_handoff_abort_releases_remote_mode_and_lmb_release_during_window_still_drops(
    ) {
        use crate::client::run_client_session_with_dnd;
        use crate::runtime::RuntimeCommand;
        use crate::server::run_server_with_dnd;
        use glide_backend::dnd::{DndBackend, DropDeliveryOutcome};
        use glide_backend::mock::create_mock_dnd;
        use std::sync::atomic::AtomicUsize;
        use std::sync::{Arc, Condvar, Mutex};
        use tokio::sync::Notify;

        /// Deterministic DnD wrapper that pauses inside `cancel_local_drag()` (on the blocking
        /// thread pool) until released, allowing tests to exercise events arriving during the
        /// async handoff window without blocking the Tokio executor thread.
        struct GatedCancelDnd {
            inner: Arc<dyn DndBackend>,
            entered_calls: Arc<AtomicUsize>,
            entered_notify: Arc<Notify>,
            release_gate: Arc<(Mutex<bool>, Condvar)>,
        }

        impl GatedCancelDnd {
            async fn wait_until_cancel_entered(&self, expected_calls: usize) {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
                while self.entered_calls.load(Ordering::SeqCst) < expected_calls {
                    let notified = self.entered_notify.notified();
                    if self.entered_calls.load(Ordering::SeqCst) >= expected_calls {
                        break;
                    }
                    let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
                    assert!(!rem.is_zero(), "timed out waiting for cancel_local_drag()");
                    let _ = tokio::time::timeout(rem, notified).await;
                }
            }

            fn release_cancel(&self) {
                let (lock, cv) = &*self.release_gate;
                *lock.lock().unwrap() = true;
                cv.notify_all();
            }

            fn reset_gate(&self) {
                let (lock, _) = &*self.release_gate;
                *lock.lock().unwrap() = false;
            }
        }

        impl DndBackend for GatedCancelDnd {
            fn set_enabled(&self, enabled: bool) {
                self.inner.set_enabled(enabled);
            }
            fn set_active_edges(&self, edges: &[Edge], dead_corner_px: i32) {
                self.inner.set_active_edges(edges, dead_corner_px);
            }
            fn set_drag_armed(&self, armed: bool) {
                self.inner.set_drag_armed(armed);
            }
            fn staged_edge_mask(&self) -> u8 {
                self.inner.staged_edge_mask()
            }
            fn take_staged_files(&self, edge: Edge) -> Option<Vec<std::path::PathBuf>> {
                self.inner.take_staged_files(edge)
            }
            fn cancel_local_drag(&self) {
                self.inner.cancel_local_drag();
                self.entered_calls.fetch_add(1, Ordering::SeqCst);
                self.entered_notify.notify_waiters();
                let (lock, cv) = &*self.release_gate;
                let mut released = lock.lock().unwrap();
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while !*released {
                    let rem = deadline.saturating_duration_since(std::time::Instant::now());
                    if rem.is_zero() {
                        break;
                    }
                    let (g, _) = cv.wait_timeout(released, rem).unwrap();
                    released = g;
                }
            }
            fn on_incoming_drag_cancel(&self) {
                self.inner.on_incoming_drag_cancel();
            }
            fn deliver_drop(
                &self,
                paths: Vec<std::path::PathBuf>,
            ) -> anyhow::Result<DropDeliveryOutcome> {
                self.inner.deliver_drop(paths)
            }
        }

        let unique = format!(
            "glide-handoff-abort-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root_tmp = std::env::temp_dir().join(unique);
        let src_file = root_tmp.join("quick-drop.txt");
        let cli_recv_dir = root_tmp.join("client-recv");
        std::fs::create_dir_all(&cli_recv_dir).unwrap();
        std::fs::write(&src_file, b"dropped during cancel window").unwrap();

        let identity = ServerIdentity::generate_ephemeral().unwrap();
        let fp = identity.fingerprint.clone();
        let endpoint =
            GlideServerEndpoint::bind("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = endpoint.local_addr();

        let geom = single_monitor_geom("Handoff");
        let (capture_handle, cap_tx, cap_state) = create_mock_capture(geom.clone());
        let (mut injector, _inj_log) = MockInjector::new(geom);
        let (srv_dnd_inner, srv_dnd_state) = create_mock_dnd();
        let gated_dnd = Arc::new(GatedCancelDnd {
            inner: srv_dnd_inner,
            entered_calls: Arc::new(AtomicUsize::new(0)),
            entered_notify: Arc::new(Notify::new()),
            release_gate: Arc::new((Mutex::new(false), Condvar::new())),
        });
        let (cli_dnd, cli_dnd_state) = create_mock_dnd();
        let (srv_cmd_tx, srv_cmd_rx) = tokio::sync::mpsc::unbounded_channel();

        let srv_cfg = ServerConfig {
            name: "handoff-srv".into(),
            discovery: false,
            clipboard: false,
            motion_coalesce_us: 0,
            drag_drop_files: true,
            ..Default::default()
        };
        let mut cli_cfg = ClientConfig {
            name: "handoff-cli".into(),
            server: Some(addr.to_string()),
            edge: Edge::Right,
            trusted_fingerprint: Some(fp),
            clipboard: false,
            drag_drop_files: true,
            ..Default::default()
        };

        let srv_dnd_trait: Arc<dyn DndBackend> = gated_dnd.clone();
        let srv_task = tokio::spawn(async move {
            let _ = run_server_with_dnd(
                srv_cfg,
                endpoint,
                capture_handle,
                None,
                srv_dnd_trait,
                None,
                "windows",
                None,
                Some(srv_cmd_rx),
            )
            .await;
        });

        let cli_recv_c = cli_recv_dir.clone();
        let cli_task = tokio::spawn(async move {
            let mut opt_clip = None;
            let _ = run_client_session_with_dnd(
                &mut cli_cfg,
                addr,
                &mut injector,
                &mut opt_clip,
                cli_dnd,
                Some(cli_recv_c),
                None,
                None,
            )
            .await;
        });

        for _ in 0..40 {
            if *cap_state.active_edges.lock() == vec![Edge::Right] {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(*cap_state.active_edges.lock(), vec![Edge::Right]);

        // --- Case 1 (Blocker #1): Locking cursor while pending_drag_handoff is in-flight
        //     must call leave_remote() immediately so remote_mode is never wedged! ---
        srv_dnd_state.stage_files(Edge::Right, vec![src_file.clone()]);
        // Simulate real OS capture flipping remote_mode=true upon emitting EdgeCrossed.
        cap_state.remote_mode.store(true, Ordering::SeqCst);
        *cap_state.last_leave.lock() = None;
        cap_tx
            .send(CaptureEvent::LocalLeftButton { pressed: true })
            .unwrap();
        cap_tx
            .send(CaptureEvent::EdgeCrossed {
                edge: Edge::Right,
                normalized_pos: 0.42,
            })
            .unwrap();
        gated_dnd.wait_until_cancel_entered(1).await;

        srv_cmd_tx.send(RuntimeCommand::SetLock(true)).unwrap();
        for _ in 0..40 {
            if cap_state.locked.load(Ordering::SeqCst)
                && !cap_state.remote_mode.load(Ordering::SeqCst)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !cap_state.remote_mode.load(Ordering::SeqCst),
            "aborting pending_drag_handoff via SetLock(true) must release remote_mode"
        );
        assert_eq!(*cap_state.last_leave.lock(), Some((Edge::Right, 0.42)));
        assert_eq!(cap_state.enter_remote_calls.load(Ordering::SeqCst), 0);

        // Unblock cancel_local_drag; stale DragHandoffReady must be ignored cleanly.
        gated_dnd.release_cancel();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!cap_state.remote_mode.load(Ordering::SeqCst));
        assert_eq!(cap_state.enter_remote_calls.load(Ordering::SeqCst), 0);
        assert!(cli_dnd_state.delivered_drops.lock().is_empty());

        // --- Case 2 (Blocker #1 + #2): Escape pressed during cancel_local_drag aborts cleanly ---
        srv_cmd_tx.send(RuntimeCommand::SetLock(false)).unwrap();
        for _ in 0..30 {
            if !cap_state.locked.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        gated_dnd.reset_gate();
        srv_dnd_state.stage_files(Edge::Right, vec![src_file.clone()]);
        cap_state.remote_mode.store(true, Ordering::SeqCst);
        *cap_state.last_leave.lock() = None;
        cap_tx
            .send(CaptureEvent::LocalLeftButton { pressed: true })
            .unwrap();
        cap_tx
            .send(CaptureEvent::EdgeCrossed {
                edge: Edge::Right,
                normalized_pos: 0.35,
            })
            .unwrap();
        gated_dnd.wait_until_cancel_entered(2).await;
        cap_tx
            .send(CaptureEvent::Key {
                usage: HidUsage::ESCAPE,
                pressed: true,
            })
            .unwrap();
        for _ in 0..40 {
            if !cap_state.remote_mode.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !cap_state.remote_mode.load(Ordering::SeqCst),
            "Escape during pending_drag_handoff must call leave_remote and clear remote_mode"
        );
        assert_eq!(*cap_state.last_leave.lock(), Some((Edge::Right, 0.35)));
        gated_dnd.release_cancel();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(cli_dnd_state.delivered_drops.lock().is_empty());

        // --- Case 3 (Blocker #2): Releasing LMB during the 20ms cancel_local_drag window
        //     must still commit DragDrop right after DragStart! ---
        gated_dnd.reset_gate();
        srv_dnd_state.stage_files(Edge::Right, vec![src_file.clone()]);
        cap_tx
            .send(CaptureEvent::LocalLeftButton { pressed: true })
            .unwrap();
        cap_tx
            .send(CaptureEvent::EdgeCrossed {
                edge: Edge::Right,
                normalized_pos: 0.5,
            })
            .unwrap();
        gated_dnd.wait_until_cancel_entered(3).await;

        // User releases LMB BEFORE cancel_local_drag() completes!
        cap_tx
            .send(CaptureEvent::MouseButton {
                button: MouseButton::Left,
                pressed: false,
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        gated_dnd.release_cancel();

        let mut delivered = false;
        for _ in 0..50 {
            if cli_dnd_state.delivered_drops.lock().len() == 1 {
                delivered = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            delivered,
            "LMB release during pending_drag_handoff cancel window must still send DragDrop"
        );
        assert_eq!(
            std::fs::read(cli_recv_dir.join("quick-drop.txt")).unwrap(),
            b"dropped during cancel window"
        );

        cli_task.abort();
        srv_task.abort();
        let _ = std::fs::remove_dir_all(&root_tmp);
    }
}
