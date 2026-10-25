//! Visible, opt-in Windows host capture. Only explicitly enabled, responsive peers
//! can be entered. Client mode passes every local input event through unchanged.
use crate::{
    capture::InputCapture,
    client::{ClientEvent, ClientHandle, Position},
    event::{Event, KeyboardEvent, PointerEvent},
};
use futures::Stream;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    thread,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use windows_sys::Win32::{
    Foundation::*,
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

#[derive(Default)]
struct State {
    enabled: bool,
    peers: BTreeMap<ClientHandle, (Position, bool)>,
    active: Option<ClientHandle>,
    anchor: (i32, i32),
    restore: (i32, i32),
    restore_pending: bool,
    warping: bool,
    armed: bool,
    pending_disconnect: Option<ClientHandle>,
    modifiers: std::collections::BTreeSet<u32>,
    /// Within this window after a release, a renewed edge cross does not
    /// capture input — this prevents one movement from ping-ponging between
    /// the two computers.
    release_lock: Option<Instant>,
    /// Human-readable reason for the next release, for the service log.
    release_cause: Option<&'static str>,
    /// Rate limiter for the "no ready peer at this edge" diagnostic.
    last_edge_miss: Option<(Position, Instant)>,
    /// Rate limiter for the "entry paused after a release" diagnostic.
    last_entry_suppress: Option<Instant>,
}
struct HookContext {
    state: Arc<Mutex<State>>,
    events: mpsc::Sender<(ClientHandle, Event)>,
}
thread_local! { static CONTEXT: RefCell<Option<HookContext>> = const { RefCell::new(None) }; }
const WARP_POINTER: u32 = WM_APP + 21;
const RESTORE_POINTER: u32 = WM_APP + 22;
/// Tolerance for the outer pixel of the virtual desktop: mixed-DPI setups
/// clamp or round the cursor near monitors, so an exact bound check misses.
const ENTRY_SLACK: i32 = 2;
/// Distance from the entry edge where the pointer is parked after a release,
/// so continuing the same movement does not immediately re-enter the client.
const RESTORE_INSET: i32 = 72;
/// Minimum pause between a release and the next entry on the same edge.
const ENTRY_REPEAT_LOCK: Duration = Duration::from_millis(700);
/// One diagnostic miss per edge per this window, never a log storm.
const MISS_REPORT: Duration = Duration::from_secs(10);

fn position_pointer(point: (i32, i32)) -> io::Result<()> {
    unsafe {
        let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let result = SetCursorPos(point.0, point.1) != 0;
        if previous != 0 {
            SetThreadDpiAwarenessContext(previous);
        }
        if result {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// Point pulled inward from whichever outer edges the cursor touched at entry,
/// so a release parks the pointer inside the screen instead of at the edge.
fn inner_restore(
    point: (i32, i32),
    (left, top, right, bottom): (i32, i32, i32, i32),
) -> (i32, i32) {
    let (mut x, mut y) = point;
    if x >= right - ENTRY_SLACK {
        x = (right - RESTORE_INSET).max(left + 1);
    } else if x <= left + ENTRY_SLACK {
        x = (left + RESTORE_INSET).min(right - 1);
    }
    if y >= bottom - ENTRY_SLACK {
        y = (bottom - RESTORE_INSET).max(top + 1);
    } else if y <= top + ENTRY_SLACK {
        y = (top + RESTORE_INSET).min(bottom - 1);
    }
    (x, y)
}

pub struct WindowsInputCapture {
    state: Arc<Mutex<State>>,
    events: mpsc::Receiver<(ClientHandle, Event)>,
    thread_id: u32,
    worker: Option<thread::JoinHandle<()>>,
}

impl WindowsInputCapture {
    fn release_if_needed(&mut self) -> io::Result<()> {
        let pending = self
            .state
            .lock()
            .map(|state| state.restore_pending)
            .unwrap_or(false);
        if pending {
            self.release()?;
        }
        Ok(())
    }
    pub(crate) fn new() -> anyhow::Result<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let (events_tx, events) = mpsc::channel(1024);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let shared = state.clone();
        let worker = thread::Builder::new()
            .name("windows-host-capture".into())
            .spawn(move || unsafe {
                SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                let mut message: MSG = std::mem::zeroed();
                // Ensure the thread has a message queue before exposing its ID.
                PeekMessageW(&mut message, 0, 0, 0, PM_NOREMOVE);
                CONTEXT.with(|context| {
                    *context.borrow_mut() = Some(HookContext {
                        state: shared,
                        events: events_tx,
                    })
                });
                let module = GetModuleHandleW(std::ptr::null());
                let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), module, 0);
                let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), module, 0);
                if mouse == 0 || keyboard == 0 {
                    let error = io::Error::last_os_error();
                    if mouse != 0 {
                        UnhookWindowsHookEx(mouse);
                    }
                    if keyboard != 0 {
                        UnhookWindowsHookEx(keyboard);
                    }
                    let _ = ready_tx.send(Err(error));
                    return;
                }
                if ready_tx.send(Ok(GetCurrentThreadId())).is_err() {
                    UnhookWindowsHookEx(keyboard);
                    UnhookWindowsHookEx(mouse);
                    return;
                }
                while GetMessageW(&mut message, 0, 0, 0) > 0 {
                    if message.message == WARP_POINTER || message.message == RESTORE_POINTER {
                        let point = CONTEXT.with(|context| {
                            context.borrow().as_ref().and_then(|context| {
                                context.state.lock().ok().and_then(|mut state| {
                                    if message.message == WARP_POINTER {
                                        state.active.map(|_| state.anchor)
                                    } else if state.restore_pending {
                                        state.restore_pending = false;
                                        Some(state.restore)
                                    } else {
                                        None
                                    }
                                })
                            })
                        });
                        if let Some(point) = point {
                            let success = position_pointer(point);
                            if let Err(e) = &success {
                                log::warn!(
                                    "could not move the cursor to ({}, {}): {e}",
                                    point.0,
                                    point.1
                                );
                            }
                            CONTEXT.with(|context| {
                                if let Some(context) = context.borrow().as_ref() {
                                    if let Ok(mut state) = context.state.lock() {
                                        state.warping = false;
                                        let Ok(()) = success else {
                                            state.pending_disconnect = state.active.take();
                                            state.armed = false;
                                            return;
                                        };
                                        if message.message == RESTORE_POINTER {
                                            // Parking pauses renewed entry after a restore.
                                            state.release_lock = Some(Instant::now());
                                        }
                                    }
                                }
                            });
                        }
                        continue;
                    }
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                UnhookWindowsHookEx(keyboard);
                UnhookWindowsHookEx(mouse);
                CONTEXT.with(|context| context.borrow_mut().take());
            })?;
        let thread_id = ready_rx.recv_timeout(std::time::Duration::from_secs(5))??;
        log::info!("Windows input capture initialized; disabled until Host mode is selected");
        Ok(Self {
            state,
            events,
            thread_id,
            worker: Some(worker),
        })
    }
}

impl InputCapture for WindowsInputCapture {
    fn notify(&mut self, event: ClientEvent) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("Host capture state unavailable"))?;
        match event {
            ClientEvent::Create(handle, position) => {
                state.peers.insert(handle, (position, false));
            }
            ClientEvent::Destroy(handle) => {
                state.peers.remove(&handle);
                if state.active == Some(handle) {
                    state.release_cause = Some("peer was disabled or removed from Connections");
                    state.active = None;
                    state.armed = false;
                    state.restore_pending = true;
                    state.pending_disconnect = Some(handle);
                }
            }
        }
        drop(state);
        self.release_if_needed()?;
        Ok(())
    }
    fn release(&mut self) -> io::Result<()> {
        let outcome = if let Ok(mut state) = self.state.lock() {
            let restore =
                (state.active.is_some() || state.restore_pending).then_some(state.restore);
            state.active = None;
            state.armed = false;
            state.warping = false;
            state.restore_pending = false;
            let cause = state.release_cause.take().unwrap_or("release requested");
            restore.map(|point| (point, cause))
        } else {
            None
        };
        if let Some((point, cause)) = outcome {
            match position_pointer(point) {
                Ok(()) => {
                    log::info!(
                        "pointer restored to ({}, {}) after {}; a renewed edge cross is paused briefly",
                        point.0, point.1, cause
                    );
                    if let Ok(mut state) = self.state.lock() {
                        state.release_lock = Some(Instant::now());
                    }
                }
                Err(e) => log::warn!(
                    "could not restore the pointer to ({}, {}) after {}: {e}",
                    point.0,
                    point.1,
                    cause
                ),
            }
        }
        Ok(())
    }
    fn set_enabled(&mut self, enabled: bool) -> io::Result<()> {
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| io::Error::other("Host capture state unavailable"))?;
            state.release_cause = Some(if enabled {
                "host mode selected"
            } else {
                "host mode deselected"
            });
        }
        self.release()?;
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| io::Error::other("Host capture state unavailable"))?;
            state.enabled = enabled;
            state.modifiers.clear();
            state.active = None;
            state.armed = false;
        }
        log::info!(
            "Local host capture {}",
            if enabled {
                "enabled (responsive peers only)"
            } else {
                "disabled; client mode"
            }
        );
        Ok(())
    }
    fn set_ready(&mut self, client: ClientHandle, ready: bool) -> io::Result<()> {
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| io::Error::other("Host capture state unavailable"))?;
            if let Some(peer) = state.peers.get_mut(&client) {
                peer.1 = ready;
            }
            if !ready && state.active == Some(client) {
                state.release_cause = Some("peer stopped responding mid-session");
                state.active = None;
                state.armed = false;
                state.pending_disconnect = Some(client);
                state.restore_pending = true;
            }
        }
        if !ready {
            self.release_if_needed()?;
        }
        Ok(())
    }
}
impl Stream for WindowsInputCapture {
    type Item = io::Result<(ClientHandle, Event)>;
    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let pending = self
            .state
            .lock()
            .ok()
            .and_then(|mut state| state.pending_disconnect.take());
        if let Some(client) = pending {
            while self.events.try_recv().is_ok() {}
            return Poll::Ready(Some(Ok((client, Event::Disconnect()))));
        }
        self.events.poll_recv(context).map(|value| value.map(Ok))
    }
}
impl Drop for WindowsInputCapture {
    fn drop(&mut self) {
        let _ = self.release();
        if let Ok(mut state) = self.state.lock() {
            state.enabled = false;
            state.active = None;
        }
        unsafe {
            PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn queue(context: &HookContext, state: &mut State, client: ClientHandle, event: Event) -> bool {
    if context.events.try_send((client, event)).is_ok() {
        true
    } else {
        state.release_cause = Some("input queue overload (fail-open)");
        state.active = None;
        state.armed = false;
        state.pending_disconnect = Some(client);
        state.restore_pending = true;
        unsafe {
            PostThreadMessageW(GetCurrentThreadId(), RESTORE_POINTER, 0, 0);
        }
        false
    } // Fail open locally.
}

fn keycode(scan: u32, extended: bool) -> Option<u32> {
    let scan = (scan as u16) | if extended { 0xe000 } else { 0 };
    keycode::KeyMap::from_key_mapping(keycode::KeyMapping::Win(scan))
        .ok()
        .map(|key| key.evdev as u32)
        .filter(|key| *key > 0 && crate::scancode::Linux::try_from(*key).is_ok())
}

unsafe extern "system" fn keyboard_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(0, code, message, data);
    }
    let key = &*(data as *const KBDLLHOOKSTRUCT);
    if key.flags & LLKHF_INJECTED != 0 {
        return CallNextHookEx(0, code, message, data);
    }
    let handled = CONTEXT.with(|context| {
        let context = context.borrow();
        let Some(context) = context.as_ref() else {
            return false;
        };
        let Ok(mut state) = context.state.lock() else {
            return false;
        };
        if !state.enabled {
            return false;
        }
        let down = message as u32 == WM_KEYDOWN || message as u32 == WM_SYSKEYDOWN;
        if [
            VK_LCONTROL,
            VK_RCONTROL,
            VK_CONTROL,
            VK_LMENU,
            VK_RMENU,
            VK_MENU,
            VK_LSHIFT,
            VK_RSHIFT,
            VK_SHIFT,
        ]
        .iter()
        .any(|modifier| *modifier as u32 == key.vkCode)
        {
            if down {
                state.modifiers.insert(key.vkCode);
            } else {
                state.modifiers.remove(&key.vkCode);
            }
        }
        let Some(client) = state.active else {
            return false;
        };
        // Hook-level fail-safe is independent of the Tokio/UI threads and network.
        if down
            && key.vkCode == VK_F12 as u32
            && [
                [VK_CONTROL, VK_LCONTROL, VK_RCONTROL],
                [VK_MENU, VK_LMENU, VK_RMENU],
                [VK_SHIFT, VK_LSHIFT, VK_RSHIFT],
            ]
            .iter()
            .all(|group| {
                group
                    .iter()
                    .any(|key| state.modifiers.contains(&(*key as u32)))
            })
        {
            log::info!("emergency release shortcut detected (Ctrl+Alt+Shift+F12)");
            state.release_cause = Some("emergency release shortcut (Ctrl+Alt+Shift+F12)");
            let _ = queue(context, &mut state, client, Event::Disconnect());
            state.active = None;
            state.armed = false;
            state.restore_pending = true;
            PostThreadMessageW(GetCurrentThreadId(), RESTORE_POINTER, 0, 0);
            return true;
        }
        let Some(keycode) = keycode(key.scanCode, key.flags & LLKHF_EXTENDED != 0) else {
            return false;
        };
        queue(
            context,
            &mut state,
            client,
            Event::Keyboard(KeyboardEvent::Key {
                time: key.time,
                key: keycode,
                state: u8::from(down),
            }),
        )
    });
    if handled {
        1
    } else {
        CallNextHookEx(0, code, message, data)
    }
}

unsafe extern "system" fn mouse_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(0, code, message, data);
    }
    let mouse = &*(data as *const MSLLHOOKSTRUCT);
    if mouse.flags & LLMHF_INJECTED != 0 {
        return CallNextHookEx(0, code, message, data);
    }
    let handled = CONTEXT.with(|context| {
        let context = context.borrow();
        let Some(context) = context.as_ref() else {
            return false;
        };
        let Ok(mut state) = context.state.lock() else {
            return false;
        };
        if !state.enabled {
            return false;
        }
        if state.warping && message as u32 == WM_MOUSEMOVE {
            return (mouse.pt.x, mouse.pt.y) != state.anchor;
        }
        if state.active.is_none() {
            if message as u32 != WM_MOUSEMOVE {
                return false;
            }
            let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let right = left + GetSystemMetrics(SM_CXVIRTUALSCREEN) - 1;
            let bottom = top + GetSystemMetrics(SM_CYVIRTUALSCREEN) - 1;
            if mouse.pt.x > left + 8
                && mouse.pt.x < right - 8
                && mouse.pt.y > top + 8
                && mouse.pt.y < bottom - 8
            {
                state.armed = true;
            }
            if !state.armed {
                return false;
            }
            let edge = if mouse.pt.x <= left + ENTRY_SLACK {
                Some(Position::Left)
            } else if mouse.pt.x >= right - ENTRY_SLACK {
                Some(Position::Right)
            } else if mouse.pt.y <= top + ENTRY_SLACK {
                Some(Position::Top)
            } else if mouse.pt.y >= bottom - ENTRY_SLACK {
                Some(Position::Bottom)
            } else {
                None
            };
            let Some(position) = edge else {
                return false;
            };
            // Right after a release the pointer parks inside the screen, but a
            // bouncing cursor may still skim the edge: pause re-entry briefly.
            let now = Instant::now();
            if state
                .release_lock
                .is_some_and(|then| now.duration_since(then) < ENTRY_REPEAT_LOCK)
            {
                let report = state
                    .last_entry_suppress
                    .map_or(true, |then| now.duration_since(then) > MISS_REPORT);
                if report {
                    state.last_entry_suppress = Some(now);
                    log::info!(
                            "pointer touched the {:?} edge moments after a release; re-entry paused briefly to avoid flicker",
                            position
                        );
                }
                return false;
            }
            let target = state
                .peers
                .iter()
                .find(|(_, (side, ready))| *side == position && *ready)
                .map(|(handle, _)| *handle);
            let Some(client) = target else {
                let report = state.last_edge_miss.map_or(true, |(last_pos, then)| {
                    last_pos != position || now.duration_since(then) > MISS_REPORT
                });
                if report && !state.peers.is_empty() {
                    // only diagnose when peers are configured at all
                    if state.peers.iter().any(|(_, (side, _))| *side == position) {
                        state.last_edge_miss = Some((position, now));
                        let stale: Vec<String> = state
                            .peers
                            .iter()
                            .filter(|(_, (side, _))| *side == position)
                            .map(|(handle, _)| handle.to_string())
                            .collect();
                        log::info!(
                                "pointer at the {:?} edge, but no responding peer there: enabled peer(s) at this side not answering heartbeats yet (handles: {}); entry waits for a responding peer",
                                position,
                                stale.join(", ")
                            );
                    } else if !state.peers.is_empty() {
                        state.last_edge_miss = Some((position, now));
                        log::info!(
                                "pointer at the {:?} edge, but no enabled peer is configured on that side; peer positions are set per computer under Connections",
                                position
                            );
                    }
                }
                return false;
            };
            let mut point = POINT { x: 0, y: 0 };
            GetCursorPos(&mut point);
            let restore = inner_restore((point.x, point.y), (left, top, right, bottom));
            state.restore = restore;
            let anchor = (
                GetSystemMetrics(SM_CXSCREEN) / 2,
                GetSystemMetrics(SM_CYSCREEN) / 2,
            );
            state.anchor = anchor;
            log::info!(
                "pointer entered a responding peer (handle {client}) across the {:?} edge; pointer parked at ({}, {}), restore point inside the screen at ({}, {})",
                position,
                anchor.0,
                anchor.1,
                restore.0,
                restore.1
            );
            state.warping = true;
            state.active = Some(client);
            let queued = queue(context, &mut state, client, Event::Enter());
            if queued && PostThreadMessageW(GetCurrentThreadId(), WARP_POINTER, 0, 0) == 0 {
                state.release_cause = Some("pointer warp could not be scheduled");
                state.active = None;
                state.armed = false;
                state.warping = false;
                state.pending_disconnect = Some(client);
                return false;
            }
            return queued;
        }
        let client = state.active.unwrap();
        let event = match message as u32 {
            WM_MOUSEMOVE => Event::Pointer(PointerEvent::Motion {
                time: mouse.time,
                relative_x: (mouse.pt.x - state.anchor.0) as f64,
                relative_y: (mouse.pt.y - state.anchor.1) as f64,
            }),
            WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
            | WM_MBUTTONUP => {
                let (button, down) = match message as u32 {
                    WM_LBUTTONDOWN => (0x110, true),
                    WM_LBUTTONUP => (0x110, false),
                    WM_RBUTTONDOWN => (0x111, true),
                    WM_RBUTTONUP => (0x111, false),
                    WM_MBUTTONDOWN => (0x112, true),
                    _ => (0x112, false),
                };
                Event::Pointer(PointerEvent::Button {
                    time: mouse.time,
                    button,
                    state: u32::from(down),
                })
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let delta = (mouse.mouseData >> 16) as i16;
                Event::Pointer(PointerEvent::Axis {
                    time: mouse.time,
                    axis: u8::from(message as u32 == WM_MOUSEHWHEEL),
                    value: -(delta as f64) / 15.0,
                })
            }
            _ => return false,
        };
        let handled = queue(context, &mut state, client, event);
        if handled {
            let _ = queue(
                context,
                &mut state,
                client,
                Event::Pointer(PointerEvent::Frame {}),
            );
        }
        handled
    });
    if handled {
        1
    } else {
        CallNextHookEx(0, code, message, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_scan_codes_translate_without_layout_or_text_logging() {
        assert_eq!(keycode(0x1e, false), Some(30)); // A
        assert_eq!(keycode(0x1d, true), Some(97)); // Right control
    }
    #[test]
    fn input_is_disabled_until_explicit_host_selection() {
        assert!(!State::default().enabled);
    }

    #[test]
    fn callback_converts_input_without_installing_hooks_or_injecting_input() {
        let shared = Arc::new(Mutex::new(State {
            enabled: true,
            active: Some(7),
            anchor: (100, 100),
            ..State::default()
        }));
        let (tx, mut rx) = mpsc::channel(8);
        CONTEXT.with(|context| {
            *context.borrow_mut() = Some(HookContext {
                state: shared,
                events: tx,
            })
        });
        let mouse = MSLLHOOKSTRUCT {
            pt: POINT { x: 104, y: 98 },
            mouseData: 0,
            flags: 0,
            time: 1,
            dwExtraInfo: 0,
        };
        assert_eq!(
            unsafe { mouse_hook(0, WM_MOUSEMOVE as _, &mouse as *const _ as _) },
            1
        );
        assert!(matches!(
            rx.try_recv().unwrap(),
            (
                7,
                Event::Pointer(PointerEvent::Motion {
                    relative_x: 4.0,
                    relative_y: -2.0,
                    ..
                })
            )
        ));
        let _ = rx.try_recv();
        let key = KBDLLHOOKSTRUCT {
            vkCode: 0x41,
            scanCode: 0x1e,
            flags: 0,
            time: 2,
            dwExtraInfo: 0,
        };
        assert_eq!(
            unsafe { keyboard_hook(0, WM_KEYDOWN as _, &key as *const _ as _) },
            1
        );
        assert!(matches!(
            rx.try_recv().unwrap(),
            (
                7,
                Event::Keyboard(KeyboardEvent::Key {
                    key: 30,
                    state: 1,
                    ..
                })
            )
        ));
        CONTEXT.with(|context| context.borrow_mut().take());
    }

    #[test]
    fn emergency_chord_releases_locally_before_network_processing() {
        let shared = Arc::new(Mutex::new(State {
            enabled: true,
            active: Some(9),
            modifiers: [VK_LCONTROL as u32, VK_LMENU as u32, VK_LSHIFT as u32]
                .into_iter()
                .collect(),
            ..State::default()
        }));
        let (tx, mut rx) = mpsc::channel(4);
        CONTEXT.with(|context| {
            *context.borrow_mut() = Some(HookContext {
                state: shared.clone(),
                events: tx,
            })
        });
        let key = KBDLLHOOKSTRUCT {
            vkCode: VK_F12 as u32,
            scanCode: 0x58,
            flags: 0,
            time: 0,
            dwExtraInfo: 0,
        };
        assert_eq!(
            unsafe { keyboard_hook(0, WM_KEYDOWN as _, &key as *const _ as _) },
            1
        );
        assert!(shared.lock().unwrap().active.is_none());
        assert!(matches!(rx.try_recv().unwrap(), (9, Event::Disconnect())));
        CONTEXT.with(|context| context.borrow_mut().take());
    }

    #[test]
    fn full_capture_queue_fails_open_and_requests_disconnect() {
        let (tx, _rx) = mpsc::channel(1);
        let context = HookContext {
            state: Arc::new(Mutex::new(State::default())),
            events: tx,
        };
        let mut state = State {
            active: Some(3),
            ..State::default()
        };
        assert!(queue(&context, &mut state, 3, Event::Enter()));
        assert!(!queue(&context, &mut state, 3, Event::Enter()));
        assert!(state.active.is_none());
        assert_eq!(state.pending_disconnect, Some(3));
    }
}
