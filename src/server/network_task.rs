use std::net::SocketAddr;

use anyhow::Result;
use tokio::{
    net::UdpSocket,
    sync::mpsc::{Receiver, Sender},
    task::JoinHandle,
};

use crate::{event::Event, frontend::FrontendNotify};

use super::Server;

pub async fn new(
    server: Server,
    frontend_notify_tx: Sender<FrontendNotify>,
) -> Result<(
    JoinHandle<()>,
    Sender<(Event, SocketAddr)>,
    Receiver<Result<(Event, SocketAddr)>>,
    Sender<u16>,
)> {
    // bind the udp socket
    let listen_addr = SocketAddr::new("0.0.0.0".parse().unwrap(), server.port.get());
    let mut socket = UdpSocket::bind(listen_addr).await?;
    let (receiver_tx, receiver_rx) = tokio::sync::mpsc::channel(32);
    let (sender_tx, mut sender_rx) = tokio::sync::mpsc::channel::<(Event, SocketAddr)>(32);
    let (port_tx, mut port_rx) = tokio::sync::mpsc::channel(32);

    let udp_task = tokio::task::spawn_local(async move {
        loop {
            tokio::select! {
                event = receive_event(&socket) => {
                    if event.is_ok() {
                        server.packets_received.set(server.packets_received.get().saturating_add(1));
                        server.last_received.set(Some(std::time::Instant::now()));
                    }
                    let _ = receiver_tx.send(event).await;
                }
                event = sender_rx.recv() => {
                    let Some((event, addr)) = event else {
                        break;
                    };
                    if matches!(event, Event::Ping()) {
                        for state in server.client_manager.borrow_mut().get_client_states_mut() {
                            if state.client.ips.contains(&addr.ip()) { state.last_ping = Some(std::time::Instant::now()); }
                        }
                    }
                    if let Err(e) = send_event(&socket, event, addr) {
                        log::warn!("udp send failed: {e}");
                    } else {
                        server.packets_sent.set(server.packets_sent.get().saturating_add(1));
                    };
                }
                port = port_rx.recv() => {
                    let Some(port) = port else {
                        break;
                    };

                    if socket.local_addr().unwrap().port() == port {
                        continue;
                    }

                    let listen_addr = SocketAddr::new("0.0.0.0".parse().unwrap(), port);
                    match UdpSocket::bind(listen_addr).await {
                        Ok(new_socket) => {
                            socket = new_socket;
                            server.port.replace(port);
                            if let Err(e) = server.save_config() {
                                let _ = frontend_notify_tx.send(FrontendNotify::NotifyError(format!("Could not save settings: {e}"))).await;
                            }
                            let _ = frontend_notify_tx.send(FrontendNotify::NotifyPortChange(port, None)).await;
                        }
                        Err(e) => {
                            log::warn!("could not change port: {e}");
                            let port = socket.local_addr().unwrap().port();
                            let _ = frontend_notify_tx.send(FrontendNotify::NotifyPortChange(
                                    port,
                                    Some(format!("could not change port: {e}")),
                                )).await;
                        }
                    }

                }
            }
        }
    });
    Ok((udp_task, sender_tx, receiver_rx, port_tx))
}

async fn receive_event(socket: &UdpSocket) -> Result<(Event, SocketAddr)> {
    let mut buf = vec![0u8; 256];
    let (amt, src) = socket.recv_from(&mut buf).await?;
    buf.truncate(amt);
    Ok((Event::try_from(buf)?, src))
}

fn send_event(sock: &UdpSocket, e: Event, addr: SocketAddr) -> Result<usize> {
    let data: Vec<u8> = (&e).into();
    // When udp blocks, we dont want to block the event loop.
    // Dropping events is better than potentially crashing the input capture.
    Ok(sock.try_send_to(&data, addr)?)
}
