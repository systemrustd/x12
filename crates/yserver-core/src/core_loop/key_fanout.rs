//! State-borrowing key event fanout.
//!
//! KeyPress / KeyRelease delivery follows the X11 keyboard model: at
//! most one window receives the event (the active grab's window if a
//! grab is in effect, otherwise the focused window). The event is
//! emitted to subscribers with `KeyPressMask` / `KeyReleaseMask` on
//! that window. XI2 device-event subscribers on the same window also
//! receive a parallel XI2 KeyPress / KeyRelease.
//!
//! "Focus" comes from the per-client `ClientState::focused_window`.
//! In practice all clients share the same value (every `SetInputFocus`
//! mirrors it across clients), so the helper picks the first non-ROOT
//! focus it sees. When every client is rooted, the event is dropped.

use yserver_protocol::x11::{self, ClientId, ResourceId};

use crate::{
    core_loop::fanout::{emit_window_event_to_state, fanout_event_to_clients},
    host_x11::HostKeyEvent,
    resources::ROOT_WINDOW,
    server::{ActiveKeyboardGrab, ActiveKeyboardGrabSource, ServerState, xi2_mask_for_client},
};

const KEY_PRESS_MASK: u32 = 0x0000_0001;
const KEY_RELEASE_MASK: u32 = 0x0000_0002;
const XI2_MAJOR_OPCODE: u8 = 137;
const XI2_KEYPRESS_EVTYPE: u16 = 2;
const XI2_KEYRELEASE_EVTYPE: u16 = 3;
/// Modifier bits delivered on the wire (Shift|Lock|Control|Mod1|Mod4 = 0x004d).
const WIRE_MODIFIER_MASK: u16 = 0x004d;

/// Fan a host key event out to nested clients.
///
/// Returns the deduped list of clients whose outbound buffer overflowed
/// during the fanout — the caller (run_core) issues
/// `Message::ClientDisconnected` for each.
pub fn key_event_fanout_to_state(state: &mut ServerState, event: HostKeyEvent) -> Vec<ClientId> {
    let Some(target_window) = key_target_window(state, &event) else {
        return Vec::new();
    };

    // Core KeyPress / KeyRelease.
    let mask_bit = if event.pressed {
        KEY_PRESS_MASK
    } else {
        KEY_RELEASE_MASK
    };
    let mut dropped =
        emit_window_event_to_state(state, target_window, mask_bit, |buf, seq, order| {
            x11::encode_key_event(
                buf,
                order,
                x11::KeyEvent {
                    pressed: event.pressed,
                    keycode: event.keycode,
                    sequence: seq,
                    time: event.time,
                    root: ROOT_WINDOW,
                    event: target_window,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x: event.event_x,
                    event_y: event.event_y,
                    state: event.state & WIRE_MODIFIER_MASK,
                },
            );
        });

    // XI2 device-event fanout. Same target window, picks clients
    // selecting the matching XI2 evtype on `(target, deviceid)` for any
    // of the fallback device candidates [3, 1, 0].
    let xi2_evtype = if event.pressed {
        XI2_KEYPRESS_EVTYPE
    } else {
        XI2_KEYRELEASE_EVTYPE
    };
    let xi2_targets: Vec<ClientId> = state
        .clients
        .iter()
        .filter_map(|(id, client)| {
            let mask = xi2_mask_for_client(client, target_window, target_window, &[3, 1, 0]);
            if mask & (1 << xi2_evtype) != 0 {
                Some(ClientId(*id))
            } else {
                None
            }
        })
        .collect();
    if !xi2_targets.is_empty() {
        let xi2_dropped = fanout_event_to_clients(state, &xi2_targets, |buf, seq, order| {
            x11::encode_xi2_device_event(
                buf,
                order,
                seq,
                XI2_MAJOR_OPCODE,
                xi2_evtype,
                3,
                event.time,
                ROOT_WINDOW,
                target_window,
                ResourceId(0), // child=None; key events target the focus window directly
                event.root_x,
                event.root_y,
                event.event_x,
                event.event_y,
                event.state & WIRE_MODIFIER_MASK,
                u32::from(event.keycode),
                3,
            );
        });
        merge_dropped(&mut dropped, xi2_dropped);
    }
    dropped
}

fn merge_dropped(into: &mut Vec<ClientId>, more: Vec<ClientId>) {
    for cid in more {
        if !into.contains(&cid) {
            into.push(cid);
        }
    }
}

/// Apply X11 keyboard routing rules to derive the target window for
/// `event`. `None` means "drop" (no client has focus or the event would
/// land on root).
fn key_target_window(state: &mut ServerState, event: &HostKeyEvent) -> Option<ResourceId> {
    // Active explicit/passive grab: deliver to grab_window. Release
    // a passive-key grab on the matching key-release.
    if let Some(g) = state.active_keyboard_grab {
        if !event.pressed
            && let ActiveKeyboardGrabSource::PassiveKey { keycode: kc } = g.source
            && kc == event.keycode
        {
            state.active_keyboard_grab = None;
        }
        return Some(g.grab_window);
    }

    let focus = current_focus(state);

    // Press: try to match a passive key grab, activating it.
    if event.pressed
        && let Some((owner, grab_window)) = state
            .find_key_grab(focus, event.keycode, event.state)
            .map(|g| (g.owner, g.grab_window))
    {
        state.active_keyboard_grab = Some(ActiveKeyboardGrab {
            owner,
            grab_window,
            source: ActiveKeyboardGrabSource::PassiveKey {
                keycode: event.keycode,
            },
        });
        return Some(grab_window);
    }

    // Drop key events that would land on root with no grab.
    if focus == ROOT_WINDOW {
        return None;
    }
    Some(focus)
}

/// Pick the current keyboard focus.
///
/// Per-client `focused_window` is intended to be a global value
/// mirrored across clients. Pick the first non-ROOT focus we see; if
/// every client is rooted, return `ROOT_WINDOW`.
fn current_focus(state: &ServerState) -> ResourceId {
    state
        .clients
        .values()
        .map(|c| c.focused_window)
        .find(|f| *f != ROOT_WINDOW)
        .unwrap_or(ROOT_WINDOW)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{ActiveKeyboardGrab, ActiveKeyboardGrabSource, KeyGrab, ServerState};
    use yserver_protocol::x11::ClientId;

    fn key_event(pressed: bool, keycode: u8) -> HostKeyEvent {
        HostKeyEvent {
            pressed,
            keycode,
            time: 1,
            root_x: 10,
            root_y: 20,
            event_x: 10,
            event_y: 20,
            state: 0,
        }
    }

    #[test]
    fn dropped_when_focus_is_root_and_no_grabs() {
        // No clients = no focus = nothing to fan out. Just verify the
        // helper returns an empty drop list cleanly.
        let mut state = ServerState::new();
        let dropped = key_event_fanout_to_state(&mut state, key_event(true, 38));
        assert!(dropped.is_empty());
    }

    #[test]
    fn passive_key_grab_activates_on_press_and_clears_on_release() {
        let mut state = ServerState::new();
        // No focus is set on any client — find_key_grab walks up from
        // focus. Setting the grab on ROOT exercises the matching path.
        let grab_owner = ClientId(7);
        state.key_grabs.push(KeyGrab {
            owner: grab_owner,
            grab_window: ROOT_WINDOW,
            keycode: 38,
            modifiers: 0,
            owner_events: false,
            pointer_mode: 1,
            keyboard_mode: 1,
        });
        // Press: activates passive grab.
        let _ = key_event_fanout_to_state(&mut state, key_event(true, 38));
        match state.active_keyboard_grab {
            Some(ActiveKeyboardGrab {
                owner,
                grab_window,
                source: ActiveKeyboardGrabSource::PassiveKey { keycode },
            }) => {
                assert_eq!(owner, grab_owner);
                assert_eq!(grab_window, ROOT_WINDOW);
                assert_eq!(keycode, 38);
            }
            other => panic!("expected PassiveKey grab, got {other:?}"),
        }
        // Release with matching keycode clears the grab.
        let _ = key_event_fanout_to_state(&mut state, key_event(false, 38));
        assert!(state.active_keyboard_grab.is_none());
    }

    #[test]
    fn explicit_grab_persists_across_release() {
        let mut state = ServerState::new();
        state.active_keyboard_grab = Some(ActiveKeyboardGrab {
            owner: ClientId(3),
            grab_window: ResourceId(0x100),
            source: ActiveKeyboardGrabSource::Explicit,
        });
        let _ = key_event_fanout_to_state(&mut state, key_event(false, 38));
        // Explicit grab is NOT cleared by a key release (only passive
        // grabs auto-clear). Persists until UngrabKeyboard.
        assert!(matches!(
            state.active_keyboard_grab,
            Some(ActiveKeyboardGrab {
                source: ActiveKeyboardGrabSource::Explicit,
                ..
            })
        ));
    }
}
