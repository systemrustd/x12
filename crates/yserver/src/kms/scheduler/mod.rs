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

/// Outcome of `RenderScheduler::defer_resource_release_decision`. Pure
/// view over the scheduler state at the call site. Test-only callers
/// use this to verify the decision tree; production code uses
/// `defer_resource_release` which does the action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferDecision {
    /// No live batch could reference the resource. Caller releases
    /// synchronously. This covers:
    ///   - Empty scheduler (no submitted batches, no open batch).
    ///   - Open batch is `Idle` / `Poisoned` AND `submitted_paint_batches`
    ///     is empty (no in-flight GPU work, no recorded CB).
    Synchronous,
    /// Open batch is `Recording` / `Closed` (has a CB that may
    /// reference the resource). Adopt into the current open batch;
    /// its fence at submit time signals after every prior submitted
    /// batch's fence (same-queue FIFO), so the adopted resource
    /// outlives every possible reference.
    AdoptOpenRecording,
    /// No usable open batch (None / `Idle` / `Poisoned`) but at least
    /// one batch is in `submitted_paint_batches`. Adopt into the
    /// most-recently-submitted batch (back of the queue). Same FIFO
    /// argument as `AdoptOpenRecording`: that batch's fence is the
    /// latest pending fence on the queue, so when it signals every
    /// older submission has signaled too, and the resource releases
    /// safely. Specifically introduced to fix the `AMD page-fault on
    /// FreePicture defer-release` issue: previously `AdoptOpen`
    /// lazy-opened an Idle batch with no CB, which retired
    /// immediately on the next `close_and_submit{,_async}` regardless
    /// of submitted predecessors — UAF.
    AdoptLastSubmitted,
}

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

    /// Pure-function decision over `(has_submitted, current_state)`.
    /// Codex round-2 P2 refactor: exposing the args explicitly lets
    /// `#[cfg(test)]` callers exercise the full decision tree —
    /// including the `Poisoned` branches — without needing to
    /// construct a real `PaintBatch` (which would require an
    /// `Arc<VkContext>` and a `vk::CommandPool`).
    ///
    /// **Decision tree:**
    ///
    /// | current_state              | has_submitted | result               |
    /// |----------------------------|---------------|----------------------|
    /// | None                       | false         | Synchronous          |
    /// | None                       | true          | AdoptLastSubmitted   |
    /// | Some(Idle)                 | false         | Synchronous          |
    /// | Some(Idle)                 | true          | AdoptLastSubmitted   |
    /// | Some(Recording \| Closed)  | any           | AdoptOpenRecording   |
    /// | Some(Poisoned)             | false         | Synchronous          |
    /// | Some(Poisoned)             | true          | AdoptLastSubmitted   |
    /// | Some(Submitted \| Retired) | any           | (unreachable)        |
    ///
    /// **Why `Idle` routes to `AdoptLastSubmitted` (not `AdoptOpenRecording`).**
    /// An `Idle` batch has no CB allocated; `submit_{and_wait, async}`
    /// short-circuits Idle → Retired without going through the queue.
    /// If we adopted the resource into the Idle batch and submitted
    /// predecessors are still in flight referencing the same VkImage,
    /// the short-circuit retires the Idle batch and drops the resource
    /// *immediately* — UAF. Routing to `AdoptLastSubmitted` piggybacks
    /// the resource on the back submitted batch's fence; same-queue
    /// FIFO means it signals after every prior fence.
    ///
    /// **Poisoned-batch handling**: a `Poisoned` current batch is NOT
    /// a host for adoption (`PaintBatch::Drop` for Poisoned is a no-op,
    /// so any adopted resource would leak). If there are no submitted
    /// predecessors either, the resource releases synchronously.
    /// Otherwise the production `defer_resource_release` discards the
    /// Poisoned batch first and routes to `AdoptLastSubmitted`.
    #[must_use]
    pub fn defer_resource_release_decision_for(
        has_submitted: bool,
        current_state: Option<BatchState>,
    ) -> DeferDecision {
        match current_state {
            Some(BatchState::Recording | BatchState::Closed) => DeferDecision::AdoptOpenRecording,
            // None / Idle / Poisoned: no CB recording or already-
            // dead-on-arrival. Defer to the back of the submitted
            // queue if there's GPU work in flight; otherwise nothing
            // can reference the resource.
            _ => {
                if has_submitted {
                    DeferDecision::AdoptLastSubmitted
                } else {
                    DeferDecision::Synchronous
                }
            }
        }
    }

    /// Thin wrapper that snapshots `self`'s state and delegates to
    /// the pure helper. Production callers use this; tests use the
    /// pure form directly.
    #[must_use]
    pub fn defer_resource_release_decision(&self) -> DeferDecision {
        Self::defer_resource_release_decision_for(
            !self.submitted_paint_batches.is_empty(),
            self.current_paint_batch.as_ref().map(PaintBatch::state),
        )
    }

    /// Defer-release the boxed `BatchResource`: keep the resource
    /// alive until every in-flight CB that could reference it has
    /// retired, then drop it. The decision routes one of three ways
    /// per `defer_resource_release_decision_for`:
    ///
    /// - `Synchronous`: nothing in flight, no recorded CB → release
    ///   directly on the calling thread.
    /// - `AdoptOpenRecording`: open batch has a CB → adopt into it;
    ///   the batch's eventual submit signals after every prior
    ///   submission's fence (same-queue FIFO), so the adopted
    ///   resource outlives every reference.
    /// - `AdoptLastSubmitted`: no usable open batch but submitted
    ///   batches exist → adopt into the most-recently-submitted
    ///   batch (back of `submitted_paint_batches`). Its fence is
    ///   the latest pending fence on the queue; when it signals,
    ///   every older submission has signaled too. The single-
    ///   threaded core loop guarantees no race with
    ///   `poll_retired_paint_batches`.
    ///
    /// **Why we don't lazy-open an Idle batch here anymore.**
    /// Previous behavior was to call `open_batch` for the no-open-
    /// batch path and adopt into the freshly-opened Idle batch. The
    /// problem: a subsequent `close_and_submit{,_async}` on an Idle
    /// batch short-circuits to `retire_now` without going through
    /// the queue, releasing the adopted resource *immediately* —
    /// while submitted predecessors are still in flight. On bee
    /// (RDNA2 / RADV) this surfaced as an amdgpu PERMISSION_FAULTS
    /// page fault when the shader sampled a freed VkImage. Adopting
    /// into the back submitted batch instead piggybacks the resource
    /// on a fence that *will* be observed before retirement.
    ///
    /// `vk` and `pool` are no longer strictly needed (no lazy-open
    /// branch), but retained in the signature for backward
    /// compatibility with the existing five call sites. `vk` is
    /// still used by the Synchronous branch.
    pub fn defer_resource_release(
        &mut self,
        vk: Arc<VkContext>,
        _pool: vk::CommandPool,
        resource: Box<dyn paint_batch::BatchResource>,
    ) {
        // Discard a Poisoned current batch before deciding. A
        // Poisoned batch's Drop is a no-op (the leak-on-error
        // contract), so adopting into it would silently leak the
        // resource. After discard, the decision tree sees
        // current_state=None and routes correctly: Synchronous if no
        // submitted predecessors, AdoptLastSubmitted otherwise.
        if let Some(b) = self.current_paint_batch.as_ref()
            && b.state() == BatchState::Poisoned
        {
            self.current_paint_batch = None;
        }
        match self.defer_resource_release_decision() {
            DeferDecision::Synchronous => {
                resource.release(&vk);
            }
            DeferDecision::AdoptOpenRecording => {
                self.current_paint_batch
                    .as_mut()
                    .expect("AdoptOpenRecording implies Some(Recording|Closed)")
                    .adopt(resource);
            }
            DeferDecision::AdoptLastSubmitted => {
                self.submitted_paint_batches
                    .back_mut()
                    .expect("AdoptLastSubmitted implies non-empty queue")
                    .adopt(resource);
            }
        }
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

    // ============ Pure decision-helper tests (codex round-2 P2). ============
    // The pure form takes (has_submitted, current_state) explicitly
    // so we exercise all 12 combinations without constructing a
    // PaintBatch.

    #[test]
    fn defer_decision_empty_is_synchronous() {
        assert_eq!(
            RenderScheduler::defer_resource_release_decision_for(false, None),
            DeferDecision::Synchronous
        );
    }

    #[test]
    fn defer_decision_submitted_only_is_last_submitted() {
        // No open batch, submitted in flight → adopt into the back of
        // the submitted queue (the fix for the bee FreePicture UAF).
        assert_eq!(
            RenderScheduler::defer_resource_release_decision_for(true, None),
            DeferDecision::AdoptLastSubmitted
        );
    }

    #[test]
    fn defer_decision_current_idle_no_submitted_is_synchronous() {
        // Idle batch with no CB and no submitted predecessors: no
        // in-flight GPU work that could reference the resource.
        // Release directly. (Pre-fix this answered AdoptOpen and
        // lazy-host the Idle, which was harmless when no submitted
        // existed.)
        assert_eq!(
            RenderScheduler::defer_resource_release_decision_for(false, Some(BatchState::Idle)),
            DeferDecision::Synchronous
        );
    }

    #[test]
    fn defer_decision_current_idle_with_submitted_is_last_submitted() {
        // The load-bearing fix: open Idle (no CB) + submitted in
        // flight → adopt into back submitted batch, NOT into Idle.
        // Pre-fix this returned AdoptOpen which short-circuited Idle
        // and released the resource while submitted predecessors
        // still referenced it (UAF, amdgpu PERMISSION_FAULTS).
        assert_eq!(
            RenderScheduler::defer_resource_release_decision_for(true, Some(BatchState::Idle)),
            DeferDecision::AdoptLastSubmitted
        );
    }

    #[test]
    fn defer_decision_current_recording_is_open_recording() {
        assert_eq!(
            RenderScheduler::defer_resource_release_decision_for(
                false,
                Some(BatchState::Recording)
            ),
            DeferDecision::AdoptOpenRecording
        );
    }

    #[test]
    fn defer_decision_current_recording_with_submitted_is_open_recording() {
        // Open Recording always wins over submitted: its fence
        // signals after every submitted fence (same-queue FIFO).
        assert_eq!(
            RenderScheduler::defer_resource_release_decision_for(true, Some(BatchState::Recording)),
            DeferDecision::AdoptOpenRecording
        );
    }

    #[test]
    fn defer_decision_current_closed_is_open_recording() {
        assert_eq!(
            RenderScheduler::defer_resource_release_decision_for(false, Some(BatchState::Closed)),
            DeferDecision::AdoptOpenRecording
        );
    }

    #[test]
    fn defer_decision_current_poisoned_no_submitted_is_synchronous() {
        // A Poisoned current batch is NOT a valid adoption host
        // (its Drop is a no-op). With no submitted predecessors,
        // the resource must release synchronously.
        assert_eq!(
            RenderScheduler::defer_resource_release_decision_for(false, Some(BatchState::Poisoned)),
            DeferDecision::Synchronous
        );
    }

    #[test]
    fn defer_decision_current_poisoned_with_submitted_is_last_submitted() {
        // Predecessors might reference the resource → adopt into the
        // back submitted batch. The production fn discards the
        // Poisoned batch first, but this pure helper sees the raw
        // snapshot and answers identically because Poisoned routes
        // to the same arm as None / Idle.
        assert_eq!(
            RenderScheduler::defer_resource_release_decision_for(true, Some(BatchState::Poisoned)),
            DeferDecision::AdoptLastSubmitted
        );
    }

    // Retired/Submitted as a current_state is a state-machine
    // invariant violation (current_paint_batch is never observed in
    // those states at a defer-release call site — Submitted lives
    // in submitted_paint_batches, Retired is short-lived inside
    // close_and_submit). Test for completeness; falls into the
    // "no usable open batch" arm and routes by has_submitted.
    #[test]
    fn defer_decision_current_submitted_no_submitted_is_synchronous() {
        assert_eq!(
            RenderScheduler::defer_resource_release_decision_for(
                false,
                Some(BatchState::Submitted)
            ),
            DeferDecision::Synchronous
        );
    }

    #[test]
    fn defer_decision_current_submitted_with_submitted_is_last_submitted() {
        assert_eq!(
            RenderScheduler::defer_resource_release_decision_for(true, Some(BatchState::Submitted)),
            DeferDecision::AdoptLastSubmitted
        );
    }

    #[test]
    fn defer_decision_current_retired_no_submitted_is_synchronous() {
        assert_eq!(
            RenderScheduler::defer_resource_release_decision_for(false, Some(BatchState::Retired)),
            DeferDecision::Synchronous
        );
    }

    #[test]
    fn defer_decision_current_retired_with_submitted_is_last_submitted() {
        assert_eq!(
            RenderScheduler::defer_resource_release_decision_for(true, Some(BatchState::Retired)),
            DeferDecision::AdoptLastSubmitted
        );
    }

    // Empty-scheduler convenience test that goes through the
    // self-wrapping form. Verifies the wrapper composes correctly
    // with the pure helper.
    #[test]
    fn defer_decision_is_synchronous_with_empty_scheduler() {
        let s = RenderScheduler::new();
        assert_eq!(
            s.defer_resource_release_decision(),
            DeferDecision::Synchronous
        );
    }

    // Adopt-branch downstream behavior (PaintBatch::adopt +
    // retire_now release) is covered by binary integration tests +
    // hardware smoke — those require a real Vulkan context.
}
