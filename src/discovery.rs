//! Find peers with mDNS, UDP, or gateway probes. Always authenticate over TCP.
use crate::protocol::{Channel, Digest, random_bytes};
use anyhow::{Context, Result, bail, ensure};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Mutex, OnceLock},
    time::Duration,
};
use tokio::{
    net::{TcpStream, UdpSocket},
    task::{JoinHandle, JoinSet},
    time::{Instant, timeout},
};

pub const PORT: u16 = 48933;
pub const DISCOVERY_PORT: u16 = 48934;
const SERVICE: &str = "_edrelay._tcp.local.";
const QUERY: &[u8; 8] = b"EDFIND02";
const REPLY: &[u8; 8] = b"EDHERE02";
static MANUAL: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static LAST: OnceLock<Mutex<Option<SocketAddr>>> = OnceLock::new();

pub fn local_addresses() -> Vec<String> {
    let mut addresses = Vec::new();
    for interface in if_addrs::get_if_addrs().unwrap_or_default() {
        if interface.is_loopback() {
            continue;
        }
        let ip = interface.ip();
        if matches!(ip, IpAddr::V6(ip) if ip.is_unicast_link_local()) {
            continue;
        }
        addresses.push(ip.to_string());
    }
    addresses.sort();
    addresses.dedup();
    addresses
}

pub fn pair_id(key: &Digest) -> String {
    blake3::hash(key).to_hex()[..16].to_string()
}
pub fn embedded_key() -> Digest {
    blake3::derive_key(
        "elite-journal-relay v1 pre-shared key",
        env!("ED_RELAY_PAIR_KEY").as_bytes(),
    )
}

pub fn manual_address(text: &str) -> Result<Option<String>> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    if let Ok(ip) = text.parse::<IpAddr>() {
        ensure!(
            !ip.is_unspecified() && !ip.is_multicast(),
            "invalid server address"
        );
        return Ok(Some(SocketAddr::new(ip, PORT).to_string()));
    }
    let addr: SocketAddr = text
        .parse()
        .context("enter an IP address, IP:port, [IPv6]:port, or auto")?;
    ensure!(
        addr.port() != 0 && !addr.ip().is_unspecified() && !addr.ip().is_multicast(),
        "invalid server address"
    );
    Ok(Some(addr.to_string()))
}
pub fn set_manual(text: &str) -> Result<()> {
    *MANUAL.get_or_init(|| Mutex::new(None)).lock().unwrap() = manual_address(text)?;
    Ok(())
}
pub fn remember(addr: SocketAddr) {
    *LAST.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(addr);
}

fn tag(key: &Digest, bytes: &[u8]) -> Digest {
    *blake3::keyed_hash(key, bytes).as_bytes()
}
fn verified(key: &Digest, bytes: &[u8], signature: &[u8]) -> bool {
    let signature: [u8; 32] = match signature.try_into() {
        Ok(bytes) => bytes,
        Err(_) => return false,
    };
    blake3::Hash::from(signature) == blake3::Hash::from(tag(key, bytes))
}
fn query(key: &Digest, nonce: &Digest) -> Vec<u8> {
    let mut packet = QUERY.to_vec();
    packet.extend(nonce);
    packet.extend(tag(key, &packet));
    packet
}
fn response(key: &Digest, packet: &[u8], port: u16) -> Option<Vec<u8>> {
    if packet.len() != 72 || &packet[..8] != QUERY || !verified(key, &packet[..40], &packet[40..]) {
        return None;
    }
    let mut reply = REPLY.to_vec();
    reply.extend(&packet[8..40]);
    reply.extend(port.to_be_bytes());
    reply.extend(tag(key, &reply));
    Some(reply)
}
fn response_port(key: &Digest, nonce: &Digest, packet: &[u8]) -> Option<u16> {
    if packet.len() != 74
        || &packet[..8] != REPLY
        || &packet[8..40] != nonce
        || !verified(key, &packet[..42], &packet[42..])
    {
        return None;
    }
    let port = u16::from_be_bytes([packet[40], packet[41]]);
    (port != 0).then_some(port)
}

struct Mdns(ServiceDaemon);
impl Drop for Mdns {
    fn drop(&mut self) {
        let _ = self.0.shutdown();
    }
}
pub struct Advertisement {
    _mdns: Option<Mdns>,
    udp: JoinHandle<()>,
}
impl Drop for Advertisement {
    fn drop(&mut self) {
        self.udp.abort();
    }
}

pub async fn advertise(key: Digest, listen: SocketAddr) -> Result<Advertisement> {
    let ip = if listen.is_ipv4() {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        listen.ip()
    };
    let udp = UdpSocket::bind(SocketAddr::new(ip, DISCOVERY_PORT))
        .await
        .context("bind discovery UDP")?;
    let port = listen.port();
    let task = tokio::spawn(async move {
        let mut buffer = [0u8; 512];
        let mut last = Instant::now() - Duration::from_secs(1);
        loop {
            let Ok((n, peer)) = udp.recv_from(&mut buffer).await else {
                break;
            };
            if last.elapsed() < Duration::from_millis(10) {
                continue;
            }
            if let Some(reply) = response(&key, &buffer[..n], port) {
                last = Instant::now();
                let _ = udp.send_to(&reply, peer).await;
            }
        }
    });
    let mdns = (|| -> Result<Mdns> {
        let daemon = Mdns(ServiceDaemon::new()?);
        let id = pair_id(&key);
        let properties = [("pair", id.as_str()), ("version", "2")];
        let info = ServiceInfo::new(
            SERVICE,
            &format!("Elite Relay {id}"),
            &format!("edrelay-{id}.local."),
            "",
            port,
            &properties[..],
        )?
        .enable_addr_auto();
        daemon.0.register(info)?;
        let monitor = daemon.0.monitor()?;
        std::thread::spawn(move || {
            while let Ok(event) = monitor.recv() {
                if let mdns_sd::DaemonEvent::Error(error) = event {
                    tracing::warn!(%error,"mDNS unavailable; host probes remain enabled");
                }
            }
        });
        Ok(daemon)
    })();
    let mdns = match mdns {
        Ok(m) => Some(m),
        Err(error) => {
            tracing::warn!(%error,"mDNS unavailable; UDP/IP fallback active");
            None
        }
    };
    tracing::info!(pair=%pair_id(&key),tcp=port,udp=DISCOVERY_PORT,"automatic discovery ready; waiting for Windows client");
    Ok(Advertisement {
        _mdns: mdns,
        udp: task,
    })
}

pub fn gateway_candidates() -> Vec<SocketAddr> {
    let mut ips = vec![Ipv4Addr::LOCALHOST];
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            NetworkManagement::IpHelper::{FreeMibTable, GetIpForwardTable2},
            Networking::WinSock::AF_INET,
        };
        let mut table = std::ptr::null_mut();
        // IP Helper owns this aligned table of NumEntries rows; free it below.
        // PowerShell/WMI route queries can stall on Windows ARM64.
        unsafe {
            if GetIpForwardTable2(AF_INET, &mut table) == 0 && !table.is_null() {
                let rows = std::slice::from_raw_parts(
                    std::ptr::addr_of!((*table).Table).cast(),
                    (*table).NumEntries as usize,
                );
                for row in rows {
                    let row: &windows_sys::Win32::NetworkManagement::IpHelper::MIB_IPFORWARD_ROW2 =
                        row;
                    if row.DestinationPrefix.PrefixLength == 0 {
                        let ip =
                            Ipv4Addr::from(row.NextHop.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes());
                        if !ip.is_unspecified() && !ip.is_multicast() {
                            ips.push(ip);
                        }
                    }
                }
                FreeMibTable(table.cast());
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = &mut ips;
    }
    ips.sort();
    ips.dedup();
    let mut addresses = Vec::new();
    for ip in ips {
        addresses.push(SocketAddr::new(IpAddr::V4(ip), PORT));
    }
    addresses
}

pub async fn connect_auto(key: Digest) -> Result<Channel> {
    let candidates = tokio::task::spawn_blocking(gateway_candidates).await?;
    let udp = UdpSocket::bind("0.0.0.0:0").await?;
    udp.set_broadcast(true)?;
    let nonce = random_bytes()?;
    let request = query(&key, &nonce);
    // Try the gateway first to avoid a Windows multicast firewall prompt.
    let mut _daemon = None;
    let mut browse: Option<mdns_sd::Receiver<ServiceEvent>> = None;
    let mdns_at = Instant::now() + Duration::from_secs(2);
    let mut mdns_started = false;
    let mut discovered = HashSet::new();
    let mut attempts = JoinSet::new();
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut buffer = [0u8; 512];
    let id = pair_id(&key);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(mdns_at), if !mdns_started => {
                mdns_started = true;
                match ServiceDaemon::new() {
                    Ok(daemon) => {
                        browse = match daemon.browse(SERVICE) {
                            Ok(rx) => Some(rx),
                            Err(_) => None,
                        };
                        _daemon = Some(Mdns(daemon));
                    }
                    Err(error) => tracing::warn!(%error,"mDNS unavailable; gateway and IP fallback remain active"),
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                bail!("no paired server found; open the Mac server, or type its IP here and press Enter");
            }
            _ = tick.tick() => {
                let mut addresses = candidates.clone();
                let last = *LAST.get_or_init(|| Mutex::new(None)).lock().unwrap();
                if let Some(last) = last {
                    addresses.insert(0, last);
                }
                for address in &discovered {
                    addresses.push(*address);
                }
                addresses.sort();
                addresses.dedup();

                let manual = MANUAL.get_or_init(|| Mutex::new(None)).lock().unwrap().clone();
                if let Some(manual) = manual {
                    match manual.parse() {
                        Ok(addr) => {
                            attempts.abort_all();
                            addresses.clear();
                            addresses.push(addr);
                        }
                        Err(_) => (),
                    }
                }
                for address in addresses.into_iter().take(32) {
                    attempts.spawn(async move {
                        let res = timeout(Duration::from_millis(1800), async {
                            let stream = TcpStream::connect(address).await?;
                            let channel = crate::transport::connect(stream, key, None).await?;
                            Ok::<_, anyhow::Error>((channel, address))
                        }).await;
                        res.context("candidate timed out")?
                    });
                    let udp_address = SocketAddr::new(address.ip(), DISCOVERY_PORT);
                    let _ = udp.send_to(&request, udp_address).await;
                }
                if MANUAL.get_or_init(|| Mutex::new(None)).lock().unwrap().is_none() {
                    let _ = udp.send_to(&request, (Ipv4Addr::BROADCAST, DISCOVERY_PORT)).await;
                }
            }
            packet = udp.recv_from(&mut buffer) => {
                let manual = MANUAL.get_or_init(|| Mutex::new(None)).lock().unwrap().is_some();
                if manual {
                    continue;
                }
                let (n, peer) = match packet {
                    Ok(packet) => packet,
                    Err(_) => continue,
                };
                let port = match response_port(&key, &nonce, &buffer[..n]) {
                    Some(port) => port,
                    None => continue,
                };
                let address = SocketAddr::new(peer.ip(), port);
                if discovered.len() < 32 && discovered.insert(address) {
                    attempts.spawn(async move {
                        let res = timeout(Duration::from_millis(1800), async {
                            let stream = TcpStream::connect(address).await?;
                            let channel = crate::transport::connect(stream, key, None).await?;
                            Ok::<_, anyhow::Error>((channel, address))
                        }).await;
                        res.context("candidate timed out")?
                    });
                }
            }
            event = async {
                match &browse {
                    Some(rx) => rx.recv_async().await.ok(),
                    None => std::future::pending().await,
                }
            } => {
                let manual = MANUAL.get_or_init(|| Mutex::new(None)).lock().unwrap().is_some();
                if manual {
                    continue;
                }
                let info = match event {
                    Some(ServiceEvent::ServiceResolved(info)) => info,
                    _ => continue,
                };
                if info.get_property_val_str("pair") != Some(id.as_str()) {
                    continue;
                }
                for address in info.get_addresses().iter().take(16) {
                    // Link-local IPv6 needs an interface scope we don't have here.
                    let ip = match address.to_ip_addr() {
                        IpAddr::V4(ip) => ip,
                        _ => continue,
                    };
                    let address = SocketAddr::new(IpAddr::V4(ip), info.get_port());
                    if discovered.len() < 32 && discovered.insert(address) {
                        attempts.spawn(async move {
                        let res = timeout(Duration::from_millis(1800), async {
                            let stream = TcpStream::connect(address).await?;
                            let channel = crate::transport::connect(stream, key, None).await?;
                            Ok::<_, anyhow::Error>((channel, address))
                        }).await;
                        res.context("candidate timed out")?
                    });
                    }
                }
            }
            result = attempts.join_next(), if !attempts.is_empty() => {
                let result = match result {
                    Some(Ok(result)) => result,
                    _ => continue,
                };
                if let Ok((channel, address)) = result {
                    remember(address);
                    tracing::info!(server=%address,"paired Mac server discovered and authenticated");
                    return Ok(channel);
                }
            }
        }
    }
}
