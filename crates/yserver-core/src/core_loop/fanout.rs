//! Core-side event fanout helpers — the state-borrowing replacements
//! for `server::fanout_event` / `server::fanout_raw_event` /
//! `server::pointer_event_fanout`.
//!
//! Each helper takes `&mut ServerState` so it can update each
//! client's `last_sequence`, encode against the client's
//! `byte_order`, and push bytes through `client_io::write_or_buffer`
//! — the same path opcode dispatch will use after the D3 lift.
//! Disconnect outcomes are reported back to the caller as a
//! `Vec<ClientId>` so the core's request-loop can issue
//! `Message::ClientDisconnected` for each one.
//!
//! The pre-lift `EventTarget`-based helpers in `server.rs` remain in
//! place; D3 migrates callers off them.

use std::{collections::HashSet, sync::atomic::Ordering};

use yserver_protocol::x11::{self, ClientByteOrder, ClientId, ResourceId, SequenceNumber};

use crate::{
    core_loop::client_io::{self, WriteOutcome},
    host_x11::{HostExposeEvent, HostXidMap},
    resources::{MapState, ROOT_WINDOW},
    server::ServerState,
};

/// Build a deduped client-id list of every client that selected at
/// least one of the bits in `mask_bits` on `window`.
///
/// Replaces `ServerState::subscribers` for the new fanout API. Order
/// follows `HashMap` iteration — already non-deterministic in the old
/// path, so wire ordering is unchanged.
pub fn subscribers_by_id(state: &ServerState, window: ResourceId, mask_bits: u32) -> Vec<ClientId> {
    state
        .clients
        .iter()
        .filter_map(|(id, c)| {
            let mask = c.event_masks.get(&window).copied().unwrap_or(0);
            if mask & mask_bits != 0 {
                Some(ClientId(*id))
            } else {
                None
            }
        })
        .collect()
}

/// Walk up the parent chain from `start`, returning the first window
/// with any client subscribed to `mask_bits`, the (event_x, event_y)
/// translated to be relative to that window, and the subscriber list.
///
/// Mirror of `ServerState::pointer_propagation_target` but in the new
/// state-borrowing fanout API: returns `Vec<ClientId>` instead of
/// `Vec<EventTarget>`. Order follows `HashMap` iteration (matches the
/// pre-lift behaviour).
/// Returns `(propagation_target, x, y, subscribers, child)` where
/// `child` is the immediate descendant of `propagation_target` along
/// the path to `start` (i.e. the X11 `child` field for ButtonPress /
/// Motion events). `child == ResourceId(0)` is the X11 `None` sentinel
/// and indicates the propagation target was reached without walking up
/// (the click landed directly on the subscribed window).
///
/// Window managers use this `child` field to distinguish a bare-root
/// click — for which they typically open the root menu — from a click
/// on an application window that happened to propagate up because the
/// app didn't select core ButtonPress (modern toolkits select XI2
/// instead). Without an accurate `child`, fvwm3's `Mouse 1 R A Menu`
/// binding fires on every click anywhere in the screen.
#[must_use]
pub fn pointer_propagation_target_by_id(
    state: &ServerState,
    start: ResourceId,
    start_x: i16,
    start_y: i16,
    mask_bits: u32,
) -> Option<(ResourceId, i16, i16, Vec<ClientId>, ResourceId)> {
    let mut current = start;
    let mut x = start_x;
    let mut y = start_y;
    let mut child: Option<ResourceId> = None;
    for _ in 0..256 {
        let subs = subscribers_by_id(state, current, mask_bits);
        if !subs.is_empty() {
            return Some((current, x, y, subs, child.unwrap_or(ResourceId(0))));
        }
        let window = state.resources.window(current)?;
        if window.parent == current {
            return None;
        }
        x = x.wrapping_add(window.x);
        y = y.wrapping_add(window.y);
        child = Some(current);
        current = window.parent;
    }
    None
}

/// Returns `Some(client_id)` if `client_id` corresponds to a registered
/// client. Mirror of `ServerState::client_target` in the new fanout
/// API — useful as a guard before fanning out to a single client.
#[must_use]
pub fn client_target_id(state: &ServerState, client_id: ClientId) -> Option<ClientId> {
    state
        .clients
        .contains_key(&client_id.0)
        .then_some(client_id)
}

/// Mirror of `ServerState::selection_owner_target` in the new fanout
/// API: returns the owner window and the owning client's id.
#[must_use]
pub fn selection_owner_target_id(
    state: &ServerState,
    selection: yserver_protocol::x11::AtomId,
) -> Option<(ResourceId, ClientId)> {
    let owner_window = *state.selections.get(&selection)?;
    let owner_client = state.resources.window_owner(owner_window)?;
    let target = client_target_id(state, owner_client)?;
    Some((owner_window, target))
}

/// `encode(buf, sequence, byte_order)` writes a 32-byte (or larger)
/// X11 event into `buf` against the given sequence/byte order. Same
/// contract as `server::fanout_event`.
pub fn fanout_event_to_clients<F>(
    state: &mut ServerState,
    client_ids: &[ClientId],
    encode: F,
) -> Vec<ClientId>
where
    F: Fn(&mut Vec<u8>, SequenceNumber, ClientByteOrder),
{
    let mut disconnected = Vec::new();
    let mut seen = HashSet::new();
    for cid in client_ids {
        if !seen.insert(cid.0) {
            continue;
        }
        let Some(client) = state.clients.get_mut(&cid.0) else {
            continue;
        };
        let seq = SequenceNumber(client.last_sequence.load(Ordering::Relaxed));
        let order = client.byte_order;
        let mut buf = Vec::with_capacity(32);
        encode(&mut buf, seq, order);
        match client_io::write_or_buffer(client, &buf) {
            Ok(WriteOutcome::Done | WriteOutcome::WouldBlock) => {}
            Ok(WriteOutcome::Disconnect) => disconnected.push(*cid),
            Err(_) => disconnected.push(*cid),
        }
    }
    disconnected
}

/// State-borrowing replacement for `server::emit_window_event`.
///
/// Looks up subscribers to `mask_bits` on `window` directly out of
/// `state.clients`, then encodes per client and writes via
/// `client_io::write_or_buffer`. Returns the list of clients whose
/// outbound buffer overflowed so the core's request loop can issue
/// `Message::ClientDisconnected` for each.
pub fn emit_window_event_to_state<F>(
    state: &mut ServerState,
    window: ResourceId,
    mask_bits: u32,
    encode: F,
) -> Vec<ClientId>
where
    F: Fn(&mut Vec<u8>, SequenceNumber, ClientByteOrder),
{
    let targets = subscribers_by_id(state, window, mask_bits);
    if targets.is_empty() {
        return Vec::new();
    }
    fanout_event_to_clients(state, &targets, encode)
}

/// State-borrowing replacement for `nested::emit_xi2_focus_event`.
///
/// Emits an XI2 FocusIn / FocusOut on `window` to clients selecting
/// the matching XI2 evtype on `(window, deviceid)` for any of the
/// fallback device candidates `[3, 1, 0]` (Master Keyboard, then
/// AllMasterDevices, then AllDevices). The encoding is byte-order
/// agnostic, matching the pre-lift helper.
///
/// `xi2_major_opcode` is the XI extension's runtime-assigned major
/// opcode (137 in the current build).
pub fn emit_xi2_focus_event_to_state(
    state: &mut ServerState,
    window: ResourceId,
    evtype: u16,
    xi2_major_opcode: u8,
) -> Vec<ClientId> {
    let targets: Vec<ClientId> = state
        .clients
        .iter()
        .filter_map(|(id, client)| {
            let mask = client
                .xi2_masks
                .get(&(window, 3))
                .or_else(|| client.xi2_masks.get(&(window, 1)))
                .or_else(|| client.xi2_masks.get(&(window, 0)))
                .copied()
                .unwrap_or(0);
            if mask & (1 << evtype) != 0 {
                Some(ClientId(*id))
            } else {
                None
            }
        })
        .collect();
    if targets.is_empty() {
        return Vec::new();
    }
    fanout_event_to_clients(state, &targets, |buf, seq, order| {
        x11::encode_xi2_focus_event(
            buf,
            order,
            seq,
            xi2_major_opcode,
            evtype,
            3,
            0,
            window,
            0,
            0,
        );
    })
}

/// State-borrowing replacement for `nested::expose_event_fanout`.
///
/// Translates the host xid via `xid_map`, emits an Expose event to
/// every client that selected `ExposureMask` on the resolved nested
/// window, and (for top-level exposes) walks descendants in the
/// exposed area so sub-window children also redraw.
///
/// Returns a deduped list of clients whose outbound buffer overflowed
/// during the fanout.
pub fn expose_event_fanout_to_state(
    state: &mut ServerState,
    xid_map: &HostXidMap,
    ev: HostExposeEvent,
) -> Vec<ClientId> {
    let Some(window) = xid_map.get(&ev.host_xid).copied() else {
        return Vec::new();
    };
    let mut dropped =
        emit_window_event_to_state(state, window, EXPOSURE_MASK_BIT, |buf, seq, order| {
            x11::encode_expose_event(
                buf, seq, order, window, ev.x, ev.y, ev.width, ev.height, ev.count,
            );
        });
    if window == ROOT_WINDOW {
        return dropped;
    }
    let exposed = state.resources.descendants_in_exposed_area(
        window,
        ev.x as i16,
        ev.y as i16,
        ev.width,
        ev.height,
    );
    for rect in exposed {
        let target_window = rect.window;
        let more = emit_window_event_to_state(
            state,
            target_window,
            EXPOSURE_MASK_BIT,
            |buf, seq, order| {
                x11::encode_expose_event(
                    buf,
                    seq,
                    order,
                    target_window,
                    rect.x as u16,
                    rect.y as u16,
                    rect.width,
                    rect.height,
                    0,
                );
            },
        );
        merge_dropped(&mut dropped, more);
    }
    dropped
}

/// State-borrowing replacement for `nested::emit_expose_subtree`.
///
/// Walks every mapped descendant of `root` and emits Expose to those
/// that selected `ExposureMask`. Used after a top-level becomes
/// viewable so deeply-nested widgets repaint immediately.
pub fn emit_expose_subtree_to_state(state: &mut ServerState, root: ResourceId) -> Vec<ClientId> {
    let mut dropped = Vec::new();
    let children: Vec<ResourceId> = state.resources.children(root).to_vec();
    for child in children {
        let extents = state
            .resources
            .window(child)
            .filter(|w| w.map_state == MapState::Viewable)
            .map(|w| (w.width, w.height));
        if let Some((w, h)) = extents {
            let target = child;
            let more =
                emit_window_event_to_state(state, target, EXPOSURE_MASK_BIT, |buf, seq, order| {
                    x11::encode_expose_event(buf, seq, order, target, 0, 0, w, h, 0);
                });
            merge_dropped(&mut dropped, more);
            let recursed = emit_expose_subtree_to_state(state, child);
            merge_dropped(&mut dropped, recursed);
        }
    }
    dropped
}

const EXPOSURE_MASK_BIT: u32 = 0x0000_8000;

fn merge_dropped(into: &mut Vec<ClientId>, more: Vec<ClientId>) {
    for cid in more {
        if !into.contains(&cid) {
            into.push(cid);
        }
    }
}

/// Raw-event variant: `event` is a 32-byte template encoded in
/// `template_byte_order`. For each recipient we copy the template,
/// re-encode into the recipient's byte order via the per-event-type
/// swap table, then patch the sequence number in the recipient's
/// byte order.
///
/// `template_byte_order` is `LittleEndian` for events the server
/// builds itself (SelectionNotify, RANDR, …) and the sender's byte
/// order for `SendEvent`.
pub fn fanout_raw_event_to_clients(
    state: &mut ServerState,
    client_ids: &[ClientId],
    event: &[u8; 32],
    template_byte_order: ClientByteOrder,
) -> Vec<ClientId> {
    use yserver_protocol::x11::wire_swap;
    let mut disconnected = Vec::new();
    let mut seen = HashSet::new();
    let event_type = event[0] & 0x7f;
    let entries = wire_swap::core_event_swap_table(event_type);
    for cid in client_ids {
        if !seen.insert(cid.0) {
            continue;
        }
        let Some(client) = state.clients.get_mut(&cid.0) else {
            continue;
        };
        let recipient_order = client.byte_order;
        let mut buf = *event;
        // Step 1: undo source byte order to native LE so the swap to
        // recipient byte order produces correct bytes.
        wire_swap::swap_in_place(entries, template_byte_order, &mut buf);
        // Step 2: convert from native LE to recipient byte order.
        wire_swap::swap_in_place(entries, recipient_order, &mut buf);
        // Patch the sequence number in the recipient's byte order.
        let seq = client.last_sequence.load(Ordering::Relaxed);
        let seq_bytes = match recipient_order {
            ClientByteOrder::LittleEndian => seq.to_le_bytes(),
            ClientByteOrder::BigEndian => seq.to_be_bytes(),
        };
        buf[2] = seq_bytes[0];
        buf[3] = seq_bytes[1];
        match client_io::write_or_buffer(client, &buf) {
            Ok(WriteOutcome::Done | WriteOutcome::WouldBlock) => {}
            Ok(WriteOutcome::Disconnect) => disconnected.push(*cid),
            Err(_) => disconnected.push(*cid),
        }
    }
    disconnected
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::Read,
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };

    use crate::{
        resources::ROOT_WINDOW,
        server::{ClientState, ServerState},
    };

    fn make_client(writer: UnixStream, mask_for_root: u32) -> ClientState {
        ClientState {
            writer: Arc::new(Mutex::new(writer)),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0,
            resource_id_mask: 0,
            event_masks: HashMap::from([(ROOT_WINDOW, mask_for_root)]),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            outbound: VecDeque::new(),
            watching_writable: false,
            focused_window: ROOT_WINDOW,
            reader_control: None,
        }
    }

    fn install(state: &mut ServerState, id: u32, mask: u32) -> UnixStream {
        let (a, b) = UnixStream::pair().unwrap();
        let client = make_client(a, mask);
        state.clients.insert(id, client);
        b
    }

    #[test]
    fn subscribers_by_id_filters_by_mask_bit() {
        let mut state = ServerState::new();
        let _peer1 = install(&mut state, 1, 0x40_0000); // PropertyChange
        let _peer2 = install(&mut state, 2, 0x00_0001); // KeyPress only
        let mut got = subscribers_by_id(&state, ROOT_WINDOW, 0x40_0000);
        got.sort_by_key(|c| c.0);
        assert_eq!(got, vec![ClientId(1)]);
    }

    #[test]
    fn fanout_event_to_clients_writes_and_dedups() {
        let mut state = ServerState::new();
        let mut peer1 = install(&mut state, 1, 0xFFFF_FFFF);
        let mut peer2 = install(&mut state, 2, 0xFFFF_FFFF);
        let dropped = fanout_event_to_clients(
            &mut state,
            // Pass id=1 twice — dedup must collapse to a single send.
            &[ClientId(1), ClientId(2), ClientId(1)],
            |buf, _seq, _order| {
                buf.resize(32, 0);
                buf[0] = 0xAB;
            },
        );
        assert!(dropped.is_empty());

        let mut buf1 = [0u8; 64];
        let n1 = peer1.read(&mut buf1).unwrap();
        assert_eq!(n1, 32);
        assert_eq!(buf1[0], 0xAB);

        let mut buf2 = [0u8; 64];
        let n2 = peer2.read(&mut buf2).unwrap();
        assert_eq!(n2, 32);
    }

    #[test]
    fn fanout_raw_event_patches_sequence_per_client() {
        let mut state = ServerState::new();
        let mut peer1 = install(&mut state, 1, 0xFFFF_FFFF);
        let mut peer2 = install(&mut state, 2, 0xFFFF_FFFF);
        // Bump sequences so the two clients have distinct numbers.
        state
            .clients
            .get(&1)
            .unwrap()
            .last_sequence
            .store(0x1234, Ordering::Relaxed);
        state
            .clients
            .get(&2)
            .unwrap()
            .last_sequence
            .store(0x5678, Ordering::Relaxed);

        let template = [0xCDu8; 32];
        let dropped = fanout_raw_event_to_clients(
            &mut state,
            &[ClientId(1), ClientId(2)],
            &template,
            ClientByteOrder::LittleEndian,
        );
        assert!(dropped.is_empty());

        let mut got1 = [0u8; 32];
        peer1.read_exact(&mut got1).unwrap();
        assert_eq!(got1[2], 0x34);
        assert_eq!(got1[3], 0x12);

        let mut got2 = [0u8; 32];
        peer2.read_exact(&mut got2).unwrap();
        assert_eq!(got2[2], 0x78);
        assert_eq!(got2[3], 0x56);
    }

    #[test]
    fn missing_client_id_is_skipped_quietly() {
        let mut state = ServerState::new();
        let _peer = install(&mut state, 1, 0xFFFF_FFFF);
        let dropped = fanout_event_to_clients(
            &mut state,
            &[ClientId(1), ClientId(99)], // 99 doesn't exist
            |buf, _, _| buf.resize(32, 0),
        );
        assert!(dropped.is_empty());
    }

    #[test]
    fn client_target_id_returns_some_only_for_registered() {
        let mut state = ServerState::new();
        let _peer = install(&mut state, 7, 0);
        assert_eq!(client_target_id(&state, ClientId(7)), Some(ClientId(7)));
        assert_eq!(client_target_id(&state, ClientId(99)), None);
    }

    #[test]
    fn emit_xi2_focus_event_to_state_only_writes_clients_with_matching_mask() {
        let mut state = ServerState::new();
        let window = ROOT_WINDOW;
        // Client 1 selects XI2 FocusIn (evtype 9) on (root, deviceid=3).
        let mut peer1 = install(&mut state, 1, 0);
        state
            .clients
            .get_mut(&1)
            .unwrap()
            .xi2_masks
            .insert((window, 3), 1 << 9);
        // Client 2 selects nothing on root.
        let mut peer2 = install(&mut state, 2, 0);

        let dropped = emit_xi2_focus_event_to_state(&mut state, window, 9, 137);
        assert!(dropped.is_empty());

        let mut buf = [0u8; 64];
        let n = peer1.read(&mut buf).unwrap();
        assert!(n >= 32, "client 1 should receive an XI2 focus event");

        peer2
            .set_nonblocking(true)
            .expect("set_nonblocking on peer2");
        let mut other = [0u8; 64];
        match peer2.read(&mut other) {
            Ok(0) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("client 2 unexpectedly received: {other:?}"),
        }
    }

    #[test]
    fn expose_event_fanout_translates_host_xid() {
        let mut state = ServerState::new();
        let mut peer = install(&mut state, 1, 0x0000_8000); // ExposureMask
        let host_xid = 0xdead_beefu32;
        let xid_map: HostXidMap = std::collections::HashMap::from([(host_xid, ROOT_WINDOW)]);
        let dropped = expose_event_fanout_to_state(
            &mut state,
            &xid_map,
            HostExposeEvent {
                host_xid,
                x: 0,
                y: 0,
                width: 10,
                height: 10,
                count: 0,
            },
        );
        assert!(dropped.is_empty());
        let mut buf = [0u8; 32];
        peer.read_exact(&mut buf).unwrap();
        assert_eq!(buf[0], 12); // X11 Expose event opcode
    }

    #[test]
    fn expose_event_fanout_unknown_host_xid_is_quiet() {
        let mut state = ServerState::new();
        let _peer = install(&mut state, 1, 0xFFFF_FFFF);
        let xid_map: HostXidMap = std::collections::HashMap::new();
        let dropped = expose_event_fanout_to_state(
            &mut state,
            &xid_map,
            HostExposeEvent {
                host_xid: 1234,
                x: 0,
                y: 0,
                width: 10,
                height: 10,
                count: 0,
            },
        );
        assert!(dropped.is_empty());
    }

    #[test]
    fn emit_window_event_to_state_only_writes_subscribers() {
        let mut state = ServerState::new();
        let mut peer1 = install(&mut state, 1, 0x40_0000); // PropertyChange
        let mut peer2 = install(&mut state, 2, 0x00_0001); // KeyPress only
        let dropped =
            emit_window_event_to_state(&mut state, ROOT_WINDOW, 0x40_0000, |buf, _seq, _order| {
                buf.resize(32, 0);
                buf[0] = 0x55;
            });
        assert!(dropped.is_empty());

        let mut buf1 = [0u8; 32];
        peer1.read_exact(&mut buf1).unwrap();
        assert_eq!(buf1[0], 0x55);

        peer2
            .set_nonblocking(true)
            .expect("set_nonblocking on peer2");
        let mut buf2 = [0u8; 32];
        match peer2.read(&mut buf2) {
            Ok(0) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("unsubscribed peer2 unexpectedly received: {other:?}"),
        }
    }
}
