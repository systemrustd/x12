//! Stage 5 frame-builder Phase B sub-phase B.1: deferred per-frame
//! op-list recording.
//!
//! `FrameBuilder` owns a `Closed ↔ OpenForPaint` lifecycle. Paint
//! entry points that have been ported (`composite_glyphs` in B.1)
//! append `RecordedOp`s; a close trigger (Invariant M2 / M3, the
//! existing get_image / PRESENT-completion sync points, a timeout,
//! shutdown, or a pin-set ceiling) replays the op list as ONE primary
//! command buffer, submits it via the SubmitGroup (cap=1, so the
//! submit auto-flushes immediately), and parks the frame's resource
//! pins on a `pending_frames` queue gated by the submit's
//! `FenceTicket`.
//!
//! Phase B spec — `docs/superpowers/specs/2026-05-24-frame-builder-phase-b-design.md`.
//! This file holds the no-Vk-required pieces (state machine, op enum,
//! pin sets, layout overlay); the recording side lives in
//! `engine.rs::FrameBuilder::close_into_cb_*` because it needs the
//! engine's CB pool + atlas + drawable-store access.

use super::platform::FenceTicket;

/// Why a frame closed. Bumped into telemetry on every close so the
/// rollout can see which trigger is dominating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseReason {
    /// `maybe_composite` saw a ready output + dirty scene; the frame
    /// closes paint-only (compose stays separate in B.1 — folded into
    /// the frame at B.4).
    SceneCompose,
    /// Invariant M2: a non-ported paint op is about to record its own
    /// CB; close the frame first so the non-ported op sees committed
    /// `Drawable::storage.current_layout` + `last_render_ticket`.
    NonPortedPaintOp,
    /// Invariant M3: legacy scene compose is about to record; close
    /// the frame first for the same reason as M2.
    LegacyScCompose,
    /// COW PRESENT-completion semaphore got attached; the frame must
    /// close immediately so `vkGetSemaphoreFdKHR(SYNC_FD)` sees a
    /// queued signal-op (Task 6.1 yoga hang precedent).
    PresentCompletionSignal,
    /// `get_image` is about to wait on a fence; close the frame first
    /// so the readback's `ticket.wait()` observes a submitted CB.
    SyncWait,
    /// Idle / no-pageflip case. A frame open > T ms forces close to
    /// release pinned resources.
    Timeout,
    /// `KmsBackendV2::shutdown` is tearing down platform state.
    Shutdown,
    /// `max_pinned_resources_per_frame` ceiling hit (1024 default).
    PinCeiling,
}

/// FrameBuilder lifecycle. `Closed` is the hot path for X11 traffic
/// that doesn't touch the paint surface (event-only requests, idle).
/// `OpenForPaint` is where every recorded op accumulates between
/// the first paint and a close trigger.
///
/// Phase B's spec sketches a third state, `ClosingWithCompose`, for
/// when scene compose joins the frame. That state lands in sub-phase
/// B.4; B.1 only carries the two-state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameState {
    Closed,
    OpenForPaint,
}

#[derive(Debug)]
pub(crate) struct FrameBuilder {
    state: FrameState,
    open: Option<Box<OpenFrame>>,
    lifetime_opens: u64,
    lifetime_closes: u64,
    max_pinned_resources_per_frame: usize,
}

impl FrameBuilder {
    pub(crate) fn new() -> Self {
        Self {
            state: FrameState::Closed,
            open: None,
            lifetime_opens: 0,
            lifetime_closes: 0,
            max_pinned_resources_per_frame: 1024,
        }
    }

    pub(crate) fn state(&self) -> FrameState {
        self.state
    }

    pub(crate) fn is_open(&self) -> bool {
        matches!(self.state, FrameState::OpenForPaint)
    }

    pub(crate) fn lifetime_opens(&self) -> u64 {
        self.lifetime_opens
    }

    pub(crate) fn lifetime_closes(&self) -> u64 {
        self.lifetime_closes
    }

    pub(crate) fn set_max_pinned_resources_per_frame(&mut self, n: usize) {
        self.max_pinned_resources_per_frame = n.max(1);
    }

    pub(crate) fn max_pinned_resources_per_frame(&self) -> usize {
        self.max_pinned_resources_per_frame
    }
}

/// Per-frame bookkeeping. Allocated when `Closed → OpenForPaint` fires;
/// dropped on close.
#[derive(Debug)]
pub(crate) struct OpenFrame {
    pub(crate) ticket: FenceTicket,
    pub(crate) close_reason_on_open: Option<CloseReason>, // unused in B.1; reserved for B.4
}

#[cfg(test)]
mod state_tests {
    use super::*;

    #[test]
    fn fresh_frame_builder_is_closed_with_no_lifetime_counts() {
        let fb = FrameBuilder::new();
        assert_eq!(fb.state(), FrameState::Closed);
        assert!(!fb.is_open());
        assert_eq!(fb.lifetime_opens(), 0);
        assert_eq!(fb.lifetime_closes(), 0);
    }

    #[test]
    fn default_pin_ceiling_is_1024() {
        let fb = FrameBuilder::new();
        assert_eq!(fb.max_pinned_resources_per_frame(), 1024);
    }

    #[test]
    fn set_max_pinned_resources_clamps_to_at_least_one() {
        let mut fb = FrameBuilder::new();
        fb.set_max_pinned_resources_per_frame(0);
        assert_eq!(fb.max_pinned_resources_per_frame(), 1);
        fb.set_max_pinned_resources_per_frame(42);
        assert_eq!(fb.max_pinned_resources_per_frame(), 42);
    }

    #[test]
    fn close_reason_has_eight_variants_for_b1() {
        fn _exhaustive(r: CloseReason) -> &'static str {
            match r {
                CloseReason::SceneCompose => "scene_compose",
                CloseReason::NonPortedPaintOp => "non_ported_paint_op",
                CloseReason::LegacyScCompose => "legacy_sc_compose",
                CloseReason::PresentCompletionSignal => "present_completion_signal",
                CloseReason::SyncWait => "sync_wait",
                CloseReason::Timeout => "timeout",
                CloseReason::Shutdown => "shutdown",
                CloseReason::PinCeiling => "pin_ceiling",
            }
        }
        assert_eq!(_exhaustive(CloseReason::SceneCompose), "scene_compose");
    }
}

// The rest of this module — RecordedOp, FramePinSet, FrameLayoutTable,
// FrameSubmittedRecord — lands in subsequent tasks.
