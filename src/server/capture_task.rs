use anyhow::{anyhow, Result};
use futures::StreamExt;
use std::{collections::HashSet, net::SocketAddr};

use tokio::{sync::mpsc::Sender, task::JoinHandle};

use crate::{
    capture::InputCapture,
    client::{ClientEvent, ClientHandle},
    event::{Event, KeyboardEvent},
    scancode,
    server::State,
};

use super::Server;

#[derive(Clone, Copy, Debug)]
pub enum CaptureEvent {
    ReleasePeer(ClientHandle, Option<SocketAddr>),
    SetEnabled(bool),
    SetReady(ClientHandle, bool),
    /// capture must release the mouse
    Release,
    /// capture is notified of a change in client states
    ClientEvent(ClientEvent),
    /// termination signal
    Terminate,
}

pub fn new(
    mut capture: Box<dyn InputCapture>,
    server: Server,
    sender_tx: Sender<(Event, SocketAddr)>,
    timer_tx: Sender<()>,
    release_bind: Vec<scancode::Linux>,
) -> (JoinHandle<Result<()>>, Sender<CaptureEvent>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let task = tokio::task::spawn_local(async move {
        capture.set_enabled(server.role.get() == crate::config::Role::Host)?;
        let mut pressed_keys = HashSet::new();
        loop {
            tokio::select! {
                event = capture.next() => {
                    match event {
                        Some(Ok(event)) => handle_capture_event(&server, &mut capture, &sender_tx, &timer_tx, event, &mut pressed_keys, &release_bind).await?,
                        Some(Err(e)) => return Err(anyhow!("input capture: {e:?}")),
                        None => return Err(anyhow!("input capture terminated")),
                    }
                }
                e = rx.recv() => {
                    log::debug!("input capture notify rx: {e:?}");
                    match e {
                        Some(e) => match e {
                            CaptureEvent::ReleasePeer(client, address) => {
                                if let Some(address) = address { let _ = sender_tx.send((Event::Disconnect(), address)).await; }
                                if server.active_client.get() == Some(client) { capture.release()?; pressed_keys.clear(); server.active_client.set(None); server.state.set(State::Receiving); }
                            },
                            CaptureEvent::SetEnabled(enabled) => {
                                let addresses: Vec<_> = server.client_manager.borrow().get_client_states().filter(|s| s.active).filter_map(|s| s.active_addr).collect();
                                for address in addresses { let _ = sender_tx.send((Event::Disconnect(), address)).await; }
                                disconnect_active(&server, &sender_tx).await; capture.release()?; capture.set_enabled(enabled)?; pressed_keys.clear(); server.state.set(State::Receiving);
                            },
                            CaptureEvent::SetReady(client, ready) => capture.set_ready(client, ready)?,
                            CaptureEvent::Release => {
                                disconnect_active(&server, &sender_tx).await;
                                capture.release()?;
                                pressed_keys.clear();
                                server.state.replace(State::Receiving);

                            }
                            CaptureEvent::ClientEvent(e) => capture.notify(e)?,
                            CaptureEvent::Terminate => break,
                        },
                        None => break,
                    }
                }
            }
        }
        anyhow::Ok(())
    });
    (task, tx)
}

async fn disconnect_active(server: &Server, sender: &Sender<(Event, SocketAddr)>) {
    let address = server.active_client.get().and_then(|c| {
        server
            .client_manager
            .borrow()
            .get(c)
            .and_then(|s| s.active_addr)
    });
    if let Some(address) = address {
        let _ = sender.send((Event::Disconnect(), address)).await;
    } else if server.role.get() == crate::config::Role::Client {
        let addresses: Vec<_> = server
            .client_manager
            .borrow()
            .get_client_states()
            .filter(|s| s.active)
            .filter_map(|s| s.active_addr)
            .collect();
        for address in addresses {
            let _ = sender.send((Event::Disconnect(), address)).await;
        }
    }
    server.active_client.set(None);
}

fn update_pressed_keys(pressed_keys: &mut HashSet<scancode::Linux>, key: u32, state: u8) {
    if let Ok(scancode) = scancode::Linux::try_from(key) {
        match state {
            1 => pressed_keys.insert(scancode),
            _ => pressed_keys.remove(&scancode),
        };
    }
}

async fn handle_capture_event(
    server: &Server,
    capture: &mut Box<dyn InputCapture>,
    sender_tx: &Sender<(Event, SocketAddr)>,
    timer_tx: &Sender<()>,
    event: (ClientHandle, Event),
    pressed_keys: &mut HashSet<scancode::Linux>,
    release_bind: &[scancode::Linux],
) -> Result<()> {
    let (c, mut e) = event;
    if server.role.get() != crate::config::Role::Host {
        capture.release()?;
        return Ok(());
    }
    if e == Event::Disconnect() {
        server.state.set(State::Receiving);
        capture.release()?;
        pressed_keys.clear();
    }

    if let Event::Keyboard(KeyboardEvent::Key { key, state, .. }) = e {
        update_pressed_keys(pressed_keys, key, state);
        if release_bind.iter().all(|k| pressed_keys.contains(k)) {
            pressed_keys.clear();
            log::info!("releasing pointer");
            capture.release()?;
            server.state.replace(State::Receiving);
            log::trace!("STATE ===> Receiving");
            // send an event to release all the modifiers
            e = Event::Disconnect();
        }
    }

    let (addr, enter, start_timer) = {
        let mut enter = false;
        let mut start_timer = false;

        // get client state for handle
        let mut client_manager = server.client_manager.borrow_mut();
        let client_state = match client_manager.get_mut(c) {
            Some(state) => state,
            None => {
                // should not happen
                log::warn!("unknown client!");
                capture.release()?;
                server.state.replace(State::Receiving);
                log::trace!("STATE ===> Receiving");
                return Ok(());
            }
        };

        // if we just entered the client we want to send additional enter events until
        // we get a leave event
        if let Event::Enter() = e {
            server.state.replace(State::AwaitingLeave);
            server
                .active_client
                .replace(Some(client_state.client.handle));
            log::trace!("Active client => {}", client_state.client.handle);
            log::info!(
                "sharing started: pointer crossed the {} edge toward peer '{}' (handle {})",
                client_state.client.pos,
                client_state.client.label(),
                client_state.client.handle
            );
            start_timer = true;
            log::trace!("STATE ===> AwaitingLeave");
            enter = true;
        } else {
            // ignore any potential events in receiving mode
            if server.state.get() == State::Receiving && e != Event::Disconnect() {
                return Ok(());
            }
        }

        (client_state.active_addr, enter, start_timer)
    };
    if start_timer {
        let _ = timer_tx.try_send(());
    }
    if let Some(addr) = addr {
        if enter {
            let _ = sender_tx.send((Event::Enter(), addr)).await;
        }
        let _ = sender_tx.send((e, addr)).await;
    }
    Ok(())
}
