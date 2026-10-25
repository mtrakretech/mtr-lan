use log;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use tokio::signal;

use crate::{capture, emulate};
use crate::{
    client::{ClientHandle, ClientManager},
    config::Config,
    dns,
    frontend::{FrontendEvent, FrontendListener},
    server::capture_task::CaptureEvent,
};

use self::{emulation_task::EmulationEvent, resolver_task::DnsRequest};

mod capture_task;
mod discovery;
mod emulation_task;
mod escape;
mod frontend_task;
mod network_task;
mod ping_task;
mod resolver_task;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    /// Currently sending events to another device
    Sending,
    /// Currently receiving events from other devices
    Receiving,
    /// Entered the deadzone of another device but waiting
    /// for acknowledgement (Leave event) from the device
    AwaitingLeave,
}

#[derive(Clone)]
pub struct Server {
    role: Rc<Cell<crate::config::Role>>,
    device_name: Rc<RefCell<String>>,
    capture_available: Rc<Cell<bool>>,
    discovered: Rc<RefCell<Vec<crate::frontend::DiscoveredDevice>>>,
    discovery_status: Rc<RefCell<String>>,
    config_path: std::path::PathBuf,
    started: std::time::Instant,
    packets_received: Rc<Cell<u64>>,
    packets_sent: Rc<Cell<u64>>,
    last_received: Rc<Cell<Option<std::time::Instant>>>,
    /// Real pointer/key activity from peers (not control packets), for the escape gesture.
    last_remote_input: Rc<Cell<Option<std::time::Instant>>>,
    active_client: Rc<Cell<Option<ClientHandle>>>,
    client_manager: Rc<RefCell<ClientManager>>,
    port: Rc<Cell<u16>>,
    state: Rc<Cell<State>>,
    release_bind: Vec<crate::scancode::Linux>,
}

impl Server {
    pub fn new(config: &Config) -> Self {
        let active_client = Rc::new(Cell::new(None));
        let client_manager = Rc::new(RefCell::new(ClientManager::new()));
        let state = Rc::new(Cell::new(State::Receiving));
        let port = Rc::new(Cell::new(config.port));
        for config_client in config.get_clients() {
            let handle = client_manager.borrow_mut().add_client(
                config_client.hostname,
                config_client.ips,
                config_client.port,
                config_client.pos,
                config_client.active,
            );
            if let Some(state) = client_manager.borrow_mut().get_mut(handle) {
                state.client.display_name = config_client.display_name;
            }
        }
        let release_bind = config.release_bind.clone();
        Self {
            role: Rc::new(Cell::new(config.role)),
            device_name: Rc::new(RefCell::new(config.device_name.clone())),
            capture_available: Rc::new(Cell::new(false)),
            discovered: Rc::new(RefCell::new(vec![])),
            discovery_status: Rc::new(RefCell::new("Starting LAN discovery…".into())),
            config_path: config.config_path.clone(),
            started: std::time::Instant::now(),
            packets_received: Rc::new(Cell::new(0)),
            packets_sent: Rc::new(Cell::new(0)),
            last_received: Rc::new(Cell::new(None)),
            last_remote_input: Rc::new(Cell::new(None)),
            active_client,
            client_manager,
            port,
            state,
            release_bind,
        }
    }

    fn status(&self) -> crate::frontend::ServiceStatus {
        let manager = self.client_manager.borrow();
        crate::frontend::ServiceStatus {
            computer_name: crate::config::computer_name(),
            device_name: self.device_name.borrow().clone(),
            role: self.role.get(),
            capture_available: self.capture_available.get(),
            sharing: self.state.get() != State::Receiving,
            local_addresses: discovery::local_addresses()
                .iter()
                .map(ToString::to_string)
                .collect(),
            discovered: self.discovered.borrow().clone(),
            discovery_status: self.discovery_status.borrow().clone(),
            peers: manager
                .get_client_states()
                .map(|s| crate::frontend::PeerStatus {
                    handle: s.client.handle,
                    display_name: s.client.display_name.clone(),
                    hostname: s.client.hostname.clone(),
                    enabled: s.active,
                    responding: s
                        .last_seen
                        .map(|t| t.elapsed().as_secs() < 4)
                        .unwrap_or(false),
                    last_seen_secs: s.last_seen.map(|t| t.elapsed().as_secs()),
                    latency_ms: s.latency_ms,
                    addresses: s.client.ips.iter().map(ToString::to_string).collect(),
                })
                .collect(),
            port: self.port.get(),
            configured: manager.get_client_states().count(),
            enabled: manager.get_client_states().filter(|s| s.active).count(),
            packets_received: self.packets_received.get(),
            packets_sent: self.packets_sent.get(),
            last_received_secs: self.last_received.get().map(|t| t.elapsed().as_secs()),
            uptime_secs: self.started.elapsed().as_secs(),
        }
    }

    fn save_config(&self) -> anyhow::Result<()> {
        let clients = self
            .client_manager
            .borrow()
            .get_client_states()
            .map(|s| crate::config::StoredClient {
                display_name: s.client.display_name.clone(),
                hostname: s.client.hostname.clone(),
                ips: s.client.fix_ips.clone(),
                port: s.client.port,
                position: s.client.pos,
                activate_on_startup: s.active,
            })
            .collect();
        crate::config::save_peer_state(&self.config_path, self.port.get(), clients)?;
        crate::config::save_identity(
            &self.config_path,
            &self.device_name.borrow(),
            self.role.get(),
        )
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        // create frontend communication adapter
        let frontend = match FrontendListener::new().await {
            Some(f) => f?,
            None => {
                // none means some other instance is already running
                log::info!("service already running, exiting");
                return anyhow::Ok(());
            }
        };
        let (emulate, capture) = tokio::join!(emulate::create(), capture::create());
        self.capture_available.set(capture.is_available());

        let (timer_tx, timer_rx) = tokio::sync::mpsc::channel(1);
        let (frontend_notify_tx, frontend_notify_rx) = tokio::sync::mpsc::channel(1);

        // udp task
        let (mut udp_task, sender_tx, receiver_rx, port_tx) =
            network_task::new(self.clone(), frontend_notify_tx).await?;

        // input capture
        let (mut capture_task, capture_channel) = capture_task::new(
            capture,
            self.clone(),
            sender_tx.clone(),
            timer_tx.clone(),
            self.release_bind.clone(),
        );

        // input emulation
        let (mut emulation_task, emulate_channel) = emulation_task::new(
            emulate,
            self.clone(),
            receiver_rx,
            sender_tx.clone(),
            capture_channel.clone(),
            timer_tx,
        );

        // create dns resolver
        let resolver = dns::DnsResolver::new().await?;
        let (mut resolver_task, resolve_tx) = resolver_task::new(resolver, self.clone());

        // frontend listener
        let (mut frontend_task, frontend_tx) = frontend_task::new(
            frontend,
            frontend_notify_rx,
            self.clone(),
            capture_channel.clone(),
            emulate_channel.clone(),
            resolve_tx.clone(),
            port_tx,
        );

        // task that pings clients to see if they are responding
        let mut ping_task = ping_task::new(
            self.clone(),
            sender_tx.clone(),
            emulate_channel.clone(),
            capture_channel.clone(),
            timer_rx,
        );
        let discovery_task = discovery::start(self.clone());
        let heartbeat = discovery::heartbeat(
            self.clone(),
            sender_tx.clone(),
            capture_channel.clone(),
            resolve_tx.clone(),
        );
        // Client-side "return" gesture: escape only while remote input is active.
        let mut escape_task = escape::new(self.clone(), sender_tx.clone(), emulate_channel.clone());

        let active = self
            .client_manager
            .borrow()
            .get_client_states()
            .filter_map(|s| {
                if s.active {
                    Some((s.client.handle, s.client.hostname.clone()))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        for (handle, hostname) in active {
            frontend_tx
                .send(FrontendEvent::ActivateClient(handle, true))
                .await?;
            if let Some(hostname) = hostname {
                let _ = resolve_tx.send(DnsRequest { hostname, handle }).await;
            }
        }
        log::info!("running service");

        tokio::select! {
            _ = signal::ctrl_c() => {
                log::info!("terminating service");
            }
            e = &mut capture_task => {
                if let Ok(Err(e)) = e {
                    log::error!("error in input capture task: {e}");
                }
            }
            e = &mut emulation_task => {
                if let Ok(Err(e)) = e {
                    log::error!("error in input emulation task: {e}");
                }
            }
            e = &mut frontend_task => {
                if let Ok(Err(e)) = e {
                    log::error!("error in frontend listener: {e}");
                }
            }
            _ = &mut resolver_task => { }
            _ = &mut udp_task => { }
            _ = &mut ping_task => { }
            _ = &mut escape_task => { }
        }

        let _ = emulate_channel.send(EmulationEvent::Terminate).await;
        let _ = capture_channel.send(CaptureEvent::Terminate).await;
        let _ = frontend_tx.send(FrontendEvent::Shutdown()).await;

        if !capture_task.is_finished() {
            if let Err(e) = capture_task.await {
                log::error!("error in input capture task: {e}");
            }
        }
        if !emulation_task.is_finished() {
            if let Err(e) = emulation_task.await {
                log::error!("error in input emulation task: {e}");
            }
        }

        if !frontend_task.is_finished() {
            if let Err(e) = frontend_task.await {
                log::error!("error in frontend listener: {e}");
            }
        }

        resolver_task.abort();
        discovery_task.abort();
        heartbeat.abort();
        escape_task.abort();
        udp_task.abort();
        ping_task.abort();

        Ok(())
    }
}
