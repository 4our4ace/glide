//! Zero-config LAN auto-discovery (LocalSend-style UDP multicast + broadcast).
//!
//! Servers announce their real machine hostname, QUIC port, OS, and TLS fingerprint
//! on multicast `239.255.71.76:24801` and broadcast `255.255.255.255:24801`, and
//! reply immediately to active client probes so clients connect without typing IPs.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tracing::{debug, trace, warn};

/// Multicast group `239.255.71.76` (`71, 76` = ASCII `"GL"`).
pub const DISCOVERY_MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 71, 76);
pub const DEFAULT_DISCOVERY_PORT: u16 = 24801;

const MAGIC_BEACON: &str = "GLIDE_BEACON_V1";
const MAGIC_PROBE: &[u8] = b"GLIDE_DISCOVER_V1";

/// Beacon packet broadcast by running Glide servers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerBeacon {
    pub magic: String,
    pub name: String,
    pub quic_port: u16,
    pub fingerprint: String,
    pub os: String,
}

/// A discovered Glide server on the local network.
#[derive(Debug, Clone)]
pub struct DiscoveredServer {
    pub addr: SocketAddr,
    pub name: String,
    pub fingerprint: String,
    pub os: String,
}

/// Returns the machine's real hostname (no goofy aliases).
pub fn machine_hostname() -> String {
    if let Ok(h) = std::env::var("COMPUTERNAME") {
        let t = h.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Ok(h) = std::env::var("HOSTNAME") {
        let t = h.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Ok(contents) = std::fs::read_to_string("/etc/hostname") {
        let t = contents.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    "glide-host".to_string()
}

fn bind_discovery_socket(port: u16, join_multicast: bool) -> Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("failed to create discovery UDP socket")?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    {
        let _ = sock.set_reuse_port(true);
    }
    let _ = sock.set_broadcast(true);
    sock.set_nonblocking(true)?;

    let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
    sock.bind(&bind_addr.into())
        .with_context(|| format!("failed to bind discovery UDP socket on {bind_addr}"))?;

    if join_multicast {
        if let Err(e) = sock.join_multicast_v4(&DISCOVERY_MULTICAST_ADDR, &Ipv4Addr::UNSPECIFIED) {
            debug!(error = %e, "Could not join multicast group (broadcast fallback still active)");
        }
        let _ = sock.set_multicast_loop_v4(true);
    }

    let std_sock: std::net::UdpSocket = sock.into();
    UdpSocket::from_std(std_sock).context("failed to convert discovery socket to tokio UdpSocket")
}

/// Spawns the server-side LAN discovery responder (replies immediately to probes and
/// periodically broadcasts presence every 1.5s).
pub fn spawn_server_beacon(
    discovery_port: u16,
    name: String,
    quic_port: u16,
    fingerprint: String,
    os: String,
) -> Result<JoinHandle<()>> {
    let socket = bind_discovery_socket(discovery_port, true)?;
    let beacon = ServerBeacon {
        magic: MAGIC_BEACON.into(),
        name,
        quic_port,
        fingerprint,
        os,
    };
    let payload = serde_json::to_vec(&beacon).context("failed to serialize server beacon")?;
    let multicast_dest =
        SocketAddr::V4(SocketAddrV4::new(DISCOVERY_MULTICAST_ADDR, discovery_port));
    let broadcast_dest = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, discovery_port));

    let handle = tokio::spawn(async move {
        let mut buf = [0u8; 1024];
        let mut interval = tokio::time::interval(Duration::from_millis(1500));

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let _ = socket.send_to(&payload, multicast_dest).await;
                    let _ = socket.send_to(&payload, broadcast_dest).await;
                }
                recv = socket.recv_from(&mut buf) => {
                    match recv {
                        Ok((n, src)) => {
                            if &buf[..n] == MAGIC_PROBE {
                                trace!(%src, "Received LAN discovery probe; replying with beacon");
                                let _ = socket.send_to(&payload, src).await;
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "Discovery socket recv error");
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        }
                    }
                }
            }
        }
    });

    Ok(handle)
}

/// Actively probes the local network for running Glide servers for up to `timeout`.
/// If `stop_on_first` is true, returns as soon as a matching server responds!
pub async fn discover_servers(
    discovery_port: u16,
    timeout: Duration,
    name_filter: Option<&str>,
    stop_on_first: bool,
) -> Result<Vec<DiscoveredServer>> {
    // Bind an ephemeral port for probe/reply AND optionally listen on discovery_port for unsolicited beacons.
    let probe_sock = bind_discovery_socket(0, false)?;
    let beacon_sock = bind_discovery_socket(discovery_port, true).ok();

    let multicast_dest =
        SocketAddr::V4(SocketAddrV4::new(DISCOVERY_MULTICAST_ADDR, discovery_port));
    let broadcast_dest = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, discovery_port));
    let loopback_dest = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, discovery_port));

    // Send immediate probes (multicast + broadcast + localhost for single-machine testing).
    let _ = probe_sock.send_to(MAGIC_PROBE, multicast_dest).await;
    let _ = probe_sock.send_to(MAGIC_PROBE, broadcast_dest).await;
    let _ = probe_sock.send_to(MAGIC_PROBE, loopback_dest).await;

    let mut found: HashMap<String, DiscoveredServer> = HashMap::new();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut probe_retry = tokio::time::interval(Duration::from_millis(400));
    probe_retry.tick().await; // consume immediate tick

    let mut buf_a = [0u8; 2048];
    let mut buf_b = [0u8; 2048];

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline - now;

        let packet = tokio::select! {
            _ = tokio::time::sleep(remaining) => break,
            _ = probe_retry.tick() => {
                let _ = probe_sock.send_to(MAGIC_PROBE, multicast_dest).await;
                let _ = probe_sock.send_to(MAGIC_PROBE, broadcast_dest).await;
                continue;
            }
            res = probe_sock.recv_from(&mut buf_a) => {
                res.ok().map(|(n, src)| (&buf_a[..n], src))
            }
            res = async {
                match &beacon_sock {
                    Some(s) => s.recv_from(&mut buf_b).await.ok(),
                    None => std::future::pending().await,
                }
            } => {
                res.map(|(n, src)| (&buf_b[..n], src))
            }
        };

        if let Some((data, src)) = packet {
            if let Ok(beacon) = serde_json::from_slice::<ServerBeacon>(data) {
                if beacon.magic != MAGIC_BEACON {
                    continue;
                }
                if let Some(filter) = name_filter {
                    if !beacon.name.eq_ignore_ascii_case(filter) {
                        continue;
                    }
                }
                let addr = SocketAddr::new(src.ip(), beacon.quic_port);
                let key = format!("{}:{}", beacon.fingerprint, addr);
                found.insert(
                    key,
                    DiscoveredServer {
                        addr,
                        name: beacon.name,
                        fingerprint: beacon.fingerprint,
                        os: beacon.os,
                    },
                );
                if stop_on_first && !found.is_empty() {
                    break;
                }
            }
        }
    }

    let mut list: Vec<DiscoveredServer> = found.into_values().collect();
    list.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.addr.cmp(&b.addr)));
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lan_discovery_finds_local_server_beacon() {
        let port = 24891;
        let handle = spawn_server_beacon(
            port,
            "win11-workstation".into(),
            24800,
            "aa:bb:cc:dd".into(),
            "windows".into(),
        )
        .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        let servers = discover_servers(port, Duration::from_secs(2), None, true)
            .await
            .unwrap();
        handle.abort();

        assert!(!servers.is_empty(), "expected to discover local beacon");
        assert_eq!(servers[0].name, "win11-workstation");
        assert_eq!(servers[0].addr.port(), 24800);
        assert_eq!(servers[0].fingerprint, "aa:bb:cc:dd");
    }
}
