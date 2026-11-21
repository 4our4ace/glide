//! Zero-config, offline-first LAN & direct-link auto-discovery.
//!
//! Designed to work reliably even when Wi-Fi / internet is completely down
//! (e.g. Ethernet LAN, USB-Ethernet tether, or direct link-local `169.254.x.x` cable)
//! and on multi-homed Windows 11 / Ubuntu machines (with WSL2, Hyper-V, Docker, VPNs):
//!
//! - Enumerates EVERY active non-loopback IPv4 network interface (re-refreshed periodically).
//! - Sends & listens using per-interface sockets bound to each NIC's local IPv4 address:
//!   1. Subnet-directed broadcast (e.g. `192.168.178.255`, `169.254.255.255`)
//!   2. IPv4 Multicast (`239.255.71.76`) with `IP_MULTICAST_IF` explicitly set per NIC
//!   3. Global limited broadcast (`255.255.255.255`)
//! - Zero external DNS, zero internet, zero mDNS/Avahi/Bonjour daemon required.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tracing::{debug, trace, warn};

/// Multicast group `239.255.71.76` (`71, 76` = ASCII `"GL"`).
pub const DISCOVERY_MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 71, 76);
pub const DEFAULT_DISCOVERY_PORT: u16 = 24801;

const MAGIC_BEACON: &str = "GLIDE_BEACON_V1";
const MAGIC_PROBE: &[u8] = b"GLIDE_DISCOVER_V1";
const MAX_DISCOVERED_SERVERS: usize = 64;
const MAX_BEACON_NAME_CHARS: usize = 64;
const MAX_BEACON_OS_CHARS: usize = 32;
const MAX_BEACON_IPS: usize = 16;
const PROBE_PER_SRC_MIN_INTERVAL: Duration = Duration::from_millis(250);
const PROBE_BURST_WINDOW: Duration = Duration::from_secs(1);
const PROBE_BURST_CAP: u32 = 10;
const PROBE_BROADCAST_MIN_INTERVAL: Duration = Duration::from_millis(500);
const PROBE_TRACKED_SOURCES_CAP: usize = 256;

/// Active local IPv4 interface with its computed subnet broadcast address.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LocalIpv4Iface {
    pub name: String,
    pub ip: Ipv4Addr,
    pub broadcast: Ipv4Addr,
}

/// Beacon packet broadcast by running Glide servers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerBeacon {
    pub magic: String,
    pub name: String,
    pub quic_port: u16,
    pub fingerprint: String,
    pub os: String,
    #[serde(default)]
    pub ips: Vec<String>,
}

/// A discovered Glide server on the local network or direct link.
#[derive(Debug, Clone)]
pub struct DiscoveredServer {
    pub addr: SocketAddr,
    pub name: String,
    pub fingerprint: String,
    pub os: String,
    pub all_ips: Vec<String>,
}

fn sanitize_beacon_label(raw: &str, max_chars: usize) -> String {
    raw.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(max_chars)
        .collect()
}

/// Validates and canonicalizes a SHA-256 fingerprint carried in a LAN discovery beacon.
/// Accepts either 64 raw ASCII hex digits or 32 colon-separated hex byte pairs (`xx:xx:...`).
pub(crate) fn canonicalize_beacon_fingerprint(raw: &str) -> Option<String> {
    let hex_only: String = if raw.contains(':') {
        if raw.len() != 95 {
            return None;
        }
        let bytes = raw.as_bytes();
        for (idx, &b) in bytes.iter().enumerate() {
            if idx % 3 == 2 {
                if b != b':' {
                    return None;
                }
            } else if !b.is_ascii_hexdigit() {
                return None;
            }
        }
        return Some(raw.to_ascii_lowercase());
    } else {
        if raw.len() != 64 || !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        raw.to_ascii_lowercase()
    };

    let mut formatted = String::with_capacity(95);
    for (idx, chunk) in hex_only.as_bytes().chunks_exact(2).enumerate() {
        if idx > 0 {
            formatted.push(':');
        }
        formatted.push(chunk[0] as char);
        formatted.push(chunk[1] as char);
    }
    Some(formatted)
}

fn validate_beacon(beacon: ServerBeacon, src_ip: std::net::IpAddr) -> Option<DiscoveredServer> {
    if beacon.magic != MAGIC_BEACON || beacon.quic_port == 0 {
        return None;
    }
    let fingerprint = canonicalize_beacon_fingerprint(&beacon.fingerprint)?;
    let name = sanitize_beacon_label(&beacon.name, MAX_BEACON_NAME_CHARS);
    if name.is_empty() {
        return None;
    }
    let os_clean = sanitize_beacon_label(&beacon.os, MAX_BEACON_OS_CHARS);
    let os = if os_clean.is_empty() {
        "unknown".to_string()
    } else {
        os_clean
    };
    let mut all_ips = Vec::new();
    for raw_ip in beacon.ips {
        if all_ips.len() >= MAX_BEACON_IPS {
            break;
        }
        if let Ok(parsed) = raw_ip.trim().parse::<std::net::IpAddr>() {
            let s = parsed.to_string();
            if !all_ips.contains(&s) {
                all_ips.push(s);
            }
        }
    }
    Some(DiscoveredServer {
        addr: SocketAddr::new(src_ip, beacon.quic_port),
        name,
        fingerprint,
        os,
        all_ips,
    })
}

fn insert_discovered_server(
    found: &mut HashMap<String, DiscoveredServer>,
    server: DiscoveredServer,
) {
    let key = server.fingerprint.clone();
    match found.get(&key) {
        None => {
            if found.len() < MAX_DISCOVERED_SERVERS {
                found.insert(key, server);
            }
        }
        Some(existing) => {
            if existing.addr.ip().is_loopback() && !server.addr.ip().is_loopback() {
                found.insert(key, server);
            }
        }
    }
}

struct ProbeRateLimiter {
    per_source: HashMap<std::net::IpAddr, Instant>,
    window_start: Instant,
    window_count: u32,
    last_broadcast: Option<Instant>,
}

impl ProbeRateLimiter {
    fn new(now: Instant) -> Self {
        Self {
            per_source: HashMap::new(),
            window_start: now,
            window_count: 0,
            last_broadcast: None,
        }
    }

    /// Returns `(allow_unicast_reply, allow_broadcast_reply)`.
    fn check(&mut self, src_ip: std::net::IpAddr, now: Instant) -> (bool, bool) {
        if now.saturating_duration_since(self.window_start) >= PROBE_BURST_WINDOW {
            self.window_start = now;
            self.window_count = 0;
        }
        if self.window_count >= PROBE_BURST_CAP {
            return (false, false);
        }
        if let Some(&last) = self.per_source.get(&src_ip) {
            if now.saturating_duration_since(last) < PROBE_PER_SRC_MIN_INTERVAL {
                return (false, false);
            }
        }
        if self.per_source.len() >= PROBE_TRACKED_SOURCES_CAP {
            self.per_source
                .retain(|_, ts| now.saturating_duration_since(*ts) < Duration::from_secs(5));
            if self.per_source.len() >= PROBE_TRACKED_SOURCES_CAP {
                self.per_source.clear();
            }
        }
        self.per_source.insert(src_ip, now);
        self.window_count += 1;

        let allow_bcast = self.last_broadcast.is_none_or(|ts| {
            now.saturating_duration_since(ts) >= PROBE_BROADCAST_MIN_INTERVAL
        });
        if allow_bcast {
            self.last_broadcast = Some(now);
        }
        (true, allow_bcast)
    }
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

/// Enumerates all active non-loopback IPv4 interfaces (including link-local `169.254.x.x`
/// for direct Ethernet/USB cables with no router or Wi-Fi!).
pub fn active_ipv4_interfaces() -> Vec<LocalIpv4Iface> {
    let mut out = Vec::new();
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return out;
    };
    for iface in ifaces {
        if iface.is_loopback() {
            continue;
        }
        if let if_addrs::IfAddr::V4(v4) = iface.addr {
            let ip = v4.ip;
            if ip.is_unspecified() || ip.is_loopback() {
                continue;
            }
            let broadcast = v4.broadcast.unwrap_or_else(|| {
                let ip_u = u32::from(ip);
                let mask_u = u32::from(v4.netmask);
                Ipv4Addr::from((ip_u & mask_u) | !mask_u)
            });
            out.push(LocalIpv4Iface {
                name: iface.name,
                ip,
                broadcast,
            });
        }
    }
    out
}

fn is_virtual_iface_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("vethernet")
        || n.contains("wsl")
        || n.contains("hyper-v")
        || n.contains("vmware")
        || n.contains("virtualbox")
        || n.contains("vbox")
        || n.starts_with("docker")
        || n.starts_with("br-")
        || n.starts_with("veth")
        || n.starts_with("virbr")
        || n.starts_with("tailscale")
        || n.starts_with("wg")
        || n.starts_with("tun")
        || n.starts_with("tap")
        || n.starts_with("zt")
}

fn lan_priority(iface: &LocalIpv4Iface) -> (u8, String) {
    let octets = iface.ip.octets();
    let virt = is_virtual_iface_name(&iface.name);
    let tier = if !virt && octets[0] == 192 && octets[1] == 168 {
        0
    } else if !virt && octets[0] == 10 {
        1
    } else if !virt && octets[0] == 172 && (16..=31).contains(&octets[1]) {
        2
    } else if !virt && octets[0] == 169 && octets[1] == 254 {
        3
    } else if !virt {
        4
    } else {
        5
    };
    (tier, iface.ip.to_string())
}

fn ranked_ipv4_addresses(ifaces: &[LocalIpv4Iface]) -> Vec<String> {
    let mut ranked: Vec<(u8, String)> = ifaces.iter().map(lan_priority).collect();
    ranked.sort();
    let mut ips: Vec<String> = Vec::with_capacity(ranked.len());
    for (_, ip) in ranked {
        if !ips.contains(&ip) {
            ips.push(ip);
        }
    }
    ips
}

/// Convenience helper returning active local IPv4 addresses ordered with physical LAN IPs first.
pub fn local_ipv4_addresses() -> Vec<String> {
    ranked_ipv4_addresses(&active_ipv4_interfaces())
}

/// Binds a listener UDP socket on `0.0.0.0:port` and joins multicast on EVERY active IPv4 NIC.
fn bind_listener_socket_with_ifaces(
    port: u16,
    ifaces: &[LocalIpv4Iface],
) -> Result<(UdpSocket, HashSet<Ipv4Addr>)> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("failed to create discovery listener socket")?;
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

    let mut joined = HashSet::new();
    let _ = sock.set_multicast_loop_v4(true);
    for iface in ifaces {
        if sock
            .join_multicast_v4(&DISCOVERY_MULTICAST_ADDR, &iface.ip)
            .is_ok()
        {
            joined.insert(iface.ip);
        }
    }
    // Also attempt default join as safety fallback
    let _ = sock.join_multicast_v4(&DISCOVERY_MULTICAST_ADDR, &Ipv4Addr::UNSPECIFIED);

    let std_sock: std::net::UdpSocket = sock.into();
    let tokio_sock = UdpSocket::from_std(std_sock)?;
    Ok((tokio_sock, joined))
}

fn bind_interface_sender(ip: Ipv4Addr) -> Option<std::net::UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).ok()?;
    let _ = sock.set_broadcast(true);
    let _ = sock.set_multicast_if_v4(&ip);
    let _ = sock.set_multicast_ttl_v4(2);
    let _ = sock.set_multicast_loop_v4(true);
    sock.bind(&SocketAddrV4::new(ip, 0).into()).ok()?;
    Some(sock.into())
}

/// Caches interface-bound ephemeral UDP sockets so periodic beacons and probe retries do not
/// churn sockets (`socket`/`setsockopt`/`bind`/`close`) or re-enumerate NICs on every tick.
struct BroadcastBlaster {
    sockets: HashMap<Ipv4Addr, (Ipv4Addr, std::net::UdpSocket)>,
    loopback: Option<std::net::UdpSocket>,
}

impl BroadcastBlaster {
    fn new(ifaces: &[LocalIpv4Iface]) -> Self {
        let mut me = Self {
            sockets: HashMap::with_capacity(ifaces.len()),
            loopback: std::net::UdpSocket::bind("127.0.0.1:0").ok(),
        };
        me.refresh(ifaces);
        me
    }

    fn refresh(&mut self, ifaces: &[LocalIpv4Iface]) {
        self.sockets
            .retain(|ip, _| ifaces.iter().any(|i| i.ip == *ip));
        for iface in ifaces {
            if let Some((bcast, _)) = self.sockets.get_mut(&iface.ip) {
                *bcast = iface.broadcast;
            } else if let Some(sock) = bind_interface_sender(iface.ip) {
                self.sockets.insert(iface.ip, (iface.broadcast, sock));
            }
        }
        if self.loopback.is_none() {
            self.loopback = std::net::UdpSocket::bind("127.0.0.1:0").ok();
        }
    }

    /// Sends `payload` out of every cached non-loopback IPv4 interface (subnet broadcast +
    /// multicast + limited broadcast), plus loopback when requested.
    fn blast(&mut self, payload: &[u8], port: u16, include_loopback: bool) {
        let mcast_dest = SocketAddrV4::new(DISCOVERY_MULTICAST_ADDR, port);
        let bcast_dest = SocketAddrV4::new(Ipv4Addr::BROADCAST, port);

        // Drop any socket whose interface went down (e.g. after sleep/wake) so the next refresh rebinds it.
        self.sockets.retain(|_, (iface_bcast, std_sock)| {
            // 1. Subnet-directed broadcast (most reliable across Windows Firewall & home routers & direct cables)
            let r1 = std_sock.send_to(payload, SocketAddrV4::new(*iface_bcast, port));
            // 2. Multicast explicitly routed out THIS NIC
            let r2 = std_sock.send_to(payload, mcast_dest);
            // 3. Global broadcast explicitly routed out THIS NIC
            let r3 = std_sock.send_to(payload, bcast_dest);
            r1.is_ok() || r2.is_ok() || r3.is_ok()
        });

        if include_loopback {
            if let Some(ref sock) = self.loopback {
                if sock
                    .send_to(payload, SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
                    .is_err()
                {
                    self.loopback = None;
                }
            }
        }
    }
}

/// Spawns the server-side LAN discovery responder.
///
/// - Broadcasts beacons every 500 ms across EVERY active IPv4 interface (Ethernet, Wi-Fi, link-local)
/// - Replies immediately to active `GLIDE_DISCOVER_V1` probes (rate-limited per source and globally)
/// - Periodically re-scans interfaces every 3 s so newly plugged cables / dropped Wi-Fi recover automatically!
pub fn spawn_server_beacon(
    discovery_port: u16,
    name: String,
    quic_port: u16,
    fingerprint: String,
    os: String,
) -> Result<JoinHandle<()>> {
    let initial_ifaces = active_ipv4_interfaces();
    let (socket, mut joined_ifaces) =
        bind_listener_socket_with_ifaces(discovery_port, &initial_ifaces)?;

    let handle = tokio::spawn(async move {
        let mut buf = [0u8; 1024];
        let mut beacon_tick = tokio::time::interval(Duration::from_millis(500));
        let mut last_iface_refresh = Instant::now();
        let mut rate_limiter = ProbeRateLimiter::new(Instant::now());
        let mut blaster = BroadcastBlaster::new(&initial_ifaces);

        let make_payload = |ips: &[String]| -> Vec<u8> {
            let beacon = ServerBeacon {
                magic: MAGIC_BEACON.into(),
                name: name.clone(),
                quic_port,
                fingerprint: fingerprint.clone(),
                os: os.clone(),
                ips: ips.to_vec(),
            };
            serde_json::to_vec(&beacon).unwrap_or_default()
        };

        let mut cached_ips = ranked_ipv4_addresses(&initial_ifaces);
        let mut cached_payload = make_payload(&cached_ips);

        loop {
            tokio::select! {
                _ = beacon_tick.tick() => {
                    if last_iface_refresh.elapsed() >= Duration::from_secs(3) {
                        last_iface_refresh = Instant::now();
                        let current = active_ipv4_interfaces();
                        joined_ifaces.retain(|ip| current.iter().any(|i| i.ip == *ip));
                        for iface in &current {
                            if !joined_ifaces.contains(&iface.ip)
                                && socket.join_multicast_v4(DISCOVERY_MULTICAST_ADDR, iface.ip).is_ok()
                            {
                                debug!(ip = %iface.ip, iface = %iface.name, "Joined discovery multicast on new interface");
                                joined_ifaces.insert(iface.ip);
                            }
                        }
                        blaster.refresh(&current);
                        let new_ips = ranked_ipv4_addresses(&current);
                        if new_ips != cached_ips {
                            cached_ips = new_ips;
                            cached_payload = make_payload(&cached_ips);
                        }
                    }
                    blaster.blast(&cached_payload, discovery_port, true);
                }
                recv = socket.recv_from(&mut buf) => {
                    match recv {
                        Ok((n, src)) => {
                            if &buf[..n] == MAGIC_PROBE {
                                let (allow_unicast, allow_bcast) =
                                    rate_limiter.check(src.ip(), Instant::now());
                                if allow_unicast {
                                    trace!(%src, allow_bcast, "Received LAN discovery probe; replying");
                                    let _ = socket.send_to(&cached_payload, src).await;
                                    if allow_bcast {
                                        blaster.blast(&cached_payload, discovery_port, false);
                                    }
                                }
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

/// Actively probes EVERY local IPv4 interface AND listens for unsolicited server beacons
/// for up to `timeout`. Works even when Windows Firewall drops inbound probes!
pub async fn discover_servers(
    discovery_port: u16,
    timeout: Duration,
    name_filter: Option<&str>,
    stop_on_first: bool,
) -> Result<Vec<DiscoveredServer>> {
    // Ephemeral probe socket for unicast replies
    let probe_sock = UdpSocket::bind("0.0.0.0:0").await?;
    let _ = probe_sock.set_broadcast(true);

    // Enumerate NICs once per discovery scan and reuse bound sender sockets across retries.
    let ifaces = active_ipv4_interfaces();
    let mut blaster = BroadcastBlaster::new(&ifaces);

    // Passive listener on discovery_port (joins multicast on all NICs to catch periodic 500ms beacons!)
    let beacon_sock = bind_listener_socket_with_ifaces(discovery_port, &ifaces)
        .ok()
        .map(|(s, _)| s);

    // Send probes out of EVERY interface AND from probe_sock so server unicast replies reach probe_sock
    blaster.blast(MAGIC_PROBE, discovery_port, true);
    let _ = send_probe_from_socket(&probe_sock, discovery_port, &ifaces).await;

    let mut found: HashMap<String, DiscoveredServer> = HashMap::new();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut probe_retry = tokio::time::interval(Duration::from_millis(400));
    probe_retry.tick().await;

    let mut buf_a = [0u8; 2048];
    let mut buf_b = [0u8; 2048];

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline - now;

        let recv_event: Option<std::io::Result<(&[u8], SocketAddr)>> = tokio::select! {
            _ = tokio::time::sleep(remaining) => break,
            _ = probe_retry.tick() => {
                blaster.blast(MAGIC_PROBE, discovery_port, true);
                let _ = send_probe_from_socket(&probe_sock, discovery_port, &ifaces).await;
                continue;
            }
            res = probe_sock.recv_from(&mut buf_a) => {
                Some(res.map(|(n, src)| (&buf_a[..n], src)))
            }
            res = async {
                match &beacon_sock {
                    Some(s) => s.recv_from(&mut buf_b).await,
                    None => std::future::pending().await,
                }
            } => {
                Some(res.map(|(n, src)| (&buf_b[..n], src)))
            }
        };

        let (data, src) = match recv_event {
            Some(Ok(pkt)) => pkt,
            Some(Err(e)) => {
                debug!(error = %e, "Discovery recv_from error; backing off briefly");
                tokio::time::sleep(Duration::from_millis(50).min(remaining)).await;
                continue;
            }
            None => continue,
        };

        if let Ok(beacon) = serde_json::from_slice::<ServerBeacon>(data) {
            let Some(server) = validate_beacon(beacon, src.ip()) else {
                continue;
            };
            if let Some(filter) = name_filter {
                if !server.name.eq_ignore_ascii_case(filter) {
                    continue;
                }
            }
            insert_discovered_server(&mut found, server);
            if stop_on_first && !found.is_empty() {
                // Wait a tiny moment if we only saw loopback so a real LAN packet can win
                if !found.values().all(|s| s.addr.ip().is_loopback()) {
                    break;
                }
            }
        }
    }

    let mut list: Vec<DiscoveredServer> = found.into_values().collect();
    list.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.addr.cmp(&b.addr)));
    Ok(list)
}

async fn send_probe_from_socket(
    sock: &UdpSocket,
    port: u16,
    ifaces: &[LocalIpv4Iface],
) -> Result<()> {
    let mcast = SocketAddrV4::new(DISCOVERY_MULTICAST_ADDR, port);
    let bcast = SocketAddrV4::new(Ipv4Addr::BROADCAST, port);
    let _ = sock.send_to(MAGIC_PROBE, mcast).await;
    let _ = sock.send_to(MAGIC_PROBE, bcast).await;
    for iface in ifaces {
        let _ = sock
            .send_to(MAGIC_PROBE, SocketAddrV4::new(iface.broadcast, port))
            .await;
    }
    let _ = sock
        .send_to(MAGIC_PROBE, SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn sample_fingerprint(seed: u8) -> String {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        bytes[31] = seed.wrapping_add(1);
        bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    #[tokio::test]
    async fn lan_discovery_finds_local_server_beacon() {
        let port = 24891;
        let fp = crate::tls::sha256_fingerprint(b"test-local-beacon-cert");
        let handle = spawn_server_beacon(
            port,
            "win11-workstation".into(),
            24800,
            fp.clone(),
            "windows".into(),
        )
        .unwrap();

        tokio::time::sleep(Duration::from_millis(80)).await;
        let servers = discover_servers(port, Duration::from_secs(2), None, true)
            .await
            .unwrap();
        handle.abort();

        assert!(!servers.is_empty(), "expected to discover local beacon");
        assert_eq!(servers[0].name, "win11-workstation");
        assert_eq!(servers[0].addr.port(), 24800);
        assert_eq!(servers[0].fingerprint, fp);
    }

    #[test]
    fn beacon_validation_rejects_malformed_and_sanitizes_fields() {
        let src = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20));
        let fp = sample_fingerprint(0xab);

        // Reject quic_port == 0
        assert!(validate_beacon(
            ServerBeacon {
                magic: MAGIC_BEACON.into(),
                name: "pc".into(),
                quic_port: 0,
                fingerprint: fp.clone(),
                os: "linux".into(),
                ips: vec![],
            },
            src,
        )
        .is_none());

        // Reject invalid / short / empty fingerprints
        for bad_fp in ["", "aa:bb:cc:dd", "not-a-fingerprint", "zz".repeat(32).as_str()] {
            assert!(
                validate_beacon(
                    ServerBeacon {
                        magic: MAGIC_BEACON.into(),
                        name: "pc".into(),
                        quic_port: 24800,
                        fingerprint: bad_fp.into(),
                        os: "linux".into(),
                        ips: vec![],
                    },
                    src,
                )
                .is_none(),
                "expected bad fingerprint '{bad_fp}' to be rejected"
            );
        }

        // Accept raw 64-hex uppercase fingerprint and canonicalize to colon-hex lowercase
        let raw_hex = fp.replace(':', "").to_ascii_uppercase();
        let valid = validate_beacon(
            ServerBeacon {
                magic: MAGIC_BEACON.into(),
                name: format!("  host\x00\x1b[31m\n{}  ", "x".repeat(80)),
                quic_port: 24800,
                fingerprint: raw_hex,
                os: format!("win\r\n{}", "y".repeat(60)),
                ips: vec![
                    "192.168.1.20".into(),
                    "not-an-ip".into(),
                    "10.0.0.5".into(),
                    "192.168.1.20".into(),
                ],
            },
            src,
        )
        .unwrap();
        assert_eq!(valid.fingerprint, fp);
        assert!(!valid.name.chars().any(|c| c.is_control()));
        assert!(valid.name.chars().count() <= MAX_BEACON_NAME_CHARS);
        assert!(!valid.os.chars().any(|c| c.is_control()));
        assert!(valid.os.chars().count() <= MAX_BEACON_OS_CHARS);
        assert_eq!(valid.all_ips, vec!["192.168.1.20", "10.0.0.5"]);

        // Control-only name is rejected
        assert!(validate_beacon(
            ServerBeacon {
                magic: MAGIC_BEACON.into(),
                name: "\x00\r\n   ".into(),
                quic_port: 24800,
                fingerprint: fp,
                os: "linux".into(),
                ips: vec![],
            },
            src,
        )
        .is_none());
    }

    #[test]
    fn discovered_server_table_capped_at_64() {
        let mut found = HashMap::new();
        for i in 0..80u8 {
            let server = DiscoveredServer {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, i + 1)), 24800),
                name: format!("host-{i}"),
                fingerprint: sample_fingerprint(i),
                os: "linux".into(),
                all_ips: vec![],
            };
            insert_discovered_server(&mut found, server);
        }
        assert_eq!(found.len(), MAX_DISCOVERED_SERVERS);
    }

    #[test]
    fn probe_rate_limiter_enforces_per_source_and_burst_limits() {
        let t0 = Instant::now();
        let mut rl = ProbeRateLimiter::new(t0);
        let src_a = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10));

        // First probe allowed (unicast + broadcast)
        assert_eq!(rl.check(src_a, t0), (true, true));
        // Immediate duplicate from same source rejected
        assert_eq!(
            rl.check(src_a, t0 + Duration::from_millis(100)),
            (false, false)
        );
        // After per-source cooldown (250ms), unicast allowed but broadcast still throttled (<500ms)
        assert_eq!(
            rl.check(src_a, t0 + Duration::from_millis(300)),
            (true, false)
        );
        // Global burst cap blocks excessive distinct sources within 1s
        for i in 2..PROBE_BURST_CAP {
            let src = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10 + i as u8));
            assert!(rl.check(src, t0 + Duration::from_millis(350)).0);
        }
        let overflow_src = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 200));
        assert_eq!(
            rl.check(overflow_src, t0 + Duration::from_millis(360)),
            (false, false)
        );
        // Next burst window allows again (including broadcast)
        assert_eq!(
            rl.check(overflow_src, t0 + PROBE_BURST_WINDOW + Duration::from_millis(10)),
            (true, true)
        );
    }
}
