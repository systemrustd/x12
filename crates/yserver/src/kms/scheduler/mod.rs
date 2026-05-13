//! Frame-ownership and scheduling primitives.
//!
//! See `docs/superpowers/specs/2026-05-12-rendering-rearchitecture-hld.md`.
//! Phase 3A introduces batch-owned resource lifetimes (PaintBatch
//! state machine, BatchUploadArena, per-batch descriptor pools).

use std::sync::Arc;

use ash::vk;

use crate::kms::vk::device::VkContext;

use self::{
    in_flight::InFlight,
    paint_batch::{BatchError, BatchState, PaintBatch},
};

pub mod batch_descriptor_arena;
pub mod batch_upload_arena;
pub mod composite_pool_ring;
pub mod damage;
pub mod in_flight;
pub mod output_frame;
pub mod paint_batch;

/// Maximum number of paint batches that can be Submitted but
/// not yet Retired. Phase 4 T4 backpressure: when
/// `close_and_submit_async` would push past this, the oldest
/// batch is blocking-waited first.
///
/// 4 ≈ one composite period at 60 Hz worth of paint cycles; high
/// enough to absorb bursty paint without blocking, low enough to
/// bound GPU-side queue depth and CPU-side resource lifetime.
const MAX_IN_FLIGHT_PAINT_BATCHES: usize = 4;

#[derive(Debug, Default)]
pub struct RenderScheduler {
    pub in_flight: InFlight,
    pub current_paint_batch: Option<PaintBatch>,
    /// FIFO queue of submitted-but-not-yet-retired paint batches.
    /// Pushed by `close_and_submit_async`; drained by
    /// `poll_retired_paint_batches` when each batch's fence
    /// signals. Retirement is strict-prefix-FIFO (stop at first
    /// non-signaled batch — resource lifetimes layer on submission
    /// order, same as `InFlight::drain_retired`).
    pub submitted_paint_batches: std::collections::VecDeque<PaintBatch>,
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

    /// Async sibling of `close_and_submit`. Submits the current
    /// paint batch and moves it to `submitted_paint_batches` for
    /// later poll-driven retirement. Returns the fence handle so
    /// callers that need synchronous completion (strict flushes)
    /// can `wait_for_fences` on it themselves.
    ///
    /// Behavior on no-batch / Idle / Poisoned matches
    /// `close_and_submit`:
    ///   - no batch open → `Ok(vk::Fence::null())`.
    ///   - Idle batch (no CB) → retires directly, returns null fence.
    ///   - Poisoned → `Ok(vk::Fence::null())` (best-effort
    ///     swallows; strict caller surfaces separately).
    ///
    /// Path-1 (submit failure): same as `close_and_submit` — the
    /// returned `Err(BatchError::Vk)` is fatal to the KMS renderer.
    #[allow(clippy::missing_errors_doc)]
    pub fn close_and_submit_async(
        &mut self,
        dirty_outputs: Vec<usize>,
    ) -> Result<vk::Fence, BatchError> {
        // 4-T4: backpressure. If the in-flight queue is at cap,
        // synchronously wait on the oldest batch before pushing
        // a new one. Prevents unbounded growth under chatty
        // clients (e.g. mate-cc + adapta-nokto's pixmap-churn
        // pattern that fires hundreds of ProtocolBarrier flushes
        // per second).
        while self.submitted_paint_batches.len() >= MAX_IN_FLIGHT_PAINT_BATCHES {
            let Some(mut oldest) = self.submitted_paint_batches.pop_front() else {
                break;
            };
            oldest.wait_for_completion()?;
            drop(oldest);
        }

        let Some(mut batch) = self.current_paint_batch.take() else {
            return Ok(vk::Fence::null());
        };
        batch.dirty_outputs = dirty_outputs;
        match batch.submit_async() {
            Ok(fence) => {
                if batch.state() == BatchState::Submitted {
                    self.submitted_paint_batches.push_back(batch);
                }
                // else: state was Idle and submit_async retired
                // synchronously; nothing to push. fence is null.
                Ok(fence)
            }
            Err(BatchError::Poisoned) => {
                drop(batch);
                Ok(vk::Fence::null())
            }
            Err(e) => Err(e),
        }
    }

    /// Non-blocking poll over the submitted-paint-batch queue.
    /// Walks front-to-back; calls `try_retire_if_signaled` on
    /// each; removes any that successfully retired. Stops at the
    /// first non-signaled batch (strict-prefix-FIFO).
    ///
    /// Returns the number of batches retired. On a fence-status
    /// error, the batch is left in the queue in `Submitted`
    /// (leaked resources) and the error propagates — the caller
    /// MUST treat this as a renderer-failed condition.
    #[allow(clippy::missing_errors_doc)]
    pub fn poll_retired_paint_batches(&mut self) -> Result<usize, BatchError> {
        let mut retired = 0;
        while let Some(batch) = self.submitted_paint_batches.front_mut() {
            if batch.try_retire_if_signaled()? {
                self.submitted_paint_batches.pop_front();
                retired += 1;
            } else {
                break;
            }
        }
        Ok(retired)
    }

    /// Synchronously retire every batch currently in the
    /// submitted-paint-batches queue. For shutdown only — best-
    /// effort `flush_if_needed(Shutdown)` would leave batches in
    /// the queue and `PaintBatch::drop` would leak them.
    ///
    /// Order: FIFO. Calls `wait_for_completion` on each batch.
    /// On any failure, the remaining batches are left in the
    /// queue (their `Drop` will fire the leak warning); the
    /// error propagates. Caller MUST already have latched a
    /// renderer-failed condition before calling, OR be prepared
    /// to handle the leak (acceptable at shutdown).
    ///
    /// Should be called AFTER `vkDeviceWaitIdle()` in the
    /// teardown sequence — at that point every fence is
    /// guaranteed signaled, so each `wait_for_completion`
    /// returns immediately. Calling before `vkDeviceWaitIdle`
    /// would block on each fence in turn (still correct, just
    /// pessimal).
    #[allow(clippy::missing_errors_doc)]
    pub fn drain_submitted_paint_batches(&mut self) -> Result<(), BatchError> {
        while let Some(mut batch) = self.submitted_paint_batches.pop_front() {
            batch.wait_for_completion()?;
            drop(batch);
        }
        Ok(())
    }

    /// Total in-flight paint batches (Submitted, not yet Retired).
    /// Used by backpressure logic in T4.
    pub fn pending_paint_batches(&self) -> usize {
        self.submitted_paint_batches.len()
    }

    /// State of the current batch, or `None` if no batch is open.
    /// Used by audit assertions in `composite_and_flip`.
    #[must_use]
    pub fn current_batch_state(&self) -> Option<BatchState> {
        self.current_paint_batch.as_ref().map(PaintBatch::state)
    }

    /// Append a paint-recorder op into the current `PaintBatch`'s
    /// command buffer. Closure receives `(&VkContext, &mut PaintBatch, vk::CommandBuffer)`.
    ///
    /// **This is the load-bearing API** with `&mut PaintBatch`
    /// exposed so 3C+ recorders can call `batch.upload_arena_mut()`,
    /// `batch.descriptor_arena_mut()`, `batch.adopt(resource)` from
    /// inside the closure. 3B fill/copy recorders ignore the batch
    /// parameter (use `record_paint_op` shim).
    ///
    /// `vk` and `pool` are passed in (not pulled from `self`) so the
    /// `&mut self` here is `&mut RenderScheduler` only — disjoint
    /// from `&mut KmsBackend.windows` / `.pixmaps` borrows the call
    /// site holds for the recorder's `&mut DrawableImage` argument.
    pub fn record_paint_batch_op<F>(
        &mut self,
        vk_arc: Arc<VkContext>,
        pool: vk::CommandPool,
        record: F,
    ) -> Result<(), vk::Result>
    where
        F: FnOnce(&VkContext, &mut PaintBatch, vk::CommandBuffer) -> Result<(), vk::Result>,
    {
        // open_batch consumes the Arc — clone for the closure invocation.
        // Note: parameter is `vk_arc` not `vk` so it doesn't shadow the
        // `ash::vk` module used for `vk::Result::*` below.
        let _ = self.open_batch(vk_arc.clone(), pool);
        let batch = self
            .current_paint_batch
            .as_mut()
            .expect("open_batch just ran");
        match batch.state() {
            BatchState::Poisoned => return Err(vk::Result::ERROR_DEVICE_LOST),
            BatchState::Closed | BatchState::Submitted | BatchState::Retired => {
                log::error!(
                    "record_paint_batch_op: batch in non-recording state {:?}",
                    batch.state()
                );
                return Err(vk::Result::ERROR_UNKNOWN);
            }
            BatchState::Idle => {
                if let Err(e) = batch.begin_recording_explicit() {
                    return Err(match e {
                        BatchError::Vk(r) => r,
                        _ => vk::Result::ERROR_UNKNOWN,
                    });
                }
            }
            BatchState::Recording => {}
        }
        let cb = batch.command_buffer().expect("Recording implies cb");
        match record(&vk_arc, batch, cb) {
            Ok(()) => Ok(()),
            Err(e) => {
                batch.poison_external();
                Err(e)
            }
        }
    }

    /// Thin shim for recorders that don't need the batch handle
    /// (3B fill/copy). Same closure signature as `run_one_shot_op`
    /// for textual-rewrite migration.
    pub fn record_paint_op<F>(
        &mut self,
        vk_arc: Arc<VkContext>,
        pool: vk::CommandPool,
        record: F,
    ) -> Result<(), vk::Result>
    where
        F: FnOnce(&VkContext, vk::CommandBuffer) -> Result<(), vk::Result>,
    {
        self.record_paint_batch_op(vk_arc, pool, |vk, _batch, cb| record(vk, cb))
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
