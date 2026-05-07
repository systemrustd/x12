//! Per-client disconnect cleanup, lifted out of `nested::handle_client`'s
//! closing block. Tears down every piece of state that referenced the
//! departing client (resources owned by it, per-client event masks,
//! grabs, selections, MIT-SHM segments, …) plus their host counterparts
//! (subwindows, fonts, pixmaps, RENDER pictures + glyphsets).
//!
//! Invoked from `run_core` on `Message::ClientDisconnected`, and also
//! when `process_request` reports `RequestOutcome::Disconnect` for a
//! peer that overflowed its outbound buffer.

use yserver_protocol::x11::{ClientId, ResourceId};

use crate::{
    backend::Backend,
    core_loop::fanout::{fanout_event_to_clients, subscribers_by_id},
    resources::{MapState, ROOT_WINDOW},
    server::ServerState,
};

/// One window's identity captured before the resource table forgets
/// it, so the post-mutation UnmapNotify+DestroyNotify fanout has
/// stable subscriber lists.
struct PendingDestroy {
    window: ResourceId,
    parent: ResourceId,
    was_mapped: bool,
    host_xid: Option<crate::backend::WindowHandle>,
    on_window: Vec<ClientId>,
    on_parent: Vec<ClientId>,
}

fn collect_destroy_order(
    table: &crate::resources::ResourceTable,
    root: ResourceId,
    out: &mut Vec<ResourceId>,
) {
    let Some(w) = table.window(root) else {
        return;
    };
    for child in w.children.clone() {
        collect_destroy_order(table, child, out);
    }
    out.push(root);
}

fn fanout_destroy_sequence(state: &mut ServerState, pending: &PendingDestroy) {
    let window = pending.window;
    let parent = pending.parent;
    if pending.was_mapped {
        let _dropped = fanout_event_to_clients(state, &pending.on_window, |buf, seq, order| {
            yserver_protocol::x11::encode_unmap_notify_event(
                buf, seq, order, window, window, false,
            );
        });
        let _dropped = fanout_event_to_clients(state, &pending.on_parent, |buf, seq, order| {
            yserver_protocol::x11::encode_unmap_notify_event(
                buf, seq, order, parent, window, false,
            );
        });
    }
    let _dropped = fanout_event_to_clients(state, &pending.on_window, |buf, seq, order| {
        yserver_protocol::x11::encode_destroy_notify_event(buf, seq, order, window, window);
    });
    let _dropped = fanout_event_to_clients(state, &pending.on_parent, |buf, seq, order| {
        yserver_protocol::x11::encode_destroy_notify_event(buf, seq, order, parent, window);
    });
}

/// Drop every server-side resource owned by `client_id` and free the
/// corresponding host objects.
pub fn process_disconnect(state: &mut ServerState, backend: &mut dyn Backend, client_id: ClientId) {
    // Idempotent: a client can be disconnected twice in quick succession
    // (write-side EPIPE from process_request races the reader thread's
    // EOF → Message::ClientDisconnected). The first call removes the
    // entry from state.clients; the second sees None and bails.
    if !state.clients.contains_key(&client_id.0) {
        return;
    }
    log::debug!("process_disconnect: client {}", client_id.0);
    // Send the reader thread (if any) a Shutdown so it exits cleanly
    // before we drop the client entry.
    if let Some(client) = state.clients.get(&client_id.0)
        && let Some(ctrl) = &client.reader_control
    {
        let _ = ctrl.send(crate::server::ReaderControl::Shutdown);
    }

    let mut owned_roots: Vec<ResourceId> = Vec::new();
    state
        .resources
        .collect_owned_window_roots(client_id, &mut owned_roots);

    let mut pending: Vec<PendingDestroy> = Vec::new();
    let mut all_destroyed: Vec<ResourceId> = Vec::new();
    for root in owned_roots {
        let mut order: Vec<ResourceId> = Vec::new();
        collect_destroy_order(&state.resources, root, &mut order);
        for w in &order {
            let (parent, was_mapped, host_xid) =
                state
                    .resources
                    .window(*w)
                    .map_or((ROOT_WINDOW, false, None), |win| {
                        (
                            win.parent,
                            win.map_state != MapState::Unmapped,
                            win.host_xid,
                        )
                    });
            let on_window = subscribers_by_id(state, *w, 0x0002_0000);
            let on_parent = subscribers_by_id(state, parent, 0x0008_0000);
            pending.push(PendingDestroy {
                window: *w,
                parent,
                was_mapped,
                host_xid,
                on_window,
                on_parent,
            });
        }
        let _ = state.resources.destroy_window(root);
        all_destroyed.extend(order);
    }
    state.drop_window_subscriptions(&all_destroyed);

    let removed = state
        .resources
        .remove_non_window_resources_owned_by(client_id);
    state.clients.remove(&client_id.0);

    let dead_windows: std::collections::HashSet<ResourceId> =
        all_destroyed.iter().copied().collect();
    state
        .xfixes_regions
        .retain(|_, region| region.owner != client_id);
    state
        .xfixes_selection_masks
        .retain(|(owner, _, _), _| *owner != client_id.0);
    state
        .xfixes_cursor_masks
        .retain(|(owner, _), _| *owner != client_id.0);
    state
        .shape_windows
        .retain(|window, _| !dead_windows.contains(window));
    state
        .shape_select_masks
        .retain(|(owner, window), _| *owner != client_id.0 && !dead_windows.contains(window));
    state
        .sync_counters
        .retain(|_, counter| counter.owner != client_id);
    state
        .sync_alarms
        .retain(|_, alarm| alarm.owner != client_id);
    state
        .damage_objects
        .retain(|_, damage| damage.owner != client_id && !dead_windows.contains(&damage.drawable));
    state
        .composite_redirects
        .retain(|(window, _), _| !dead_windows.contains(window));
    state.present_event_selections.retain(|_, selection| {
        selection.owner != client_id && !dead_windows.contains(&selection.window)
    });
    state
        .present_msc
        .retain(|window, _| !dead_windows.contains(window));
    state
        .mit_shm_segments
        .retain(|_, seg| seg.owner != client_id);
    state
        .randr_select_masks
        .retain(|(owner, window), _| *owner != client_id.0 && !dead_windows.contains(window));
    state
        .xkb_select_event_masks
        .retain(|(owner, _), _| *owner != client_id.0);
    state.button_grabs.retain(|g| g.owner != client_id);
    if state
        .pointer_grab
        .is_some_and(|(owner, _)| owner == client_id)
    {
        state.pointer_grab = None;
        state.pointer_grab_is_passive = false;
        state.frozen_pointer_event = None;
    }
    state
        .selections
        .retain(|_, owner_window| !dead_windows.contains(owner_window));

    // Host-side teardown. Order matches `nested::handle_client`'s tail
    // so behavior is bit-identical.
    for entry in pending {
        if let Some(xid) = entry.host_xid {
            let _ = backend.destroy_subwindow(None, xid.as_raw());
            backend.unregister_host_window(xid.as_raw());
        }
        fanout_destroy_sequence(state, &entry);
    }
    for xid in removed.closed_fonts {
        let _ = backend.close_font(None, xid);
    }
    for xid in removed.freed_pixmaps {
        let _ = backend.free_pixmap(None, xid);
    }
    for (pic_xid, owned_pix) in removed.freed_pictures {
        let _ = backend.render_free_picture(None, pic_xid);
        if let Some(pix_xid) = owned_pix {
            let _ = backend.free_pixmap(None, pix_xid);
        }
    }
    for gs_xid in removed.freed_glyphsets {
        let _ = backend.render_free_glyphset(None, gs_xid);
    }
    for cursor_xid in removed.freed_cursors {
        let _ = backend.free_cursor(None, cursor_xid);
    }
}
