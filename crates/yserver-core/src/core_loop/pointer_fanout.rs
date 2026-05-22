//! State-borrowing replacement for `server::pointer_event_fanout`.
//!
//! Mirrors the pre-lift logic from `server.rs`:
//!   * translate root_x/root_y from host-screen coords to ynest-root,
//!   * honour any active or passive pointer grab,
//!   * walk the propagation chain for core device events,
//!   * fan out parallel XI2 device + raw events to clients selecting
//!     XI2 masks on the target / top-level / root.
//!
//! All work happens inside a single `&mut ServerState` borrow scope —
//! per-target writers go through `client_io::write_or_buffer` so the
//! D3 lift can wire the disconnect list back into the core loop.
//!
//! The xid_map is still passed as `Arc<Mutex<HostXidMap>>`. Phase F1
//! demotes it to a plain field on `HostX11Backend` and at that point
//! the helper takes `&HostXidMap`.

use yserver_protocol::x11::{self, ClientId, ResourceId, SequenceNumber};

use crate::{
    core_loop::fanout::{
        client_target_id, fanout_event_to_clients, pointer_propagation_target_by_id,
    },
    host_x11::{HostPointerEvent, HostXidMap, PointerEventKind},
    resources::ROOT_WINDOW,
    server::{ServerState, xi2_mask_for_client},
};

const XI2_MAJOR_OPCODE: u8 = 137;

/// Fan a host pointer event out to nested clients.
///
/// `handle_grabs` toggles passive-grab matching and active-grab
/// redirection. Pass `false` from `AllowEvents ReplayPointer` to avoid
/// re-checking the same passive grab that was just released.
///
/// `is_replay` is set when the call comes from the `AllowEvents
/// ReplayPointer` re-delivery path. XI2 device + raw events were already
/// fanned out on the original libinput-driven invocation (XI2 fanout is
/// independent of core grab state), so the replay must skip them to
/// avoid duplicate XI2 ButtonPress delivery — caja's GTK gesture
/// controllers interpret a back-to-back XI2 press pair as a drag from
/// (0,0), producing a phantom rubber-band selection on a single click
/// over the desktop.
pub fn pointer_event_fanout_to_state(
    state: &mut ServerState,
    xid_map: &HostXidMap,
    event: HostPointerEvent,
    handle_grabs: bool,
    is_replay: bool,
) -> Vec<ClientId> {
    let mut dropped = Vec::new();

    // Step 1 — translate host-screen coords to ynest-root coords.
    let event = translate_host_event(state, xid_map, event);

    if matches!(
        event.kind,
        PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease
    ) {
        log::trace!(
            "pointer_fanout entry: kind={:?} button={} host_xid=0x{:x} root=({},{}) event_xy=({},{})",
            event.kind,
            event.detail,
            event.host_xid,
            event.root_x,
            event.root_y,
            event.event_x,
            event.event_y,
        );
    }

    // Resolve the actual hit window (deepest mapped child under cursor)
    // up front. We need it for both the core-event paths below (passive
    // grab matching, normal propagation) and for the XI2 fanout, which
    // runs unconditionally — XI2 events flow to XI2 selectees regardless
    // of any active core grab (the two grab spaces are independent per
    // X11 spec).
    let root_hit = state.root_pointer_target_at(event.root_x, event.root_y);
    let top_level_id_opt = root_hit
        .map(|(target, _, _)| state.top_level_for_target(target))
        .or_else(|| xid_map.get(&event.host_xid).copied());
    let top_level_id = top_level_id_opt.unwrap_or(ROOT_WINDOW);
    let (target, target_x, target_y) = root_hit.unwrap_or_else(|| {
        xid_map
            .get(&event.host_xid)
            .copied()
            .and_then(|tl| {
                state
                    .pointer_target_at(tl, event.event_x, event.event_y)
                    .or(Some((tl, event.event_x, event.event_y)))
            })
            .unwrap_or((ROOT_WINDOW, event.event_x, event.event_y))
    });

    // ── Core fanout ─────────────────────────────────────────────────
    let mut handled_core_via_grab = false;

    // Step 2 — active-grab redirection (core events only).
    if handle_grabs
        && let Some((grab_window, grab_client, gx, gy, owner_events)) = active_grab_target(state)
    {
        // With `owner_events=true`, pointer events on windows owned
        // by the grab client are reported normally (to the deepest
        // natural window) rather than redirected to `grab_window`.
        // The grab itself is just an exclusivity mechanism — other
        // clients can't see the events. GTK3 menus rely on this:
        // motion fires on the panel button until the user actually
        // crosses into the popup, at which point natural
        // Enter/Leave fire and GTK3 transitions menu state. With
        // `owner_events=false`, all events report against grab_window.
        let target_owner = state.resources.window_owner(target);
        let target_is_owned_by_grab_client = target_owner == Some(grab_client);
        let redirect_to_grab = !owner_events || !target_is_owned_by_grab_client;
        if !matches!(
            event.kind,
            PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify
        ) && redirect_to_grab
        {
            let event_x = clamp_grab_coord(event.root_x, gx);
            let event_y = clamp_grab_coord(event.root_y, gy);
            log::trace!(
                "pointer_fanout: ACTIVE-GRAB redirect kind={:?} button={} grab_window=0x{:x} grab_client={:?} owner_events={}",
                event.kind,
                event.detail,
                grab_window.0,
                grab_client,
                owner_events,
            );
            let extras = fanout_event_to_clients(state, &[grab_client], |buf, seq, order| {
                encode_pointer_event(
                    buf,
                    order,
                    event.kind,
                    seq,
                    event.detail,
                    event.time,
                    grab_window,
                    ResourceId(0), // active-grab redirect: no propagation child
                    event,
                    event_x,
                    event_y,
                );
            });
            merge_dropped(&mut dropped, extras);
            release_passive_grab_on_button_release(state, event.kind);
            handled_core_via_grab = true;
            // Else (owner_events=true and target owned by grab client):
            // fall through to normal propagation so the event fires on
            // the natural window via the usual subscriber-walk path.
        }
        // For Enter/Leave (natural pointer crossings between windows),
        // never mark handled_core_via_grab — let them fall through to
        // the normal core propagation in step 4. Pre-fix the existing
        // code set the flag unconditionally and dropped natural
        // crossings entirely while a grab was active, so GTK3 menus
        // (xfce4-panel's main menu, marco's title-bar popup) never
        // received the "pointer entered me" notification needed to
        // engage hover/click tracking. Matches Xorg
        // `dix/events.c::DeliverGrabbedEvent` which only re-routes
        // pointer events explicitly listed in the grab's event mask.
    }

    // Step 3 — passive button-grab matching for ButtonPress.
    if !handled_core_via_grab
        && handle_grabs
        && event.kind == PointerEventKind::ButtonPress
        && let Some((grab_owner, grab_window, grab_pointer_mode)) =
            try_match_passive_grab(state, xid_map, event)
    {
        log::trace!(
            "pointer_fanout: PASSIVE-GRAB match button={} grab_owner={:?} grab_window=0x{:x} mode={}",
            event.detail,
            grab_owner,
            grab_window.0,
            grab_pointer_mode,
        );
        // Activate the passive grab atomically with the dispatch.
        if let Some(grab_target) = client_target_id(state, grab_owner) {
            if grab_pointer_mode == 0 {
                state.frozen_pointer_event = Some(event);
            }
            state.pointer_grab = Some((grab_owner, grab_window));
            state.pointer_grab_is_passive = true;

            let extras = fanout_event_to_clients(state, &[grab_target], |buf, seq, order| {
                encode_pointer_event(
                    buf,
                    order,
                    PointerEventKind::ButtonPress,
                    seq,
                    event.detail,
                    event.time,
                    grab_window,
                    ResourceId(0), // passive grab activation: no propagation child
                    event,
                    event.event_x,
                    event.event_y,
                );
            });
            merge_dropped(&mut dropped, extras);
        }
        handled_core_via_grab = true;
    }

    // Step 4 — normal core propagation, only when no grab took ownership.
    //
    // For Crossing events we also run when top_level_id is None: the
    // producer (`update_pointer_window`) emits Leave/Enter chain events
    // with host_xid pointing at the KMS root container for the
    // ROOT_WINDOW endpoint, and that host_xid isn't in xid_map.
    let is_crossing = matches!(
        event.kind,
        PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify
    );
    if !handled_core_via_grab && (top_level_id_opt.is_some() || is_crossing) {
        let mask_bit = pointer_mask_bit(event.kind, event.state);
        let (nested_id, event_x, event_y, core_targets, propagation_child) =
            pointer_propagation_target_by_id(state, target, target_x, target_y, mask_bit)
                .unwrap_or((target, target_x, target_y, Vec::new(), ResourceId(0)));

        if matches!(
            event.kind,
            PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease
        ) {
            log::debug!(
                "pointer_fanout: kind={:?} button={} host_xid=0x{:x} top_level=0x{:x} target=0x{:x} \
                 propagation_window=0x{:x} child=0x{:x} core_targets={:?} root=({},{}) event_xy=({},{})",
                event.kind,
                event.detail,
                event.host_xid,
                top_level_id.0,
                target.0,
                nested_id.0,
                propagation_child.0,
                core_targets.iter().map(|c| c.0).collect::<Vec<_>>(),
                event.root_x,
                event.root_y,
                event_x,
                event_y,
            );
        }

        let extras = fanout_event_to_clients(state, &core_targets, |buf, seq, order| {
            encode_pointer_event(
                buf,
                order,
                event.kind,
                seq,
                event.detail,
                event.time,
                nested_id,
                propagation_child,
                event,
                event_x,
                event_y,
            );
        });
        merge_dropped(&mut dropped, extras);
    }

    // ── XI2 fanout (always runs, independent of any core grab) ──────
    //
    // Skip on replay: XI2 was already fanned out on the original
    // libinput-driven invocation. Re-running here would deliver a second
    // XI2 ButtonPress for the same physical click and confuse GTK gesture
    // controllers (see is_replay rationale on the public fn).
    if is_replay {
        return dropped;
    }
    let Some(top_level_id) = top_level_id_opt else {
        log::debug!(
            "pointer_fanout: kind={:?} host_xid=0x{:x} not in xid_map — XI2 fanout skipped",
            event.kind,
            event.host_xid,
        );
        return dropped;
    };
    let xi2_evtype = xi2_evtype(event.kind);
    let xi2_raw_evtype = xi2_raw_evtype(event.kind);

    // For XI2 the "event window" is the hit target — XI2 doesn't
    // propagate up through the event mask the way core does. Use the
    // original (untranslated) coords relative to the hit target.
    let (event_x, event_y) = (target_x, target_y);
    let nested_id = target;
    let (xi2_targets, xi2_raw_targets) =
        compute_xi2_targets(state, target, top_level_id, xi2_evtype, xi2_raw_evtype);

    if matches!(
        event.kind,
        PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease
    ) {
        log::debug!(
            "pointer_fanout XI2: kind={:?} button={} time={} target=0x{:x} top_level=0x{:x} \
             xi2_targets={:?} xi2_raw_targets={:?} root=({},{}) event_xy=({},{}) state=0x{:x}",
            event.kind,
            event.detail,
            event.time,
            target.0,
            top_level_id.0,
            xi2_targets.iter().map(|c| c.0).collect::<Vec<_>>(),
            xi2_raw_targets.iter().map(|c| c.0).collect::<Vec<_>>(),
            event.root_x,
            event.root_y,
            event_x,
            event_y,
            event.state,
        );
    }

    // XI2 raw events.
    if let Some(raw_evtype) = xi2_raw_evtype {
        let extras = fanout_event_to_clients(state, &xi2_raw_targets, |buf, seq, order| {
            x11::encode_xi2_raw_event(
                buf,
                order,
                seq,
                XI2_MAJOR_OPCODE,
                raw_evtype,
                2, // deviceid: Master Pointer
                event.time,
                u32::from(event.detail),
                2, // sourceid: Master Pointer
                i32::from(event.root_x),
                i32::from(event.root_y),
            );
        });
        merge_dropped(&mut dropped, extras);
    }

    // If this is a wheel button press (4 = up, 5 = down, 6 = left,
    // 7 = right), prepend an XI_Motion event carrying the per-
    // event scroll delta before the XI_ButtonPress. The Motion's
    // axis value is the DELTA (±1 per click), NOT the cumulative
    // counter — XIScrollClass is declared Relative (mode=0), so
    // per the XI2 spec the axis value in each Motion event is the
    // change since the previous event. GDK divides by increment
    // (1.0) and fires a scroll event.
    //
    // The earlier cumulative-counter encoding was a Relative-mode
    // mismatch: GDK interpreted the cumulative value AS the delta,
    // which fires N scroll events on the Nth wheel click and
    // confuses clients that joined mid-session. State tracking on
    // ServerState (scroll_axis_value) is kept for telemetry / for
    // any future Absolute-mode switch but isn't used for emission.
    //
    // ButtonRelease doesn't carry a delta; only ButtonPress does.
    // The legacy XI_ButtonPress(4/5) event still goes out below
    // for core-X11 clients and for non-XI2 fallback. XIScrollClass
    // has NoEmulation flag set, so GDK ignores those for scroll.
    let scroll_axis_info: Option<(u8, i32)> = if event.kind == PointerEventKind::ButtonPress
        && (event.detail >= 4 && event.detail <= 7)
    {
        let (axis_idx, delta): (usize, i32) = match event.detail {
            4 => (0, -1),
            5 => (0, 1),
            6 => (1, -1),
            7 => (1, 1),
            _ => unreachable!(),
        };
        state.scroll_axis_value[axis_idx] = state.scroll_axis_value[axis_idx].wrapping_add(delta);
        let scroll_axis_num: u8 = if axis_idx == 0 { 2 } else { 3 };
        Some((scroll_axis_num, delta))
    } else {
        None
    };

    // XI2 device events (crossing or non-crossing).
    let extras = fanout_event_to_clients(state, &xi2_targets, |buf, seq, order| {
        if matches!(
            event.kind,
            PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify
        ) {
            x11::encode_xi2_crossing_event(
                buf,
                order,
                seq,
                XI2_MAJOR_OPCODE,
                xi2_evtype,
                2,
                event.time,
                ROOT_WINDOW,
                nested_id,
                event.root_x,
                event.root_y,
                event_x,
                event_y,
                event.state,
                0,
                0,
                2,
            );
        } else {
            if let Some((axis, value)) = scroll_axis_info {
                x11::encode_xi2_motion_with_scroll(
                    buf,
                    order,
                    seq,
                    XI2_MAJOR_OPCODE,
                    2,
                    event.time,
                    ROOT_WINDOW,
                    nested_id,
                    event.root_x,
                    event.root_y,
                    event_x,
                    event_y,
                    event.state,
                    2,
                    axis,
                    value,
                );
            }
            x11::encode_xi2_device_event(
                buf,
                order,
                seq,
                XI2_MAJOR_OPCODE,
                xi2_evtype,
                2,
                event.time,
                ROOT_WINDOW,
                nested_id,
                ResourceId(0), // XI2 doesn't propagate; event=hit-target, so child=None
                event.root_x,
                event.root_y,
                event_x,
                event_y,
                event.state,
                u32::from(event.detail),
                2,
            );
        }
    });
    merge_dropped(&mut dropped, extras);

    dropped
}

fn translate_host_event(
    state: &ServerState,
    xid_map: &HostXidMap,
    event: HostPointerEvent,
) -> HostPointerEvent {
    let Some(top_level_id) = xid_map.get(&event.host_xid).copied() else {
        return event;
    };
    let Some((rx, ry)) = state
        .resources
        .window(top_level_id)
        .map(|w| (w.x + event.event_x, w.y + event.event_y))
    else {
        return event;
    };
    HostPointerEvent {
        root_x: rx,
        root_y: ry,
        ..event
    }
}

fn active_grab_target(
    state: &ServerState,
) -> Option<(yserver_protocol::x11::ResourceId, ClientId, i32, i32, bool)> {
    let (client_id, grab_window) = state.pointer_grab?;
    let target = client_target_id(state, client_id)?;
    let (gx, gy) = state.resources.window_absolute_position(grab_window);
    // `owner_events` from the active grab record. Passive button-grabs
    // (activated via try_match_passive_grab) leave
    // `state.active_pointer_grab` unset, so default to false (X11
    // implicit grab semantics — events report against the grab window).
    let owner_events = state
        .active_pointer_grab
        .filter(|g| g.owner == client_id)
        .is_some_and(|g| g.owner_events);
    Some((grab_window, target, gx, gy, owner_events))
}

fn release_passive_grab_on_button_release(state: &mut ServerState, kind: PointerEventKind) {
    if kind == PointerEventKind::ButtonRelease && state.pointer_grab_is_passive {
        state.pointer_grab = None;
        state.pointer_grab_is_passive = false;
        state.frozen_pointer_event = None;
    }
}

fn clamp_grab_coord(root_coord: i16, grab_origin: i32) -> i16 {
    i32::from(root_coord)
        .saturating_sub(grab_origin)
        .clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

fn try_match_passive_grab(
    state: &ServerState,
    xid_map: &HostXidMap,
    event: HostPointerEvent,
) -> Option<(ClientId, yserver_protocol::x11::ResourceId, u8)> {
    let (hit_window, _, _) = state
        .root_pointer_target_at(event.root_x, event.root_y)
        .or_else(|| {
            let top_level_id = xid_map.get(&event.host_xid).copied()?;
            state
                .pointer_target_at(top_level_id, event.event_x, event.event_y)
                .or(Some((top_level_id, event.event_x, event.event_y)))
        })?;
    let grab = state.find_passive_grab(hit_window, event.detail, event.state)?;
    Some((grab.owner, grab.grab_window, grab.pointer_mode))
}

fn pointer_mask_bit(kind: PointerEventKind, state_mask: u16) -> u32 {
    match kind {
        PointerEventKind::ButtonPress => 0x0000_0004,
        PointerEventKind::ButtonRelease => 0x0000_0008,
        PointerEventKind::MotionNotify => {
            let mut bits: u32 = 0x0000_0040;
            let buttons_held = (state_mask >> 8) & 0x1f;
            if buttons_held != 0 {
                bits |= 0x0000_2000;
                for n in 0..5 {
                    if buttons_held & (1 << n) != 0 {
                        bits |= 0x0000_0100 << n;
                    }
                }
            }
            bits
        }
        PointerEventKind::EnterNotify => 0x0000_0010,
        PointerEventKind::LeaveNotify => 0x0000_0020,
    }
}

fn xi2_evtype(kind: PointerEventKind) -> u16 {
    match kind {
        PointerEventKind::ButtonPress => 4,
        PointerEventKind::ButtonRelease => 5,
        PointerEventKind::MotionNotify => 6,
        PointerEventKind::EnterNotify => 7,
        PointerEventKind::LeaveNotify => 8,
    }
}

fn xi2_raw_evtype(kind: PointerEventKind) -> Option<u16> {
    match kind {
        PointerEventKind::ButtonPress => Some(15),
        PointerEventKind::ButtonRelease => Some(16),
        PointerEventKind::MotionNotify => Some(17),
        PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify => None,
    }
}

fn compute_xi2_targets(
    state: &ServerState,
    target: yserver_protocol::x11::ResourceId,
    top_level_id: yserver_protocol::x11::ResourceId,
    xi2_evtype: u16,
    xi2_raw_evtype: Option<u16>,
) -> (Vec<ClientId>, Vec<ClientId>) {
    let mut xi2_targets: Vec<ClientId> = Vec::new();
    let mut xi2_raw_targets: Vec<ClientId> = Vec::new();
    if xi2_evtype == 0 {
        return (xi2_targets, xi2_raw_targets);
    }
    for (cid_u32, c) in state.clients.iter() {
        let cid = ClientId(*cid_u32);
        let mask = xi2_mask_for_client(c, target, top_level_id, &[2, 1, 0]);
        if mask & (1 << xi2_evtype) != 0 {
            xi2_targets.push(cid);
        }
        if let Some(raw_evtype) = xi2_raw_evtype {
            if mask & (1 << raw_evtype) != 0 {
                xi2_raw_targets.push(cid);
            }
            let root_mask = xi2_mask_for_client(c, ROOT_WINDOW, ROOT_WINDOW, &[1, 0, 2]);
            if root_mask & (1 << raw_evtype) != 0 && !xi2_raw_targets.contains(&cid) {
                xi2_raw_targets.push(cid);
            }
        }
    }
    (xi2_targets, xi2_raw_targets)
}

#[allow(clippy::too_many_arguments)]
fn encode_pointer_event(
    buf: &mut Vec<u8>,
    order: yserver_protocol::x11::ClientByteOrder,
    kind: PointerEventKind,
    seq: SequenceNumber,
    detail: u8,
    time: u32,
    target_window: yserver_protocol::x11::ResourceId,
    child: yserver_protocol::x11::ResourceId,
    event: HostPointerEvent,
    event_x: i16,
    event_y: i16,
) {
    let pointer = x11::PointerEvent {
        sequence: seq,
        detail,
        time,
        root: ROOT_WINDOW,
        event: target_window,
        child,
        root_x: event.root_x,
        root_y: event.root_y,
        event_x,
        event_y,
        state: event.state,
    };
    match kind {
        PointerEventKind::ButtonPress => x11::encode_button_press_event(buf, order, pointer),
        PointerEventKind::ButtonRelease => x11::encode_button_release_event(buf, order, pointer),
        PointerEventKind::MotionNotify => x11::encode_motion_notify_event(
            buf,
            order,
            x11::PointerEvent {
                detail: 0,
                ..pointer
            },
        ),
        // For Crossing events, `child` and `detail` come from the
        // producer (HostPointerEvent), which has the spec-correct
        // values computed by `crossings::normal_mode_crossings` /
        // `implicit_grab_crossings`. The fanout-walk's
        // `propagation_child` is the right value for Button/Motion
        // (where it identifies the immediate descendant of the
        // propagation target on the path to the source), but NOT for
        // crossings — crossing `child` per X11 spec is per-event in
        // the chain (None on endpoints, the next inferior on virtual
        // intermediates) and the propagation walk can't know which is
        // which.
        PointerEventKind::EnterNotify => x11::encode_enter_notify_event(
            buf,
            order,
            x11::CrossingEvent {
                sequence: seq,
                time,
                root: ROOT_WINDOW,
                event: target_window,
                child: yserver_protocol::x11::ResourceId(event.child),
                root_x: event.root_x,
                root_y: event.root_y,
                event_x,
                event_y,
                state: event.state,
                detail: event.detail,
                mode: event.crossing_mode,
            },
        ),
        PointerEventKind::LeaveNotify => x11::encode_leave_notify_event(
            buf,
            order,
            x11::CrossingEvent {
                sequence: seq,
                time,
                root: ROOT_WINDOW,
                event: target_window,
                child: yserver_protocol::x11::ResourceId(event.child),
                root_x: event.root_x,
                root_y: event.root_y,
                event_x,
                event_y,
                state: event.state,
                detail: event.detail,
                mode: event.crossing_mode,
            },
        ),
    }
}

fn merge_dropped(into: &mut Vec<ClientId>, more: Vec<ClientId>) {
    for cid in more {
        if !into.contains(&cid) {
            into.push(cid);
        }
    }
}
