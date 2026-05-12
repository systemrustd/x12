//! Frame-ownership and scheduling primitives.
//!
//! See `docs/superpowers/specs/2026-05-12-rendering-rearchitecture-hld.md`.
//! Phase 1 lands the types with minimal behavior; recorders and the
//! hot-path `vkQueueWaitIdle` calls are unchanged.

use std::sync::Arc;

use ash::vk;

use crate::kms::vk::device::VkContext;

use self::{
    in_flight::InFlight,
    paint_batch::{BatchError, BatchState, PaintBatch},
};

// `batch_upload_arena` is added in T2; `batch_descriptor_arena` in T3.
pub mod composite_pool_ring;
pub mod damage;
pub mod in_flight;
pub mod output_frame;
pub mod paint_batch;

#[derive(Debug, Default)]
pub struct RenderScheduler {
    pub in_flight: InFlight,
    pub current_paint_batch: Option<PaintBatch>,
}

impl RenderScheduler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a paint batch if one isn't already open. Returns the
    /// batch's `frame_id`. Phase 3 takes `vk` + `pool` so the batch
    /// can lazy-allocate a primary CB on first append.
    pub fn open_batch(&mut self, vk: Arc<VkContext>, pool: vk::CommandPool) -> u64 {
        if let Some(batch) = self.current_paint_batch.as_ref() {
            return batch.frame_id;
        }
        let frame_id = self.in_flight.allocate_frame_id();
        self.current_paint_batch = Some(PaintBatch::new(frame_id, vk, pool));
        frame_id
    }

    /// Close + submit the current batch. Returns `Ok(())` if no
    /// batch was open, if it was Idle, or if it was already
    /// Poisoned (a recorder error this cycle — paint is best-effort
    /// at the cycle-close granularity). Phase 3 invariant:
    /// composite samples mirrors this batch wrote, so the
    /// `vkQueueWaitIdle` inside `submit_and_wait` is what makes
    /// the next-step composite safe.
    ///
    /// **Returning `Err(BatchError::Vk)` is fatal to the KMS
    /// renderer.** Per `PaintBatch::submit_and_wait`'s path-2
    /// semantics, a Vk error here means a CB and its resources
    /// have been abandoned. Callers MUST stop normal rendering
    /// and enter backend teardown; do not continue calling
    /// `record_paint_op` / `flush_if_needed`.
    ///
    /// `dirty_outputs` is populated by the caller before this
    /// returns — `composite_and_flip` knows which outputs passed
    /// the damage gate (candidates only; see `PaintBatch::dirty_outputs`
    /// field doc).
    #[allow(clippy::missing_errors_doc)]
    pub fn close_and_submit(&mut self, dirty_outputs: Vec<usize>) -> Result<(), BatchError> {
        let Some(mut batch) = self.current_paint_batch.take() else {
            return Ok(());
        };
        batch.dirty_outputs = dirty_outputs;
        let result = batch.submit_and_wait();
        drop(batch); // releases via retire/poison/leak per state
        match result {
            Ok(()) | Err(BatchError::Poisoned) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// State of the current batch, or `None` if no batch is open.
    /// Used by audit assertions in `composite_and_flip`.
    #[must_use]
    pub fn current_batch_state(&self) -> Option<BatchState> {
        self.current_paint_batch.as_ref().map(PaintBatch::state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_scheduler_has_no_batch_open() {
        let s = RenderScheduler::new();
        assert!(s.current_paint_batch.is_none());
    }

    #[test]
    fn close_and_submit_with_no_batch_is_noop() {
        let mut s = RenderScheduler::new();
        assert!(s.close_and_submit(Vec::new()).is_ok());
    }
}
