//! Frame-ownership and scheduling primitives.
//!
//! See `docs/superpowers/specs/2026-05-12-rendering-rearchitecture-hld.md`.
//! Phase 1 lands the types with minimal behavior; recorders and the
//! hot-path `vkQueueWaitIdle` calls are unchanged.

pub mod composite_pool_ring;
pub mod damage;
pub mod in_flight;
pub mod output_frame;
pub mod paint_batch;

use self::{in_flight::InFlight, paint_batch::PaintBatch};

/// Server-wide scheduling state. Owned as a single field on
/// `KmsBackend`. Per-output damage lives on `OutputLayout`, not
/// here — `OutputLayout` is the natural home for per-output state.
///
/// Phase 1: a shell wrapping the in-flight queue and the
/// current paint batch. The "only layer allowed to submit on the
/// hot path" invariant from the HLD is not yet enforced — phase 1
/// recorders still call `run_one_shot_op` directly.
#[derive(Debug, Default)]
pub struct RenderScheduler {
    pub in_flight: InFlight,
    pub current_paint_batch: Option<PaintBatch>,
}

impl RenderScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a paint batch for this composite cycle if one isn't
    /// already open. Returns the batch's `frame_id`. Phase 1:
    /// called from `composite_and_flip` at the start of each cycle.
    pub fn open_batch(&mut self) -> u64 {
        if let Some(batch) = self.current_paint_batch.as_ref() {
            return batch.frame_id;
        }
        let frame_id = self.in_flight.allocate_frame_id();
        self.current_paint_batch = Some(PaintBatch::new(frame_id));
        frame_id
    }

    /// Close the current batch. Phase 1: nothing to flush; the
    /// batch is just discarded. Phase 2+ submits its CB here.
    pub fn close_batch(&mut self) -> Option<PaintBatch> {
        self.current_paint_batch.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_batch_allocates_monotonic_frame_ids() {
        let mut s = RenderScheduler::new();
        let a = s.open_batch();
        s.close_batch();
        let b = s.open_batch();
        assert!(b > a);
    }

    #[test]
    fn open_batch_is_idempotent_within_a_cycle() {
        let mut s = RenderScheduler::new();
        let a = s.open_batch();
        let b = s.open_batch();
        assert_eq!(a, b, "re-opening without closing returns the same frame_id");
    }

    #[test]
    fn close_batch_drops_current() {
        let mut s = RenderScheduler::new();
        s.open_batch();
        assert!(s.current_paint_batch.is_some());
        s.close_batch();
        assert!(s.current_paint_batch.is_none());
    }
}
