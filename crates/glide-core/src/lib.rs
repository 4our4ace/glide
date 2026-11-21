//! Core platform-agnostic orchestration engine for Glide.

pub mod client;
pub mod config;
pub mod dnd;
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
            cli_recent.iter().any(|f| f.filename == "Büro Grüße äöüß.txt" && f.direction == "received"),
            "received drag file should appear in RuntimeState::recent_files"
        );
        let srv_recent = srv_rt.snapshot().recent_files;
        assert!(
            srv_recent.iter().any(|f| f.filename == "Büro Grüße äöüß.txt" && f.direction == "sent"),
            "sent drag file should appear in server RuntimeState::recent_files"
        );

        // Return focus back to server for Scenario 2
        cap_tx
            .send(CaptureEvent::MouseDelta { dx: -200.0, dy: 0.0 })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!cap_state.remote_mode.load(Ordering::SeqCst));

        // --- Scenario 2: Drag across edge -> return to origin before releasing LMB -> cancel! ---
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
        // Move inward, press LMB on a file on client
        cap_tx
            .send(CaptureEvent::MouseDelta { dx: 80.0, dy: 0.0 })
            .unwrap();
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
                clipboard: true,
                drag_drop_files: false,
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
}
