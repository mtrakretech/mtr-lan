//! While this PC receives input from a peer, pushing and briefly holding the
//! pointer against the screen edge that faces that peer releases it. Without
//! an active remote session, local pointer use never triggers this.
use super::{emulation_task::EmulationEvent, Server};
use crate::{client::Position, event::Event};
use log;
use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};
use tokio::{sync::mpsc::Sender, task::JoinHandle};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

const HOLD: Duration = Duration::from_millis(600);
const COOLDOWN: Duration = Duration::from_millis(1500);
const EDGE_SLACK: i32 = 2;
/// The gesture may fire up to this long after the last remote motion. This
/// must comfortably outlast the hold time because holding the pointer still
/// stops remote motion events, so a fresh "last input" timestamp is required
/// at the moment the edge hold *starts*, not when it completes.
const ACTIVITY_WINDOW: Duration = Duration::from_secs(3);
const MISS_REPORT: Duration = Duration::from_secs(10);

/// The screen edge that faces the peer. Peer positions store where the peer
/// sits relative to this machine — exactly like the host-side entry check —
/// so a peer configured on this machine's left side is released through the
/// left edge of this screen.
fn escape_edge(position: Position) -> Edge {
    use Position::*;
    match position {
        Left => Edge::Left,
        Right => Edge::Right,
        Top => Edge::Top,
        Bottom => Edge::Bottom,
    }
}

fn edge_name(edge: Edge) -> &'static str {
    match edge {
        Edge::Left => "left",
        Edge::Right => "right",
        Edge::Top => "top",
        Edge::Bottom => "bottom",
    }
}

fn at_edge(
    edge: Edge,
    (x, y): (i32, i32),
    (left, top, right, bottom): (i32, i32, i32, i32),
) -> bool {
    if right < left || bottom < top {
        return false; // no usable desktop (or platform without cursor metrics)
    }
    match edge {
        Edge::Left => x <= left + EDGE_SLACK,
        Edge::Right => x >= right - EDGE_SLACK,
        Edge::Top => y <= top + EDGE_SLACK,
        Edge::Bottom => y >= bottom - EDGE_SLACK,
    }
}

/// Sample the current physical cursor position (remote-injected input included).
#[cfg(windows)]
fn pointer() -> Option<(i32, i32)> {
    use windows_sys::Win32::{Foundation::POINT, UI::WindowsAndMessaging::GetCursorPos};
    let mut point = POINT { x: 0, y: 0 };
    unsafe { (GetCursorPos(&mut point) != 0).then(|| (point.x, point.y)) }
}

#[cfg(not(windows))]
fn pointer() -> Option<(i32, i32)> {
    None
}

/// Inclusive virtual-desktop bounds; degenerate off non-Windows targets.
#[cfg(windows)]
fn screen() -> (i32, i32, i32, i32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    unsafe {
        let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        (left, top, left + width - 1, top + height - 1)
    }
}

#[cfg(not(windows))]
fn screen() -> (i32, i32, i32, i32) {
    (0, 0, -1, -1)
}

struct Candidate {
    handle: u32,
    address: SocketAddr,
    edge: Edge,
    label: String,
}

/// The most recently seen enabled peer with a usable address, and the screen
/// edge through which this machine exchanges input with it.
fn peer_candidate(server: &Server) -> Option<Candidate> {
    let mut best: Option<(Candidate, Instant)> = None;
    for state in server.client_manager.borrow().get_client_states() {
        if !state.active {
            continue;
        }
        let Some(address) = state.active_addr else {
            continue;
        };
        let Some(seen) = state.last_seen else {
            continue;
        };
        let candidate = Candidate {
            handle: state.client.handle,
            address,
            edge: escape_edge(state.client.pos),
            label: state.client.label(),
        };
        if best.as_ref().is_none_or(|(_, t)| seen > *t) {
            best = Some((candidate, seen));
        }
    }
    best.map(|(candidate, _)| candidate)
}

pub fn new(
    server: Server,
    sender: Sender<(Event, SocketAddr)>,
    emulate: Sender<EmulationEvent>,
) -> JoinHandle<()> {
    tokio::task::spawn_local(async move {
        let mut holding: Option<(Edge, Instant)> = None;
        let mut armed_again = Instant::now() - COOLDOWN;
        let mut last_miss: Option<Instant> = None;
        let mut tick = tokio::time::interval(Duration::from_millis(40));
        loop {
            tick.tick().await;
            let now = Instant::now();
            let active = server
                .last_remote_input
                .get()
                .map(|t| now.duration_since(t) < ACTIVITY_WINDOW)
                .unwrap_or(false);
            let role_client = server.role.get() == crate::config::Role::Client;
            let Some(point) = pointer() else {
                holding = None;
                continue;
            };
            let bounds = screen();
            if !role_client || !active {
                holding = None;
                // Diagnostic: the user pushes against the peer-facing edge but
                // no remote session is active. Reported at most once per 10 s.
                if !role_client {
                    continue;
                }
                let outer_at = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom]
                    .into_iter()
                    .any(|edge| at_edge(edge, point, bounds));
                if !outer_at {
                    continue;
                }
                let on_edge_side = server
                    .client_manager
                    .borrow()
                    .get_client_states()
                    .any(|s| s.active && at_edge(escape_edge(s.client.pos), point, bounds));
                if !on_edge_side {
                    continue;
                }
                let reason = match server.last_remote_input.get() {
                    None => "none yet".to_string(),
                    Some(t) => format!("{}s ago (session idle)", now.duration_since(t).as_secs()),
                };
                let report = last_miss.is_none_or(|t| now.duration_since(t) > MISS_REPORT);
                if report {
                    last_miss = Some(now);
                    log::info!(
                        "edge push at a peer-facing screen edge, but no active remote session (last remote input: {reason}); holding about half a second only releases while input is being shared"
                    );
                }
                continue;
            }
            let Some(candidate) = peer_candidate(&server) else {
                holding = None;
                continue;
            };
            if at_edge(candidate.edge, point, bounds) {
                if armed_again.elapsed() < COOLDOWN {
                    holding = None;
                    continue;
                }
                if holding.is_none() {
                    log::info!(
                            "pointer at the {} edge -> return gesture armed; controlling {}, hold about half a second to come back",
                            edge_name(candidate.edge),
                            candidate.label
                        );
                }
                let (_, holding_since) = holding.get_or_insert((candidate.edge, now));
                if now.duration_since(*holding_since) < HOLD {
                    continue;
                }
                let (holding_edge, _) = holding.unwrap_or((candidate.edge, now));
                holding = None;
                armed_again = now;
                log::info!(
                    "pointer held ~0.6 s at the {} edge, releasing remote control -> disconnecting from {}",
                    edge_name(holding_edge),
                    candidate.label
                );
                let _ = sender.send((Event::Disconnect(), candidate.address)).await;
                let _ = emulate
                    .send(EmulationEvent::ReleaseKeys(candidate.handle))
                    .await;
            } else {
                if holding.is_some() {
                    log::info!(
                            "pointer left the {} edge before the hold completed; return gesture cancelled",
                            edge_name(holding.map(|(edge, _)| edge).unwrap_or(candidate.edge))
                        );
                }
                holding = None;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_edge_is_the_same_side_the_peer_sits_on() {
        assert_eq!(escape_edge(Position::Left), Edge::Left);
        assert_eq!(escape_edge(Position::Right), Edge::Right);
        assert_eq!(escape_edge(Position::Top), Edge::Top);
        assert_eq!(escape_edge(Position::Bottom), Edge::Bottom);
    }

    #[test]
    fn held_edges_only_fire_on_a_real_desktop() {
        assert!(at_edge(Edge::Right, (1919, 500), (0, 0, 1919, 1079)));
        assert!(at_edge(Edge::Right, (1917, 500), (0, 0, 1919, 1079)));
        assert!(at_edge(Edge::Left, (0, 0), (0, 0, 1919, 1079)));
        assert!(at_edge(Edge::Left, (2, 0), (0, 0, 1919, 1079)));
        assert!(!at_edge(Edge::Right, (1916, 500), (0, 0, 1919, 1079)));
        assert!(!at_edge(Edge::Left, (3, 0), (0, 0, 1919, 1079)));
        // Degenerate bounds (e.g. no cursor metrics) can never fire.
        assert!(!at_edge(Edge::Left, (0, 0), (0, 0, -1, -1)));
        assert!(!at_edge(Edge::Right, (0, 0), (0, 0, -1, -1)));
        assert!(!at_edge(Edge::Top, (0, 0), (0, 0, -1, -1)));
        assert!(!at_edge(Edge::Bottom, (0, 0), (0, 0, -1, -1)));
    }
}
