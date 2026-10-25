//! Bounded, app-specific LAN announcements. Discovery never enables a peer.
use super::{capture_task::CaptureEvent, Server};
use crate::{client::ClientHandle, config::Role, event::Event, frontend::DiscoveredDevice};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};
use tokio::{net::UdpSocket, sync::mpsc::Sender, task::JoinHandle};

const PORT: u16 = 4243;
const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 42, 42);
const MAGIC: &str = "lanmouse-desktop-v3";

pub fn local_addresses() -> Vec<Ipv4Addr> {
    #[cfg(windows)]
    {
        ipconfig::get_adapters()
            .unwrap_or_default()
            .iter()
            .flat_map(|a| a.ip_addresses().iter())
            .filter_map(|ip| match ip {
                IpAddr::V4(ip) if !ip.is_loopback() && (ip.is_private() || ip.is_link_local()) => {
                    Some(*ip)
                }
                _ => None,
            })
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec![]
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Announcement {
    magic: String,
    name: String,
    computer_name: String,
    port: u16,
    role: Role,
}

fn validate(data: &[u8]) -> Option<Announcement> {
    if data.len() > 1024 {
        return None;
    }
    let value: Announcement = serde_json::from_slice(data).ok()?;
    if value.magic != MAGIC
        || value.port == 0
        || value.name.is_empty()
        || value.name.chars().count() > 80
        || value.computer_name.chars().count() > 80
        || value
            .name
            .chars()
            .chain(value.computer_name.chars())
            .any(char::is_control)
    {
        return None;
    }
    Some(value)
}

pub fn start(server: Server) -> JoinHandle<()> {
    tokio::task::spawn_local(async move {
        let result = run(server.clone()).await;
        if let Err(e) = result {
            *server.discovery_status.borrow_mut() =
                format!("Discovery unavailable: {e}. Add a hostname or IP manually.");
            log::warn!("LAN discovery unavailable: {e}");
        }
    })
}

async fn run(server: Server) -> anyhow::Result<()> {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    socket.set_reuse_address(true)?;
    socket.set_broadcast(true)?;
    socket.set_multicast_ttl_v4(1)?;
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, PORT)).into())?;
    let mut interfaces = local_addresses();
    for ip in &interfaces {
        let _ = socket.join_multicast_v4(&GROUP, ip);
    }
    socket.set_nonblocking(true)?;
    let send: std::net::UdpSocket = socket.try_clone()?.into();
    let socket = UdpSocket::from_std(socket.into())?;
    let mut seen: HashMap<IpAddr, (Announcement, Instant)> = HashMap::new();
    let mut interval = tokio::time::interval(Duration::from_secs(3));
    let mut buffer = [0; 2048];
    *server.discovery_status.borrow_mut() =
        "LAN discovery active · UDP 4243 · routers/VLANs may block discovery".into();
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let current = local_addresses();
                for ip in &current { if !interfaces.contains(ip) { let _ = socket.join_multicast_v4(GROUP, *ip); } }
                interfaces = current;
                let packet = serde_json::to_vec(&Announcement { magic: MAGIC.into(), name: server.device_name.borrow().clone(), computer_name: crate::config::computer_name(), port: server.port.get(), role: server.role.get() })?;
                for ip in &interfaces {
                    let _ = socket2::SockRef::from(&send).set_multicast_if_v4(ip);
                    let _ = send.send_to(&packet, (GROUP, PORT));
                }
                // Explicit configured peers can be contacted through routed networks.
                // No subnet scans, Internet probing or automatic trust is performed.
                let targets: Vec<_> = server.client_manager.borrow().get_client_states().flat_map(|s| s.client.ips.iter().copied()).filter(|ip| !ip.is_loopback()).collect();
                for ip in targets { let _ = send.send_to(&packet, SocketAddr::new(ip, PORT)); }
                seen.retain(|_, (_, time)| time.elapsed() < Duration::from_secs(15));
                *server.discovered.borrow_mut() = seen.iter().map(|(ip, (value, _))| DiscoveredDevice { name: value.name.clone(), computer_name: value.computer_name.clone(), address: ip.to_string(), port: value.port, role: value.role }).collect();
            },
            received = socket.recv_from(&mut buffer) => {
                let (length, source) = received?;
                if interfaces.iter().any(|ip| IpAddr::V4(*ip) == source.ip()) { continue; }
                if let Some(packet) = validate(&buffer[..length]) {
                    if seen.len() < 128 || seen.contains_key(&source.ip()) { seen.insert(source.ip(), (packet, Instant::now())); }
                }
            }
        }
    }
}

pub fn heartbeat(
    server: Server,
    sender: Sender<(Event, SocketAddr)>,
    capture: Sender<CaptureEvent>,
    resolver: Sender<super::resolver_task::DnsRequest>,
) -> JoinHandle<()> {
    tokio::task::spawn_local(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut rounds = 0u32;
        let mut was_ready: BTreeMap<ClientHandle, bool> = BTreeMap::new();
        loop {
            tick.tick().await;
            rounds = rounds.wrapping_add(1);
            if rounds % 10 == 0 {
                let unresolved: Vec<_> = server
                    .client_manager
                    .borrow()
                    .get_client_states()
                    .filter(|s| {
                        s.active
                            && !s
                                .last_seen
                                .map(|t| t.elapsed().as_secs() < 4)
                                .unwrap_or(false)
                    })
                    .filter_map(|s| {
                        s.client
                            .hostname
                            .clone()
                            .map(|hostname| super::resolver_task::DnsRequest {
                                hostname,
                                handle: s.client.handle,
                            })
                    })
                    .collect();
                for request in unresolved {
                    let _ = resolver.try_send(request);
                }
            }
            let targets: Vec<_> = server
                .client_manager
                .borrow()
                .get_client_states()
                .map(|s| {
                    (
                        s.client.label(),
                        s.client.handle,
                        s.client.pos,
                        s.active,
                        s.last_seen
                            .map(|t| t.elapsed().as_secs() < 4)
                            .unwrap_or(false),
                        s.client
                            .ips
                            .iter()
                            .map(|ip| SocketAddr::new(*ip, s.client.port))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect();
            for (label, handle, position, enabled, ready, addresses) in targets {
                let conflict = server.role.get() == Role::Host
                    && server.discovered.borrow().iter().any(|d| {
                        d.role == Role::Host
                            && addresses.iter().any(|a| a.ip().to_string() == d.address)
                    });
                let can_enter = enabled && ready && !conflict;
                let previous = was_ready.insert(handle, can_enter);
                if previous.is_some_and(|was| was != can_enter) {
                    let addresses = addresses
                        .iter()
                        .map(|a| a.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    if can_enter {
                        log::info!(
                            "peer '{label}' is responding ({addresses}); it can be entered by pushing the pointer against the {position} edge of this screen"
                        );
                    } else if !enabled {
                        log::info!(
                            "peer '{label}' is disabled in Connections; no session can start until it is enabled on both PCs"
                        );
                    } else if conflict {
                        log::info!(
                            "peer '{label}' ({addresses}) answers, but both PCs are set to Host mode (role conflict); entry stays blocked until one side switches back to Client"
                        );
                    } else {
                        log::info!(
                            "peer '{label}' ({addresses}) stopped responding; entry waits until heartbeats are answered again (check that the app runs on the other PC)"
                        );
                    }
                }
                let _ = capture
                    .send(CaptureEvent::SetReady(
                        handle,
                        enabled && ready && !conflict,
                    ))
                    .await;
                if !enabled {
                    continue;
                }
                if let Some(state) = server.client_manager.borrow_mut().get_mut(handle) {
                    state.last_ping = Some(Instant::now());
                }
                for address in addresses {
                    if sender.send((Event::Ping(), address)).await.is_err() {
                        return;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_is_bounded_and_rejects_unrelated_or_control_text() {
        assert!(validate(&vec![b'x'; 1025]).is_none());
        assert!(validate(b"{}").is_none());
        let mut packet = Announcement {
            magic: MAGIC.into(),
            name: "Office PC".into(),
            computer_name: "PC-01".into(),
            port: 4242,
            role: Role::Client,
        };
        assert!(validate(&serde_json::to_vec(&packet).unwrap()).is_some());
        packet.name = "bad\nname".into();
        assert!(validate(&serde_json::to_vec(&packet).unwrap()).is_none());
    }
}
