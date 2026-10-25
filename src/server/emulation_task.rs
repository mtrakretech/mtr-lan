use anyhow::{anyhow, Result};
use std::net::SocketAddr;

use tokio::{
    sync::mpsc::{Receiver, Sender},
    task::JoinHandle,
};

use crate::{
    client::{ClientEvent, ClientHandle},
    emulate::InputEmulation,
    event::{Event, KeyboardEvent},
    server::State,
};

use super::{CaptureEvent, Server};

#[derive(Clone, Debug)]
pub enum EmulationEvent {
    ReleaseSnapshot(ClientHandle, Vec<u32>),
    /// input emulation is notified of a change in client states
    ClientEvent(ClientEvent),
    /// input emulation must release keys for client
    ReleaseKeys(ClientHandle),
    /// termination signal
    Terminate,
}

pub fn new(
    mut emulate: Box<dyn InputEmulation>,
    server: Server,
    mut udp_rx: Receiver<Result<(Event, SocketAddr)>>,
    sender_tx: Sender<(Event, SocketAddr)>,
    capture_tx: Sender<CaptureEvent>,
    timer_tx: Sender<()>,
) -> (JoinHandle<Result<()>>, Sender<EmulationEvent>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let emulate_task = tokio::task::spawn_local(async move {
        let mut last_ignored = None;

        loop {
            tokio::select! {
                udp_event = udp_rx.recv() => {
                    let udp_event = match udp_rx_value(udp_event)? { Some(event) => event, None => continue };
                    handle_udp_rx(&server, &capture_tx, &mut emulate, &sender_tx, &mut last_ignored, udp_event, &timer_tx).await;
                }
                emulate_event = rx.recv() => {
                    match emulate_event {
                        Some(e) => match e {
                            EmulationEvent::ReleaseSnapshot(client, keys) => { for key in keys { emulate.consume(Event::Keyboard(KeyboardEvent::Key { time: 0, key, state: 0 }), client).await; } },
                            EmulationEvent::ClientEvent(e) => emulate.notify(e).await,
                            EmulationEvent::ReleaseKeys(c) => release_keys(&server, &mut emulate, c).await,
                            EmulationEvent::Terminate => break,
                        },
                        None => break,
                    }
                }
                res = emulate.dispatch() => {
                    res?;
                }
            }
        }

        // release potentially still pressed keys
        let clients = server
            .client_manager
            .borrow()
            .get_client_states()
            .map(|s| s.client.handle)
            .collect::<Vec<_>>();
        for client in clients {
            release_keys(&server, &mut emulate, client).await;
        }

        // destroy emulator
        emulate.destroy().await;
        anyhow::Ok(())
    });
    (emulate_task, tx)
}

async fn handle_udp_rx(
    server: &Server,
    capture_tx: &Sender<CaptureEvent>,
    emulate: &mut Box<dyn InputEmulation>,
    sender_tx: &Sender<(Event, SocketAddr)>,
    last_ignored: &mut Option<SocketAddr>,
    event: (Event, SocketAddr),
    timer_tx: &Sender<()>,
) {
    let (event, addr) = event;

    // get handle for addr
    let handle = match server.client_manager.borrow().get_client(addr) {
        Some(a) => a,
        None => {
            if last_ignored.is_none() || last_ignored.is_some() && last_ignored.unwrap() != addr {
                log::warn!("ignoring events from client {addr}");
                last_ignored.replace(addr);
            }
            return;
        }
    };

    // next event can be logged as ignored again
    last_ignored.take();

    {
        let mut client_manager = server.client_manager.borrow_mut();
        let client_state = match client_manager.get_mut(handle) {
            Some(s) => s,
            None => {
                log::error!("unknown handle");
                return;
            }
        };

        // reset ttl for client and
        client_state.alive = true;
        client_state.last_seen = Some(std::time::Instant::now());
        if matches!(event, Event::Pong()) {
            client_state.latency_ms = client_state
                .last_ping
                .map(|t| t.elapsed().as_millis() as u64);
        }
        // set addr as new default for this client
        client_state.active_addr = Some(addr);
    }

    // Real pointer/key activity (not control packets) arms the escape gesture.
    if !matches!(event, Event::Ping() | Event::Pong()) {
        server
            .last_remote_input
            .set(Some(std::time::Instant::now()));
    }

    if server.role.get() == crate::config::Role::Host
        && !matches!(
            event,
            Event::Ping() | Event::Pong() | Event::Disconnect() | Event::Leave()
        )
    {
        return; // A host never accepts a second host's keyboard/mouse injection.
    }

    match (event, addr) {
        (Event::Pong(), _) => { /* ignore pong events */ }
        (Event::Ping(), addr) => {
            let _ = sender_tx.send((Event::Pong(), addr)).await;
        }
        (Event::Disconnect(), _) => {
            let label = server
                .client_manager
                .borrow()
                .get(handle)
                .map(|s| s.client.label())
                .unwrap_or_else(|| format!("handle {handle}"));
            log::info!(
                "sharing on this PC ended: disconnect received from {label} (return gesture or deliberate release)"
            );
            release_keys(server, emulate, handle).await;
            if server.active_client.get() == Some(handle) {
                let _ = capture_tx.send(CaptureEvent::Release).await;
                server.state.set(State::Receiving);
            }
        }
        (event, addr) => {
            // tell clients that we are ready to receive events
            if let Event::Enter() = event {
                let _ = sender_tx.send((Event::Leave(), addr)).await;
            }

            match server.state.get() {
                State::Sending => {
                    if let Event::Leave() = event {
                        // ignore additional leave events that may
                        // have been sent for redundancy
                    } else {
                        // upon receiving any event, we go back to receiving mode
                        server.state.replace(State::Receiving);
                        let _ = capture_tx.send(CaptureEvent::Release).await;
                        log::trace!("STATE ===> Receiving");
                    }
                }
                State::Receiving => {
                    let mut ignore_event = false;
                    if let Event::Keyboard(KeyboardEvent::Key {
                        time: _,
                        key,
                        state,
                    }) = event
                    {
                        let mut client_manager = server.client_manager.borrow_mut();
                        let client_state =
                            if let Some(client_state) = client_manager.get_mut(handle) {
                                client_state
                            } else {
                                log::error!("unknown handle");
                                return;
                            };
                        if state == 0 {
                            // ignore release event if key not pressed
                            ignore_event = !client_state.pressed_keys.remove(&key);
                        } else {
                            // ignore press event if key not released
                            ignore_event = !client_state.pressed_keys.insert(key);
                            let _ = timer_tx.try_send(());
                        }
                    }
                    // ignore double press / release events to
                    // workaround buggy rdp backend.
                    if !ignore_event {
                        // consume event
                        emulate.consume(event, handle).await;
                    }
                }
                State::AwaitingLeave => {
                    // we just entered the deadzone of a client, so
                    // we need to ignore events that may still
                    // be on the way until a leave event occurs
                    // telling us the client registered the enter
                    if let Event::Leave() = event {
                        server.state.replace(State::Sending);
                        log::trace!("STATE ===> Sending");
                    }

                    // entering a client that is waiting for a leave
                    // event should still be possible
                    if let Event::Enter() = event {
                        server.state.replace(State::Receiving);
                        let _ = capture_tx.send(CaptureEvent::Release).await;
                        log::trace!("STATE ===> Receiving");
                    }
                }
            }
        }
    }
}

fn udp_rx_value(value: Option<Result<(Event, SocketAddr)>>) -> Result<Option<(Event, SocketAddr)>> {
    match value {
        Some(Ok(event)) => Ok(Some(event)),
        Some(Err(_)) => Ok(None), // Malformed LAN traffic must never terminate input service.
        None => Err(anyhow!("receiver closed")),
    }
}

async fn release_keys(
    server: &Server,
    emulate: &mut Box<dyn InputEmulation>,
    client: ClientHandle,
) {
    let keys = server
        .client_manager
        .borrow_mut()
        .get_mut(client)
        .iter_mut()
        .flat_map(|s| s.pressed_keys.drain())
        .collect::<Vec<_>>();

    for key in keys {
        let event = Event::Keyboard(KeyboardEvent::Key {
            time: 0,
            key,
            state: 0,
        });
        emulate.consume(event, client).await;
        log::debug!("Released a held remote key");
    }

    let modifiers_event = KeyboardEvent::Modifiers {
        mods_depressed: 0,
        mods_latched: 0,
        mods_locked: 0,
        group: 0,
    };
    emulate
        .consume(Event::Keyboard(modifiers_event), client)
        .await;
}
