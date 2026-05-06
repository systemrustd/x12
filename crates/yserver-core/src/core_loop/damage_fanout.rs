//! State-borrowing replacement for `nested::accumulate_damage`.
//!
//! Walks `state.damage_objects` for damage tied to `drawable`, ORs the
//! incoming rect into each, and (for objects that haven't yet fired
//! this cycle) emits a `DamageNotify` event to the owning client. The
//! old version snapshotted writer Arcs and ran the writer pass after
//! dropping the server lock; the lifted version interleaves the state
//! mutation (pending_notify_fired toggle, rect push) with target
//! collection in a single `&mut ServerState` borrow scope, then
//! encodes + writes via `client_io::write_or_buffer`.

use yserver_protocol::x11::{ClientId, ResourceId, SequenceNumber, damage as x11damage, xfixes};

use crate::{core_loop::fanout::fanout_event_to_clients, server::ServerState};

const DAMAGE_FIRST_EVENT: u8 = 94;

/// One DamageNotify worth of identity that has already been
/// "committed" against `state.damage_objects` (rect pushed, fired
/// flag set). The caller drains this list to actually transmit.
#[derive(Debug, Clone)]
struct PendingNotify {
    owner: ClientId,
    damage_id: u32,
    level: u8,
    drawable: u32,
    geometry: x11damage::Rectangle,
}

/// Convenience: accumulate damage over the full extent of `drawable`
/// (its width × height). Mirrors `nested::accumulate_damage_full`.
pub fn accumulate_damage_full_to_state(
    state: &mut ServerState,
    drawable: ResourceId,
) -> Vec<ClientId> {
    let r = drawable_full_rect(state, drawable);
    accumulate_damage_to_state(state, drawable, 0, 0, r.width, r.height)
}

pub fn accumulate_damage_to_state(
    state: &mut ServerState,
    drawable: ResourceId,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
) -> Vec<ClientId> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let timestamp = state.timestamp_now();
    let geom_full = drawable_full_rect(state, drawable);
    let geom_rect = x11damage::Rectangle {
        x: 0,
        y: 0,
        width: geom_full.width,
        height: geom_full.height,
    };

    let damage_ids: Vec<u32> = state
        .damage_objects
        .iter()
        .filter(|(_, dmg)| dmg.drawable == drawable)
        .map(|(id, _)| *id)
        .collect();

    let mut pending: Vec<PendingNotify> = Vec::new();
    let rect = xfixes::RegionRect {
        x,
        y,
        width,
        height,
    };
    for damage_id in damage_ids {
        let (level, fired, owner) = {
            let dmg = state
                .damage_objects
                .get(&damage_id)
                .expect("just enumerated");
            (dmg.level, dmg.pending_notify_fired, dmg.owner)
        };
        if let Some(d) = state.damage_objects.get_mut(&damage_id) {
            d.rects.push(rect);
        }
        if !fired && state.clients.contains_key(&owner.0) {
            pending.push(PendingNotify {
                owner,
                damage_id,
                level,
                drawable: drawable.0,
                geometry: geom_rect,
            });
            if let Some(d) = state.damage_objects.get_mut(&damage_id) {
                d.pending_notify_fired = true;
            }
        }
    }

    if pending.is_empty() {
        return Vec::new();
    }

    let mut dropped: Vec<ClientId> = Vec::new();
    let area = x11damage::Rectangle {
        x,
        y,
        width,
        height,
    };
    for n in pending {
        let extras = fanout_event_to_clients(state, &[n.owner], |buf, seq, order| {
            encode_damage_notify(buf, order, seq, &n, timestamp, area);
        });
        for cid in extras {
            if !dropped.contains(&cid) {
                dropped.push(cid);
            }
        }
    }
    dropped
}

fn encode_damage_notify(
    buf: &mut Vec<u8>,
    byte_order: yserver_protocol::x11::ClientByteOrder,
    seq: SequenceNumber,
    n: &PendingNotify,
    timestamp: u32,
    area: x11damage::Rectangle,
) {
    let evt = x11damage::encode_damage_notify_event(
        byte_order,
        DAMAGE_FIRST_EVENT,
        seq,
        n.level,
        n.drawable,
        n.damage_id,
        timestamp,
        area,
        n.geometry,
    );
    buf.extend_from_slice(&evt);
}

fn drawable_full_rect(state: &ServerState, drawable: ResourceId) -> xfixes::RegionRect {
    if let Some(window) = state.resources.window(drawable) {
        return xfixes::RegionRect {
            x: 0,
            y: 0,
            width: window.width,
            height: window.height,
        };
    }
    state.resources.pixmap(drawable).map_or(
        xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        },
        |pixmap| xfixes::RegionRect {
            x: 0,
            y: 0,
            width: pixmap.width,
            height: pixmap.height,
        },
    )
}
