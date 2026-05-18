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
    /// Damaged area in this level's coordinate space (already
    /// translated + clipped through the ancestor walk).
    area: x11damage::Rectangle,
    /// Full extent of the level's drawable (in its own coord space).
    /// For ancestor matches this is the ancestor's extent, not the
    /// originating leaf's — per X11 DAMAGE spec.
    geometry: x11damage::Rectangle,
}

/// Maximum tree depth to walk when propagating damage to ancestors.
/// Real-world toolkits keep widget trees to a handful of levels;
/// the cap is purely a safety net against cycle / corruption in the
/// `Window.parent` chain, not a meaningful product limit.
const ANCESTOR_WALK_MAX_DEPTH: u8 = 32;

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
    let mut pending: Vec<PendingNotify> = Vec::new();

    // Initial rect in the leaf drawable's coordinate space. Carry as
    // i32 through the ancestor walk so translation + clip don't
    // overflow when a child sits well past its parent's edge;
    // saturate-cast back to i16/u16 only at emit time.
    let mut rx = i32::from(x);
    let mut ry = i32::from(y);
    let mut rw = i32::from(width);
    let mut rh = i32::from(height);

    // Process the leaf level — pixmaps and windows alike. Pixmaps
    // have no parent so processing stops here; windows fall through
    // to the ancestor walk below.
    accumulate_at_level(state, drawable, rx, ry, rw, rh, &mut pending);

    // Ancestor walk — only meaningful for window drawables. Pixmaps
    // exist outside the window tree per X11 spec (Composite spec's
    // NameWindowPixmap aliases sit beside the window tree, not in
    // it), so a `state.resources.window(drawable)` miss at the top
    // of the loop terminates the walk before the first step.
    let mut current = drawable;
    for _ in 0..ANCESTOR_WALK_MAX_DEPTH {
        // Look up the current window's own offset + parent. A
        // missing window is the loop termination for pixmaps and
        // for any path that strays off the tree.
        let Some(current_win) = state.resources.window(current) else {
            break;
        };
        let cur_off_x = i32::from(current_win.x);
        let cur_off_y = i32::from(current_win.y);
        let parent = current_win.parent;
        // Root self-parents (`parent == self`, see resources.rs
        // ROOT_WINDOW initialiser). Other walks in the tree use
        // the same sentinel for termination — propagating beyond
        // root makes no sense.
        if parent == current {
            break;
        }
        // Translate the rect from `current`'s coord space into
        // `parent`'s by adding `current`'s own (x, y).
        rx += cur_off_x;
        ry += cur_off_y;
        // Clip the translated rect to `parent`'s extent. If the
        // rect doesn't intersect this parent at all, it certainly
        // won't intersect any higher ancestor either — terminate.
        let Some(parent_win) = state.resources.window(parent) else {
            // Dangling parent xid (resources tree should keep this
            // consistent; stay defensive — stop walking).
            break;
        };
        let parent_w = i32::from(parent_win.width);
        let parent_h = i32::from(parent_win.height);
        let x0 = rx.max(0);
        let y0 = ry.max(0);
        let x1 = (rx + rw).min(parent_w);
        let y1 = (ry + rh).min(parent_h);
        if x1 <= x0 || y1 <= y0 {
            break;
        }
        // Re-anchor the rect to its visible portion within
        // `parent`. The clipped rect (in parent's coord space) is
        // what higher ancestors will see, exactly mirroring the
        // X11 spec rule "damage propagates the visible region
        // through ancestors."
        rx = x0;
        ry = y0;
        rw = x1 - x0;
        rh = y1 - y0;
        current = parent;
        accumulate_at_level(state, parent, rx, ry, rw, rh, &mut pending);
    }

    if pending.is_empty() {
        return Vec::new();
    }

    let mut dropped: Vec<ClientId> = Vec::new();
    for n in pending {
        let geometry = n.geometry;
        let area = n.area;
        let extras = fanout_event_to_clients(state, &[n.owner], |buf, seq, order| {
            encode_damage_notify(buf, order, seq, &n, timestamp, area, geometry);
        });
        for cid in extras {
            if !dropped.contains(&cid) {
                dropped.push(cid);
            }
        }
    }
    dropped
}

/// Per-level helper: match damage objects keyed on `level_drawable`,
/// push the (clipped, i32 → i16/u16-saturated) rect into each, and
/// add a fire-once `PendingNotify` for any object that hasn't fired
/// in this cycle yet. Called once per level by the ancestor walk
/// (leaf + each ancestor up to root).
///
/// `rx/ry/rw/rh` are in `level_drawable`'s coordinate space and
/// already clipped to its extent by the caller. They arrive as i32
/// for safe walk-time arithmetic; the saturate-cast here is the
/// single conversion point.
fn accumulate_at_level(
    state: &mut ServerState,
    level_drawable: ResourceId,
    rx: i32,
    ry: i32,
    rw: i32,
    rh: i32,
    pending: &mut Vec<PendingNotify>,
) {
    let damage_ids: Vec<u32> = state
        .damage_objects
        .iter()
        .filter(|(_, dmg)| dmg.drawable == level_drawable)
        .map(|(id, _)| *id)
        .collect();

    if damage_ids.is_empty() {
        return;
    }

    // Saturate-cast i32 walk coords to the wire i16/u16 shape.
    // Real window dimensions stay well under i16::MAX (32767); the
    // saturate is defensive against runaway parent chains or
    // corrupt tree state.
    let rect_i16 = xfixes::RegionRect {
        x: i16::try_from(rx).unwrap_or(i16::MAX),
        y: i16::try_from(ry).unwrap_or(i16::MAX),
        width: u16::try_from(rw).unwrap_or(u16::MAX),
        height: u16::try_from(rh).unwrap_or(u16::MAX),
    };
    let area = x11damage::Rectangle {
        x: rect_i16.x,
        y: rect_i16.y,
        width: rect_i16.width,
        height: rect_i16.height,
    };

    // Per-level geometry — the bounding box reported alongside the
    // damage rect. For ancestor matches this is the ANCESTOR's
    // extent (in its own coord space), matching the X11 spec
    // requirement that DamageNotify.geometry describes the damaged
    // drawable's full extent, not the originating leaf's.
    let geom_full = drawable_full_rect(state, level_drawable);
    let geom_rect = x11damage::Rectangle {
        x: 0,
        y: 0,
        width: geom_full.width,
        height: geom_full.height,
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
            d.rects.push(rect_i16);
        }
        if !fired && state.clients.contains_key(&owner.0) {
            pending.push(PendingNotify {
                owner,
                damage_id,
                level,
                drawable: level_drawable.0,
                geometry: geom_rect,
                area,
            });
            if let Some(d) = state.damage_objects.get_mut(&damage_id) {
                d.pending_notify_fired = true;
            }
        }
    }
}

fn encode_damage_notify(
    buf: &mut Vec<u8>,
    byte_order: yserver_protocol::x11::ClientByteOrder,
    seq: SequenceNumber,
    n: &PendingNotify,
    timestamp: u32,
    area: x11damage::Rectangle,
    geometry: x11damage::Rectangle,
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
        geometry,
    );
    buf.extend_from_slice(&evt);
}

#[cfg(test)]
mod tests {
    //! Damage fanout — ancestor-walk + clipping coverage.
    //!
    //! Regression context: marco-with-compositing on v2 shows
    //! "windows render shadow only / hover flicker" because paint
    //! into a CHILD of a redirected top-level fires damage on the
    //! child's xid alone, never on the top-level marco subscribed
    //! to. X11 DAMAGE spec says ancestor windows receive damage
    //! from descendant paint (translated to ancestor coords +
    //! clipped to ancestor extent). These tests pin the fix so the
    //! regression can't return silently.
    use super::*;
    use crate::{
        resources::ROOT_WINDOW,
        server::{ClientState, DamageObject, ServerState},
    };
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        os::unix::net::UnixStream,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU16, Ordering},
        },
    };
    use yserver_protocol::x11::{ClientByteOrder, CreatePixmapRequest, CreateWindowRequest};

    fn make_test_writer() -> Arc<Mutex<UnixStream>> {
        // Pair of sockets; we keep the read end alive in the same
        // Arc so writes don't EPIPE during tests. We never inspect
        // the data sent — assertions are against `damage_objects`.
        let (a, _b) = UnixStream::pair().expect("UnixStream::pair");
        Arc::new(Mutex::new(a))
    }

    fn add_client(state: &mut ServerState, client_id: u32, base: u32) {
        state.clients.insert(
            client_id,
            ClientState {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: base,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                outbound: VecDeque::new(),
                watching_writable: false,
                focused_window: ROOT_WINDOW,
                reader_control: None,
            },
        );
    }

    fn add_window(
        state: &mut ServerState,
        owner_client: u32,
        wid: u32,
        parent: ResourceId,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
    ) -> ResourceId {
        let id = ResourceId(wid);
        state.resources.create_window(
            ClientId(owner_client),
            CreateWindowRequest {
                depth: 24,
                window: id,
                parent,
                x,
                y,
                width: w,
                height: h,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(id);
        id
    }

    fn add_damage_on(state: &mut ServerState, owner: u32, damage_id: u32, drawable: ResourceId) {
        state.damage_objects.insert(
            damage_id,
            DamageObject {
                owner: ClientId(owner),
                drawable,
                level: 3, // NonEmpty — same level marco uses
                rects: Vec::new(),
                pending_notify_fired: false,
            },
        );
    }

    /// Load-bearing test: paint into a child window fires damage on
    /// the child AND on every ancestor up to root, with rectangles
    /// translated through each ancestor's (x, y) offset. Mirrors
    /// the marco-with-compositing case where a redirected top-level
    /// has a DamageCreate on the top-level and a GTK widget child
    /// paints into a descendant.
    #[test]
    fn paint_into_child_fires_damage_on_ancestor() {
        let mut state = ServerState::new();
        add_client(&mut state, 1, 0x0010_0000);
        // Parent W at (100, 200) 500×400, child C at (10, 20) 50×60
        // inside W. Damage subscriptions on BOTH C and W so we can
        // verify the helper hits both levels (the bug case is the
        // ancestor; the child case is the v1 baseline).
        let w_id = add_window(&mut state, 1, 0x0010_0001, ROOT_WINDOW, 100, 200, 500, 400);
        let c_id = add_window(&mut state, 1, 0x0010_0002, w_id, 10, 20, 50, 60);
        add_damage_on(&mut state, 1, 0xe000_0001, c_id);
        add_damage_on(&mut state, 1, 0xe000_0002, w_id);

        // Paint at (5, 5) 30×40 into C.
        let _ = accumulate_damage_to_state(&mut state, c_id, 5, 5, 30, 40);

        // Child level: rect arrives in C's coords (untranslated).
        let c_dmg = state
            .damage_objects
            .get(&0xe000_0001)
            .expect("C's damage object");
        assert_eq!(c_dmg.rects.len(), 1, "C must record exactly one rect");
        let c_rect = c_dmg.rects[0];
        assert_eq!(
            (c_rect.x, c_rect.y, c_rect.width, c_rect.height),
            (5, 5, 30, 40),
            "C-level rect must be in C's coord space",
        );
        assert!(
            c_dmg.pending_notify_fired,
            "C's damage object must have fired its notify",
        );

        // Ancestor level: rect translates by C's (x, y) = (10, 20)
        // into W's coord space; W's extent (500×400) accommodates
        // the full rect so no clipping happens at this level.
        let w_dmg = state
            .damage_objects
            .get(&0xe000_0002)
            .expect("W's damage object — this is the regression gate");
        assert_eq!(
            w_dmg.rects.len(),
            1,
            "W must record exactly one rect via the ancestor walk \
             — pre-fix this is 0 (the load-bearing failure)",
        );
        let w_rect = w_dmg.rects[0];
        assert_eq!(
            (w_rect.x, w_rect.y, w_rect.width, w_rect.height),
            (15, 25, 30, 40),
            "W-level rect must be C's rect translated by C's (x, y) = (10, 20)",
        );
        assert!(
            w_dmg.pending_notify_fired,
            "W's damage object must have fired its notify too",
        );
    }

    /// Clipping at the ancestor boundary: child overhangs parent's
    /// edge, ancestor damage reports only the visible (clipped)
    /// portion in parent coords.
    #[test]
    fn paint_into_child_clips_to_ancestor_extent() {
        let mut state = ServerState::new();
        add_client(&mut state, 1, 0x0010_0000);
        // Parent W 100×100 at (0, 0). Child C at (-5, -10) sized
        // 200×200 — overhangs every edge of W.
        let w_id = add_window(&mut state, 1, 0x0010_0001, ROOT_WINDOW, 0, 0, 100, 100);
        let c_id = add_window(&mut state, 1, 0x0010_0002, w_id, -5, -10, 200, 200);
        add_damage_on(&mut state, 1, 0xe000_0001, w_id);

        // Paint the full extent of C (relative coords (0, 0, 200, 200)).
        // Translated into W's coords: (-5, -10, 200, 200). Clipped
        // to W (100×100): (0, 0, 95, 90) — left/top edges clip at
        // 0, right/bottom edges clip at 100. Width = 95 (= -5 + 200
        // - clamp-to-0 = 195, then min(195, 100 - 0) = 100? Wait —
        // recompute: x0 = max(-5, 0) = 0, x1 = min(-5+200, 100) =
        // 100; width = 100 - 0 = 100. y0 = max(-10, 0) = 0, y1 =
        // min(-10+200, 100) = 100; height = 100. So the entire
        // parent extent is covered. Asserting the full-cover case
        // first; the partial-cover case below tests the trimmed edge.
        let _ = accumulate_damage_to_state(&mut state, c_id, 0, 0, 200, 200);

        let w_dmg = state
            .damage_objects
            .get(&0xe000_0001)
            .expect("W's damage object");
        assert_eq!(w_dmg.rects.len(), 1);
        let r = w_dmg.rects[0];
        assert_eq!(
            (r.x, r.y, r.width, r.height),
            (0, 0, 100, 100),
            "overhanging child paint clips to parent's full extent",
        );
    }

    /// Partial-clip variant: child paint partly overhangs parent's
    /// right edge — the reported rect is the visible left half.
    #[test]
    fn paint_into_child_clips_partial_overhang() {
        let mut state = ServerState::new();
        add_client(&mut state, 1, 0x0010_0000);
        let w_id = add_window(&mut state, 1, 0x0010_0001, ROOT_WINDOW, 0, 0, 100, 100);
        // Child fully inside parent at (10, 10), 200×30 — extends
        // past parent's right edge (10 + 200 = 210 > 100).
        let c_id = add_window(&mut state, 1, 0x0010_0002, w_id, 10, 10, 200, 30);
        add_damage_on(&mut state, 1, 0xe000_0001, w_id);

        // Paint at (0, 0, 200, 30) inside C — translates to
        // (10, 10, 200, 30) in W's coords, clips to (10, 10, 90, 30)
        // at W's right edge.
        let _ = accumulate_damage_to_state(&mut state, c_id, 0, 0, 200, 30);

        let r = state.damage_objects[&0xe000_0001].rects[0];
        assert_eq!((r.x, r.y, r.width, r.height), (10, 10, 90, 30));
    }

    /// Paint into a child that lies entirely outside its parent
    /// (offscreen): no damage propagates to the parent.
    #[test]
    fn paint_into_offscreen_child_skips_ancestor() {
        let mut state = ServerState::new();
        add_client(&mut state, 1, 0x0010_0000);
        let w_id = add_window(&mut state, 1, 0x0010_0001, ROOT_WINDOW, 0, 0, 100, 100);
        // Child far past parent's right edge.
        let c_id = add_window(&mut state, 1, 0x0010_0002, w_id, 200, 200, 50, 50);
        add_damage_on(&mut state, 1, 0xe000_0001, w_id);

        let _ = accumulate_damage_to_state(&mut state, c_id, 0, 0, 50, 50);

        assert!(
            state.damage_objects[&0xe000_0001].rects.is_empty(),
            "offscreen child paint must not propagate damage to parent",
        );
    }

    /// Walk terminates at root (self-parent sentinel). A damage
    /// subscription on root fires from a deep paint without the
    /// walk running away.
    #[test]
    fn ancestor_walk_terminates_at_root_self_parent() {
        let mut state = ServerState::new();
        add_client(&mut state, 1, 0x0010_0000);
        let w_id = add_window(&mut state, 1, 0x0010_0001, ROOT_WINDOW, 50, 60, 400, 300);
        let c_id = add_window(&mut state, 1, 0x0010_0002, w_id, 20, 30, 100, 80);
        let gc_id = add_window(&mut state, 1, 0x0010_0003, c_id, 5, 10, 40, 30);
        add_damage_on(&mut state, 1, 0xe000_0001, ROOT_WINDOW);

        let _ = accumulate_damage_to_state(&mut state, gc_id, 1, 2, 5, 5);

        // Translation: gc (5, 10) + c (20, 30) + w (50, 60) = (75, 100).
        // Paint origin (1, 2) lands at root (75 + 1, 100 + 2) = (76, 102).
        let r = state.damage_objects[&0xe000_0001].rects[0];
        assert_eq!((r.x, r.y, r.width, r.height), (76, 102, 5, 5));
        // Verify the test didn't hang: the seqno bump on the client
        // is a cheap proxy for "the encoder ran and didn't deadlock."
        let _ = state.clients[&1].last_sequence.load(Ordering::Relaxed);
    }

    /// Pixmap drawables have no place in the window tree, so the
    /// ancestor walk doesn't run for them. A DamageCreate on a
    /// window doesn't fire when an unrelated pixmap is painted.
    #[test]
    fn pixmap_paint_does_not_walk_window_ancestors() {
        let mut state = ServerState::new();
        add_client(&mut state, 1, 0x0010_0000);
        let w_id = add_window(&mut state, 1, 0x0010_0001, ROOT_WINDOW, 0, 0, 100, 100);
        // Pixmap created against root (parent for depth/visual
        // inheritance only — pixmaps don't live in the window tree).
        let pix_id = ResourceId(0x0010_0005);
        state.resources.create_pixmap(
            ClientId(1),
            CreatePixmapRequest {
                depth: 32,
                pixmap: pix_id,
                drawable: ROOT_WINDOW,
                width: 16,
                height: 16,
            },
        );
        add_damage_on(&mut state, 1, 0xe000_0001, w_id);

        let _ = accumulate_damage_to_state(&mut state, pix_id, 0, 0, 16, 16);

        assert!(
            state.damage_objects[&0xe000_0001].rects.is_empty(),
            "pixmap paint must not fan out to window-tree damage objects",
        );
    }

    fn drawable_geometry_full_extent(state: &ServerState, drawable: ResourceId) -> (u16, u16) {
        let r = drawable_full_rect(state, drawable);
        (r.width, r.height)
    }

    #[test]
    fn drawable_full_rect_is_zero_for_unknown_drawable() {
        let state = ServerState::new();
        assert_eq!(
            drawable_geometry_full_extent(&state, ResourceId(0xdead_beef)),
            (0, 0),
        );
    }
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
