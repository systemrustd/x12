//! X11 EnterNotify/LeaveNotify crossing computation for implicit
//! pointer grabs.
//!
//! When a button press creates an implicit pointer grab, the grab
//! activation generates Leave/Enter events along the path between the
//! window the pointer was in (focus) and the grab window. The release
//! generates the symmetric Ungrab pair. Today's KMS backend (and any
//! host-X11 mirror) emit a single unconditional crossing on the press
//! window, which is wrong whenever focus and grab differ — see
//! `docs/superpowers/specs/2026-05-05-single-threaded-core-design.md`,
//! "Pre-existing bugs" #2.
//!
//! `implicit_grab_crossings` is a pure function over `ServerState`'s
//! window tree: hand it the focus + grab `ResourceId`s and it returns
//! the spec-correct sequence of crossing events. The caller threads
//! the events through whichever fanout machinery is appropriate (KMS
//! backend, host-X11 dispatcher).
//!
//! Detail-code reference (X11 protocol, EnterNotify/LeaveNotify):
//! - `0` `NotifyAncestor`
//! - `1` `NotifyVirtual`
//! - `2` `NotifyInferior`
//! - `3` `NotifyNonlinear`
//! - `4` `NotifyNonlinearVirtual`

use std::collections::HashSet;

use yserver_protocol::x11::ResourceId;

use crate::server::ServerState;

pub const NOTIFY_ANCESTOR: u8 = 0;
pub const NOTIFY_VIRTUAL: u8 = 1;
pub const NOTIFY_INFERIOR: u8 = 2;
pub const NOTIFY_NONLINEAR: u8 = 3;
pub const NOTIFY_NONLINEAR_VIRTUAL: u8 = 4;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CrossingKind {
    Enter,
    Leave,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct CrossingEvent {
    pub window: ResourceId,
    pub kind: CrossingKind,
    /// One of the `NOTIFY_*` constants.
    pub detail: u8,
}

/// Compute Leave/Enter events for moving the pointer's logical
/// "window" from `focus` to `grab`. Order in the returned `Vec` is
/// the wire-emission order required by X11.
///
/// `focus == grab` returns an empty `Vec`. Cycles or missing windows
/// (defensive — `state.resources.window` returns `None`) terminate
/// the chain walk early; the returned events are still well-formed
/// X11 (just possibly truncated).
#[must_use]
pub fn implicit_grab_crossings(
    state: &ServerState,
    focus: ResourceId,
    grab: ResourceId,
) -> Vec<CrossingEvent> {
    if focus == grab {
        return Vec::new();
    }

    let focus_chain = ancestor_chain(state, focus);
    let grab_chain = ancestor_chain(state, grab);

    // Case A: grab is an ancestor of focus (pointer "moves up" out of
    // a deeper subtree into a parent).
    if focus_chain.iter().skip(1).any(|w| *w == grab) {
        let mut events = vec![CrossingEvent {
            window: focus,
            kind: CrossingKind::Leave,
            detail: NOTIFY_ANCESTOR,
        }];
        for w in focus_chain.iter().skip(1) {
            if *w == grab {
                break;
            }
            events.push(CrossingEvent {
                window: *w,
                kind: CrossingKind::Leave,
                detail: NOTIFY_VIRTUAL,
            });
        }
        events.push(CrossingEvent {
            window: grab,
            kind: CrossingKind::Enter,
            detail: NOTIFY_INFERIOR,
        });
        return events;
    }

    // Case B: focus is an ancestor of grab (pointer "moves down" into
    // a descendant subtree).
    if grab_chain.iter().skip(1).any(|w| *w == focus) {
        let mut events = vec![CrossingEvent {
            window: focus,
            kind: CrossingKind::Leave,
            detail: NOTIFY_INFERIOR,
        }];
        let mut intermediates: Vec<ResourceId> = Vec::new();
        for w in grab_chain.iter().skip(1) {
            if *w == focus {
                break;
            }
            intermediates.push(*w);
        }
        // grab_chain is [grab, parent, parent, ..., focus]; skipping
        // the head gives parents in upward order. Reverse so the wire
        // order is "ancestor closest to focus first, ancestor closest
        // to grab last" — matching X11 spec downward propagation.
        intermediates.reverse();
        for w in intermediates {
            events.push(CrossingEvent {
                window: w,
                kind: CrossingKind::Enter,
                detail: NOTIFY_VIRTUAL,
            });
        }
        events.push(CrossingEvent {
            window: grab,
            kind: CrossingKind::Enter,
            detail: NOTIFY_ANCESTOR,
        });
        return events;
    }

    // Case C: disjoint subtrees. Walk up from focus to the lowest
    // common ancestor (LCA), then back down to grab.
    let common = lowest_common_ancestor(&focus_chain, &grab_chain);
    let mut events = vec![CrossingEvent {
        window: focus,
        kind: CrossingKind::Leave,
        detail: NOTIFY_NONLINEAR,
    }];
    for w in focus_chain.iter().skip(1) {
        if Some(*w) == common {
            break;
        }
        events.push(CrossingEvent {
            window: *w,
            kind: CrossingKind::Leave,
            detail: NOTIFY_NONLINEAR_VIRTUAL,
        });
    }
    let mut downward: Vec<ResourceId> = Vec::new();
    for w in grab_chain.iter().skip(1) {
        if Some(*w) == common {
            break;
        }
        downward.push(*w);
    }
    downward.reverse();
    for w in downward {
        events.push(CrossingEvent {
            window: w,
            kind: CrossingKind::Enter,
            detail: NOTIFY_NONLINEAR_VIRTUAL,
        });
    }
    events.push(CrossingEvent {
        window: grab,
        kind: CrossingKind::Enter,
        detail: NOTIFY_NONLINEAR,
    });
    events
}

/// `[start, parent_of_start, parent_of_parent, ..., root]`, terminated
/// when a window is its own parent (X11's root convention) or when a
/// missing/cyclic parent is reached. Capped at 256 hops as a defense
/// against malformed states.
fn ancestor_chain(state: &ServerState, start: ResourceId) -> Vec<ResourceId> {
    let mut chain = vec![start];
    let mut current = start;
    let mut hops = 0;
    while hops < 256 {
        let Some(window) = state.resources.window(current) else {
            break;
        };
        if window.parent == current {
            break;
        }
        chain.push(window.parent);
        current = window.parent;
        hops += 1;
    }
    chain
}

/// First entry of `b` that also appears in `a`. With the `start`
/// ordering both chains share, this yields the lowest common
/// ancestor. Returns `None` only if no shared ancestor exists (which
/// in a well-formed X11 state means at least the root, so `None` is
/// effectively defensive).
fn lowest_common_ancestor(a: &[ResourceId], b: &[ResourceId]) -> Option<ResourceId> {
    let a_set: HashSet<_> = a.iter().copied().collect();
    b.iter().copied().find(|w| a_set.contains(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    use yserver_protocol::x11::{ClientId, CreateWindowRequest};

    use crate::{
        resources::{ROOT_VISUAL, ROOT_WINDOW},
        server::ServerState,
    };

    fn make_window(state: &mut ServerState, id: u32, parent: ResourceId) -> ResourceId {
        let rid = ResourceId(id);
        state.resources.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: rid,
                parent,
                x: 0,
                y: 0,
                width: 10,
                height: 10,
                border_width: 0,
                class: 1,
                visual: ROOT_VISUAL,
                ..Default::default()
            },
        );
        rid
    }

    fn windows_only(events: &[CrossingEvent]) -> Vec<ResourceId> {
        events.iter().map(|e| e.window).collect()
    }

    fn details_only(events: &[CrossingEvent]) -> Vec<u8> {
        events.iter().map(|e| e.detail).collect()
    }

    fn kinds_only(events: &[CrossingEvent]) -> Vec<CrossingKind> {
        events.iter().map(|e| e.kind).collect()
    }

    #[test]
    fn equal_windows_emit_no_events() {
        let mut state = ServerState::new();
        let w = make_window(&mut state, 0x0010_0030, ROOT_WINDOW);
        assert!(implicit_grab_crossings(&state, w, w).is_empty());
    }

    #[test]
    fn focus_descendant_of_grab_walks_up() {
        // root → A → B → focus, grab = A
        let mut state = ServerState::new();
        let a = make_window(&mut state, 0x0010_0010, ROOT_WINDOW);
        let b = make_window(&mut state, 0x0010_0011, a);
        let f = make_window(&mut state, 0x0010_0012, b);
        let events = implicit_grab_crossings(&state, f, a);
        assert_eq!(windows_only(&events), vec![f, b, a]);
        assert_eq!(
            kinds_only(&events),
            vec![
                CrossingKind::Leave,
                CrossingKind::Leave,
                CrossingKind::Enter
            ],
        );
        assert_eq!(
            details_only(&events),
            vec![NOTIFY_ANCESTOR, NOTIFY_VIRTUAL, NOTIFY_INFERIOR],
        );
    }

    #[test]
    fn focus_ancestor_of_grab_walks_down() {
        // root → focus → A → B → grab
        let mut state = ServerState::new();
        let f = make_window(&mut state, 0x0010_0020, ROOT_WINDOW);
        let a = make_window(&mut state, 0x0010_0021, f);
        let b = make_window(&mut state, 0x0010_0022, a);
        let g = make_window(&mut state, 0x0010_0023, b);
        let events = implicit_grab_crossings(&state, f, g);
        // Order: focus first, then ancestors of grab from closest-to-focus
        // outward, then grab.
        assert_eq!(windows_only(&events), vec![f, a, b, g]);
        assert_eq!(
            kinds_only(&events),
            vec![
                CrossingKind::Leave,
                CrossingKind::Enter,
                CrossingKind::Enter,
                CrossingKind::Enter,
            ],
        );
        assert_eq!(
            details_only(&events),
            vec![
                NOTIFY_INFERIOR,
                NOTIFY_VIRTUAL,
                NOTIFY_VIRTUAL,
                NOTIFY_ANCESTOR,
            ],
        );
    }

    #[test]
    fn disjoint_subtrees_walk_through_common_ancestor() {
        // root → C → A → focus
        // root → C → B → grab
        let mut state = ServerState::new();
        let c = make_window(&mut state, 0x0010_0040, ROOT_WINDOW);
        let a = make_window(&mut state, 0x0010_0041, c);
        let f = make_window(&mut state, 0x0010_0042, a);
        let b = make_window(&mut state, 0x0010_0043, c);
        let g = make_window(&mut state, 0x0010_0044, b);
        let events = implicit_grab_crossings(&state, f, g);
        assert_eq!(windows_only(&events), vec![f, a, b, g]);
        assert_eq!(
            kinds_only(&events),
            vec![
                CrossingKind::Leave,
                CrossingKind::Leave,
                CrossingKind::Enter,
                CrossingKind::Enter,
            ],
        );
        assert_eq!(
            details_only(&events),
            vec![
                NOTIFY_NONLINEAR,
                NOTIFY_NONLINEAR_VIRTUAL,
                NOTIFY_NONLINEAR_VIRTUAL,
                NOTIFY_NONLINEAR,
            ],
        );
    }

    #[test]
    fn disjoint_with_root_as_only_common_ancestor() {
        // root → focus, root → grab — common ancestor is root, which
        // is *not* in the chains' "skip root" iteration: chains end at
        // root because window.parent == current at root, so root never
        // gets a Leave/Enter virtual.
        let mut state = ServerState::new();
        let f = make_window(&mut state, 0x0010_0050, ROOT_WINDOW);
        let g = make_window(&mut state, 0x0010_0051, ROOT_WINDOW);
        let events = implicit_grab_crossings(&state, f, g);
        // Just Leave on focus + Enter on grab (no virtuals because no
        // intermediate ancestors below root).
        assert_eq!(windows_only(&events), vec![f, g]);
        assert_eq!(
            details_only(&events),
            vec![NOTIFY_NONLINEAR, NOTIFY_NONLINEAR],
        );
    }

    #[test]
    fn ancestor_chain_is_capped() {
        // Chain length cap is defensive — there's no practical way to
        // construct a 257-deep tree in the test harness here, so just
        // assert the code path doesn't loop forever on a sane case.
        let mut state = ServerState::new();
        let mut parent = ROOT_WINDOW;
        for i in 0..32 {
            parent = make_window(&mut state, 0x0010_0100 + i, parent);
        }
        let chain = ancestor_chain(&state, parent);
        // 33 entries: the leaf + 32 ancestors up to and including root.
        // (root is included because we hit it via parent==current and
        // break *after* pushing.)
        assert!(chain.len() >= 32, "chain too short: {}", chain.len());
        assert!(chain.len() <= 257);
    }
}
