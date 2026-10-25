use std::{
    collections::HashSet,
    io::ErrorKind,
    net::{IpAddr, SocketAddr},
};
#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg(windows)]
use tokio::net::TcpStream;

use anyhow::{anyhow, Result};
use tokio::{
    io::ReadHalf,
    sync::mpsc::{Receiver, Sender},
    task::JoinHandle,
};

use crate::{
    client::{ClientEvent, ClientHandle, Position},
    frontend::{self, FrontendEvent, FrontendListener, FrontendNotify},
};

use super::{
    capture_task::CaptureEvent, emulation_task::EmulationEvent, resolver_task::DnsRequest, Server,
};

pub(crate) fn new(
    mut frontend: FrontendListener,
    mut notify_rx: Receiver<FrontendNotify>,
    server: Server,
    capture_notify: Sender<CaptureEvent>,
    emulate_notify: Sender<EmulationEvent>,
    resolve_ch: Sender<DnsRequest>,
    port_tx: Sender<u16>,
) -> (JoinHandle<Result<()>>, Sender<FrontendEvent>) {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(32);
    let event_tx_clone = event_tx.clone();
    let frontend_task = tokio::task::spawn_local(async move {
        loop {
            tokio::select! {
                stream = frontend.accept() => {
                    let stream = match stream {
                        Ok(s) => s,
                        Err(e) => {
                            log::warn!("error accepting frontend connection: {e}");
                            continue;
                        }
                    };
                    handle_frontend_stream(&event_tx_clone, stream).await;
                }
                event = event_rx.recv() => {
                    let frontend_event = event.ok_or(anyhow!("frontend channel closed"))?;
                    if handle_frontend_event(&server, &capture_notify, &emulate_notify, &resolve_ch, &mut frontend, &port_tx, frontend_event).await {
                        break;
                    }
                }
                notify = notify_rx.recv() => {
                    let notify = notify.ok_or(anyhow!("frontend notify closed"))?;
                    let _ = frontend.notify_all(notify).await;
                }
            }
        }
        anyhow::Ok(())
    });
    (frontend_task, event_tx)
}

async fn handle_frontend_stream(
    frontend_tx: &Sender<FrontendEvent>,
    #[cfg(unix)] mut stream: ReadHalf<UnixStream>,
    #[cfg(windows)] mut stream: ReadHalf<TcpStream>,
) {
    use std::io;

    let tx = frontend_tx.clone();
    tokio::task::spawn_local(async move {
        let _ = tx.send(FrontendEvent::Enumerate()).await;
        loop {
            let event = frontend::read_event(&mut stream).await;
            match event {
                Ok(event) => {
                    let _ = tx.send(event).await;
                }
                Err(e) => {
                    if let Some(e) = e.downcast_ref::<io::Error>() {
                        if e.kind() == ErrorKind::UnexpectedEof {
                            return;
                        }
                    }
                    log::error!("error reading frontend event: {e}");
                    return;
                }
            }
        }
    });
}

async fn handle_frontend_event(
    server: &Server,
    capture_tx: &Sender<CaptureEvent>,
    emulate_tx: &Sender<EmulationEvent>,
    resolve_tx: &Sender<DnsRequest>,
    frontend: &mut FrontendListener,
    port_tx: &Sender<u16>,
    event: FrontendEvent,
) -> bool {
    log::debug!("frontend: {event:?}");
    let changed = matches!(
        &event,
        FrontendEvent::AddClient(..)
            | FrontendEvent::DelClient(..)
            | FrontendEvent::UpdateClient(..)
            | FrontendEvent::ActivateClient(..)
            | FrontendEvent::RenamePeer(..)
    );
    let response = match event {
        FrontendEvent::SetIdentity(name, role) => {
            let name = name.trim();
            if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
                Some(FrontendNotify::NotifyError(
                    "Choose a display name of 1–80 characters".into(),
                ))
            } else if role == crate::config::Role::Host && !server.capture_available.get() {
                Some(FrontendNotify::NotifyError(
                    "Windows input capture could not start. Check service logs.".into(),
                ))
            } else {
                match crate::config::save_identity(&server.config_path, name, role) {
                    Ok(()) => {
                        *server.device_name.borrow_mut() = name.into();
                        server.role.set(role);
                        let handles: Vec<_> = server
                            .client_manager
                            .borrow()
                            .get_client_states()
                            .map(|s| s.client.handle)
                            .collect();
                        for handle in handles {
                            let _ = emulate_tx.send(EmulationEvent::ReleaseKeys(handle)).await;
                        }
                        let _ = capture_tx
                            .send(CaptureEvent::SetEnabled(role == crate::config::Role::Host))
                            .await;
                        log::info!("Device role changed to {role:?}; display name updated");
                        Some(FrontendNotify::Status(server.status()))
                    }
                    Err(e) => Some(FrontendNotify::NotifyError(format!(
                        "Could not save this PC's settings: {e}"
                    ))),
                }
            }
        }
        FrontendEvent::RenamePeer(handle, name) => {
            let name = name.trim();
            if name.chars().count() > 80 || name.chars().any(char::is_control) {
                return false;
            }
            let updated = server.client_manager.borrow_mut().get_mut(handle).map(|s| {
                s.client.display_name = if name.is_empty() {
                    None
                } else {
                    Some(name.into())
                };
                s.client.clone()
            });
            updated.map(FrontendNotify::NotifyClientUpdate)
        }
        FrontendEvent::ReleaseInput() => {
            let handles: Vec<_> = server
                .client_manager
                .borrow()
                .get_client_states()
                .map(|s| s.client.handle)
                .collect();
            for handle in handles {
                let _ = emulate_tx.send(EmulationEvent::ReleaseKeys(handle)).await;
            }
            let _ = capture_tx.send(CaptureEvent::Release).await;
            Some(FrontendNotify::Status(server.status()))
        }
        FrontendEvent::Status() => Some(FrontendNotify::Status(server.status())),
        FrontendEvent::ShowWindow() => Some(FrontendNotify::ShowWindow()),
        FrontendEvent::AddClient(hostname, port, pos) => {
            let handle = add_client(server, resolve_tx, hostname, HashSet::new(), port, pos).await;
            if let Some(state) = server.client_manager.borrow_mut().get_mut(handle) {
                if let Some(device) = server.discovered.borrow().iter().find(|d| {
                    state.client.hostname.as_deref() == Some(d.address.as_str())
                        || state.client.hostname.as_deref() == Some(d.computer_name.as_str())
                }) {
                    state.client.display_name = Some(device.name.clone());
                }
            }

            let client = server
                .client_manager
                .borrow()
                .get(handle)
                .unwrap()
                .client
                .clone();
            Some(FrontendNotify::NotifyClientCreate(client))
        }
        FrontendEvent::ActivateClient(handle, active) => {
            activate_client(server, capture_tx, emulate_tx, handle, active).await;
            Some(FrontendNotify::NotifyClientActivate(handle, active))
        }
        FrontendEvent::ChangePort(port) => {
            if port == 0 {
                let _ = frontend
                    .notify_all(FrontendNotify::NotifyError(
                        "Port must be between 1 and 65535".into(),
                    ))
                    .await;
                return false;
            }
            let _ = port_tx.send(port).await;
            None
        }
        FrontendEvent::DelClient(handle) => {
            remove_client(server, capture_tx, emulate_tx, frontend, handle).await;
            Some(FrontendNotify::NotifyClientDelete(handle))
        }
        FrontendEvent::Enumerate() => {
            let clients = server
                .client_manager
                .borrow()
                .get_client_states()
                .map(|s| (s.client.clone(), s.active))
                .collect();
            Some(FrontendNotify::Enumerate(clients))
        }
        FrontendEvent::Shutdown() => {
            log::info!("terminating gracefully...");
            return true;
        }
        FrontendEvent::UpdateClient(handle, hostname, port, pos) => {
            if server.client_manager.borrow().get(handle).is_none() {
                return false;
            }
            if port == 0 {
                let _ = frontend
                    .notify_all(FrontendNotify::NotifyError(
                        "Peer port must be between 1 and 65535".into(),
                    ))
                    .await;
                return false;
            }
            update_client(
                server,
                capture_tx,
                emulate_tx,
                resolve_tx,
                (handle, hostname, port, pos),
            )
            .await;

            let client = server
                .client_manager
                .borrow()
                .get(handle)
                .unwrap()
                .client
                .clone();
            Some(FrontendNotify::NotifyClientUpdate(client))
        }
    };
    if changed {
        if let Err(e) = server.save_config() {
            let _ = frontend
                .notify_all(FrontendNotify::NotifyError(format!(
                    "Could not save settings: {e}"
                )))
                .await;
        }
    }
    let Some(response) = response else {
        return false;
    };
    if let Err(e) = frontend.notify_all(response).await {
        log::error!("error notifying frontend: {e}");
    }
    false
}

pub async fn add_client(
    server: &Server,
    resolver_tx: &Sender<DnsRequest>,
    hostname: Option<String>,
    addr: HashSet<IpAddr>,
    port: u16,
    pos: Position,
) -> ClientHandle {
    log::info!(
        "adding client [{}]{} @ {:?}",
        pos,
        hostname.as_deref().unwrap_or(""),
        &addr
    );
    let handle =
        server
            .client_manager
            .borrow_mut()
            .add_client(hostname.clone(), addr, port, pos, false);

    log::debug!("add_client {handle}");

    if let Some(hostname) = hostname {
        let _ = resolver_tx.send(DnsRequest { hostname, handle }).await;
    }

    handle
}

pub async fn activate_client(
    server: &Server,
    capture_notify_tx: &Sender<CaptureEvent>,
    emulate_notify_tx: &Sender<EmulationEvent>,
    client: ClientHandle,
    active: bool,
) {
    let (client, pos) = match server.client_manager.borrow_mut().get_mut(client) {
        Some(state) => {
            state.active = active;
            (state.client.handle, state.client.pos)
        }
        None => return,
    };
    if active {
        let _ = capture_notify_tx
            .send(CaptureEvent::ClientEvent(ClientEvent::Create(client, pos)))
            .await;
        let _ = emulate_notify_tx
            .send(EmulationEvent::ClientEvent(ClientEvent::Create(
                client, pos,
            )))
            .await;
    } else {
        let address = server
            .client_manager
            .borrow()
            .get(client)
            .and_then(|s| s.active_addr);
        let _ = capture_notify_tx
            .send(CaptureEvent::ReleasePeer(client, address))
            .await;
        let _ = emulate_notify_tx
            .send(EmulationEvent::ReleaseKeys(client))
            .await;
        let _ = capture_notify_tx
            .send(CaptureEvent::ClientEvent(ClientEvent::Destroy(client)))
            .await;
        let _ = emulate_notify_tx
            .send(EmulationEvent::ClientEvent(ClientEvent::Destroy(client)))
            .await;
    }
}

pub async fn remove_client(
    server: &Server,
    capture_notify_tx: &Sender<CaptureEvent>,
    emulate_notify_tx: &Sender<EmulationEvent>,
    frontend: &mut FrontendListener,
    client: ClientHandle,
) -> Option<ClientHandle> {
    let Some((client, active, address, keys)) = server
        .client_manager
        .borrow_mut()
        .remove_client(client)
        .map(|s| {
            (
                s.client.handle,
                s.active,
                s.active_addr,
                s.pressed_keys.into_iter().collect::<Vec<_>>(),
            )
        })
    else {
        return None;
    };

    if active {
        let _ = capture_notify_tx
            .send(CaptureEvent::ReleasePeer(client, address))
            .await;
        let _ = emulate_notify_tx
            .send(EmulationEvent::ReleaseSnapshot(client, keys))
            .await;
        let _ = capture_notify_tx
            .send(CaptureEvent::ClientEvent(ClientEvent::Destroy(client)))
            .await;
        let _ = emulate_notify_tx
            .send(EmulationEvent::ClientEvent(ClientEvent::Destroy(client)))
            .await;
    }

    let notify = FrontendNotify::NotifyClientDelete(client);
    log::debug!("{notify:?}");
    if let Err(e) = frontend.notify_all(notify).await {
        log::error!("error notifying frontend: {e}");
    }
    Some(client)
}

async fn update_client(
    server: &Server,
    capture_notify_tx: &Sender<CaptureEvent>,
    emulate_notify_tx: &Sender<EmulationEvent>,
    resolve_tx: &Sender<DnsRequest>,
    client_update: (ClientHandle, Option<String>, u16, Position),
) {
    let (handle, hostname, port, pos) = client_update;
    if server.active_client.get() == Some(handle) {
        let address = server
            .client_manager
            .borrow()
            .get(handle)
            .and_then(|s| s.active_addr);
        let _ = capture_notify_tx
            .send(CaptureEvent::ReleasePeer(handle, address))
            .await;
    }
    let mut changed = false;
    let (hostname, handle, active) = {
        // retrieve state
        let mut client_manager = server.client_manager.borrow_mut();
        let Some(state) = client_manager.get_mut(handle) else {
            return;
        };

        // update pos
        if state.client.pos != pos {
            state.client.pos = pos;
            changed = true;
        }

        // update port
        if state.client.port != port {
            state.client.port = port;
            state.active_addr = state.active_addr.map(|a| SocketAddr::new(a.ip(), port));
            changed = true;
        }

        // update hostname
        if state.client.hostname != hostname {
            state.client.ips = HashSet::new();
            state.active_addr = None;
            state.client.hostname = hostname;
            changed = true;
        }

        log::debug!("client updated: {:?}", state);
        (
            state.client.hostname.clone(),
            state.client.handle,
            state.active,
        )
    };

    // resolve dns if something changed
    if changed {
        // resolve dns
        if let Some(hostname) = hostname {
            let _ = resolve_tx.send(DnsRequest { hostname, handle }).await;
        }
    }

    // update state in event input emulator & input capture
    if changed && active {
        // update state
        let _ = capture_notify_tx
            .send(CaptureEvent::ClientEvent(ClientEvent::Destroy(handle)))
            .await;
        let _ = emulate_notify_tx
            .send(EmulationEvent::ClientEvent(ClientEvent::Destroy(handle)))
            .await;
        let _ = capture_notify_tx
            .send(CaptureEvent::ClientEvent(ClientEvent::Create(handle, pos)))
            .await;
        let _ = emulate_notify_tx
            .send(EmulationEvent::ClientEvent(ClientEvent::Create(
                handle, pos,
            )))
            .await;
    }
}
