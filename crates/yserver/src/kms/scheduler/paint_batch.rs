//! A frame's accumulated paint work.
//!
//! Phase 3A: the batch is the owner of every resource referenced by
//! commands recorded into it. Recorders append via
//! `KmsBackend::record_paint_op`; uploads go through
//! `BatchUploadArena` (T2); descriptors through `BatchDescriptorArena`
//! (T3). At close (`submit_and_wait`), the CB is ended, submitted,
//! and — until phase 4 swaps the wait for a timeline-semaphore signal
//! — the queue is idle-waited. After the wait, `holders == 0` and the
//! batch transitions to `Retired`, releasing all `BatchResource`s.
//!
//! See `docs/superpowers/specs/2026-05-12-rendering-rearchitecture-hld.md`
//! for the destination shape this implements.
//!
//! ## Layout-state policy
//!
//! Phase 3A: poison-and-discard. A failed `append` poisons the
//! batch; the CB is freed without submit; `BatchResource`s
//! release. Phase 3A relies on the recorder-side late-mutation
//! invariant — every `set_current_layout` follows the recorder's
//! last fallible operation. If that invariant holds for a
//! recorder, CPU/GPU layout state stays consistent even when the
//! batch is poisoned (no GPU work ran ⇒ GPU state unchanged; CPU
//! mutation never happened ⇒ CPU state unchanged).
//!
//! Audited 2026-05-13 (load-bearing for 3B):
//!   - fill::record_fill_rectangles: late ✓
//!   - fill::record_logic_fill: late ✓
//!   - copy::record_copy_area_distinct: late ✓
//!   - copy::record_copy_area_same: late ✓
//!
//! Deferred to their respective tranches (3C/3D BLOCK on these
//! audits passing — or on implementing the touched-drawable
//! invalidation hook described in `paint_batch.rs`'s module doc):
//!   - copy::record_copy_area_same_overlap (3D)
//!   - image::record_put_image (3C)
//!   - render::record_render_composite (3D)
//!   - text::record_text_run (3D)
//!   - traps::record_* (3D)

// The public methods on PaintBatch have extensive inline documentation that
// covers their error and panic behaviour; formal `# Errors` / `# Panics`
// doc sections would be redundant noise at this stage.
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

use std::sync::Arc;

use ash::vk;

use crate::kms::{
    scheduler::{
        batch_descriptor_arena::BatchDescriptorArena, batch_upload_arena::BatchUploadArena,
    },
    vk::device::VkContext,
};

/// Why a paint batch flush was requested. Passed to
/// `KmsBackend::flush_if_needed` so phase 4 can distinguish between
/// "submit + signal" and "submit + wait" flush policies.
///
/// Strict reasons (`Readback`, `ExternalSync`, `ProtocolBarrier`) surface
/// a `Poisoned` or `InvalidState` batch as an error — the caller's
/// completion guarantee cannot be honoured.  Best-effort reasons
/// (`VisibleComposite`, `SizeLimit`, `LatencyLimit`, `Shutdown`) swallow
/// those conditions.
#[derive(Debug, Clone, Copy)]
pub enum BatchFlushReason {
    /// The composite cycle is about to sample mirrors this batch
    /// wrote. Fires at the top of `composite_and_flip`'s per-output
    /// loop.
    VisibleComposite,
    /// A synchronous-reply request needs CPU-visible pixels.
    /// GetImage, host-readback, MIT-SHM GetImage.
    Readback,
    /// An external sync export is pending. DRI3 Present fence
    /// handoff, SYNC extension fence trigger.
    ExternalSync,
    /// An explicit protocol barrier requested it. The phase-3B
    /// `KmsBackend::run_legacy_paint_op` wrapper uses this reason
    /// to flush the batch before any paint op still on
    /// `run_one_shot_op`, so a migrated recorder's CPU-side layout
    /// mutation has GPU-completed before the legacy op reads it.
    ProtocolBarrier,
    /// The batch hit a size/op-count limit. Not load-bearing in
    /// phase 3 (no limit enforced); reserved for phase 4+.
    SizeLimit,
    /// The batch hit a latency limit. Same.
    LatencyLimit,
    /// Server shutdown / hot teardown. Forces close before any
    /// resource is freed by other paths.
    Shutdown,
}

impl std::fmt::Display for BatchFlushReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            BatchFlushReason::VisibleComposite => "VisibleComposite",
            BatchFlushReason::Readback => "Readback",
            BatchFlushReason::ExternalSync => "ExternalSync",
            BatchFlushReason::ProtocolBarrier => "ProtocolBarrier",
            BatchFlushReason::SizeLimit => "SizeLimit",
            BatchFlushReason::LatencyLimit => "LatencyLimit",
            BatchFlushReason::Shutdown => "Shutdown",
        };
        f.write_str(name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchState {
    Idle,
    Recording,
    Closed,
    Submitted,
    Retired,
    Poisoned,
}

#[derive(Debug, thiserror::Error)]
pub enum BatchError {
    #[error("vulkan: {0:?}")]
    Vk(vk::Result),
    #[error("paint batch is {0:?}; operation invalid in this state")]
    InvalidState(BatchState),
    #[error("paint batch is poisoned; discard and start a new one")]
    Poisoned,
}

impl From<vk::Result> for BatchError {
    fn from(r: vk::Result) -> Self {
        BatchError::Vk(r)
    }
}

/// A resource owned by a `PaintBatch` whose GPU lifetime equals the
/// batch's. Released at `Retired` (or `Poisoned`) transition.
///
/// Implementors must be safe to release on the thread that owns the
/// batch — phase 6.8's single-core invariant means that's the
/// backend thread, which holds the live `&VkContext`.
///
/// `Debug` is required so `PaintBatch` can `#[derive(Debug)]` —
/// implementors typically derive `Debug` themselves and emit just
/// the variant name (the Vulkan handles inside are not interesting
/// to debug-print).
pub trait BatchResource: Send + std::fmt::Debug {
    fn release(self: Box<Self>, vk: &VkContext);
}

pub struct PaintBatch {
    pub frame_id: u64,
    /// Outputs that are **candidates** to composite from this
    /// batch (passed the per-output damage gate at close time).
    /// Populated by `RenderScheduler::close_and_submit`.
    ///
    /// Phase 3 records this for shape and audit logs only — it is
    /// NOT the holder list. The authoritative phase-4 holder list
    /// is built by `OutputFrame::new` after a successful composite
    /// submit (BO acquired, descriptor pool slot acquired, fence
    /// armed). Candidate ≠ holder because pending-flip / BO-availability
    /// gates inside `composite_and_flip` can skip a candidate
    /// output and never produce an `OutputFrame` for it.
    pub dirty_outputs: Vec<usize>,
    pub state: BatchState,
    /// Number of `OutputFrame`s that have captured a dependency on
    /// this batch. Phase 3 leaves at 0 — the close-time `waitIdle`
    /// guarantees GPU retirement before any composite reads the
    /// mirrors. Phase 4 wires this up when composite waits on a
    /// timeline-semaphore signal instead.
    pub holders: u32,
    cb: Option<vk::CommandBuffer>,
    pool: vk::CommandPool,
    vk: Arc<VkContext>,
    retire_resources: Vec<Box<dyn BatchResource>>,
    upload_arena: Option<BatchUploadArena>,
    descriptor_arena: Option<BatchDescriptorArena>,
}

impl std::fmt::Debug for PaintBatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaintBatch")
            .field("frame_id", &self.frame_id)
            .field("dirty_outputs", &self.dirty_outputs)
            .field("state", &self.state)
            .field("holders", &self.holders)
            .finish_non_exhaustive()
    }
}

impl PaintBatch {
    #[must_use]
    pub fn new(frame_id: u64, vk: Arc<VkContext>, pool: vk::CommandPool) -> Self {
        Self {
            frame_id,
            dirty_outputs: Vec::new(),
            state: BatchState::Idle,
            holders: 0,
            cb: None,
            pool,
            vk,
            retire_resources: Vec::new(),
            upload_arena: None,
            descriptor_arena: None,
        }
    }

    #[must_use]
    pub fn state(&self) -> BatchState {
        self.state
    }

    /// Whether `append` would accept more work. False for any
    /// terminal state and for `Closed` / `Submitted` / `Retired`.
    #[must_use]
    pub fn is_recording_open(&self) -> bool {
        matches!(self.state, BatchState::Idle | BatchState::Recording)
    }

    /// Adopt `resource` for release at `Retired` / `Poisoned`.
    /// Used by per-batch descriptor pool, etc.
    pub fn adopt(&mut self, resource: Box<dyn BatchResource>) {
        debug_assert!(
            !matches!(self.state, BatchState::Retired | BatchState::Poisoned),
            "PaintBatch::adopt called on terminal batch"
        );
        self.retire_resources.push(resource);
    }

    /// Mutable reference to the per-batch upload arena, lazy-init
    /// on first call.
    pub fn upload_arena_mut(&mut self) -> &mut BatchUploadArena {
        if self.upload_arena.is_none() {
            self.upload_arena = Some(BatchUploadArena::new(self.vk.clone()));
        }
        self.upload_arena.as_mut().unwrap()
    }

    /// Mutable reference to the per-batch descriptor arena, lazy-init
    /// on first call.
    pub fn descriptor_arena_mut(&mut self) -> &mut BatchDescriptorArena {
        if self.descriptor_arena.is_none() {
            self.descriptor_arena = Some(BatchDescriptorArena::new(self.vk.clone()));
        }
        self.descriptor_arena.as_mut().unwrap()
    }

    /// Run `record` against the batch's CB. Lazy-allocates and
    /// begins recording on first call. On error the batch is
    /// **poisoned** and discarded — the caller's pending work for
    /// this frame is lost, and any drawables it touched must bump
    /// their dirty generation before the next composite (handled
    /// by the per-call-site `Drop` of `PaintBatchGuard` introduced
    /// in 3A T4).
    pub fn append<F>(&mut self, record: F) -> Result<(), BatchError>
    where
        F: FnOnce(&VkContext, vk::CommandBuffer) -> Result<(), vk::Result>,
    {
        match self.state {
            BatchState::Poisoned => return Err(BatchError::Poisoned),
            BatchState::Closed | BatchState::Submitted | BatchState::Retired => {
                return Err(BatchError::InvalidState(self.state));
            }
            BatchState::Idle => self.begin_recording()?,
            BatchState::Recording => {}
        }
        let cb = self.cb.expect("Recording state implies cb is Some");
        match record(&self.vk, cb) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.poison();
                Err(BatchError::Vk(e))
            }
        }
    }

    fn begin_recording(&mut self) -> Result<(), BatchError> {
        debug_assert_eq!(self.state, BatchState::Idle);
        let alloc_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let cb = unsafe { self.vk.device.allocate_command_buffers(&alloc_info)?[0] };
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        if let Err(e) = unsafe { self.vk.device.begin_command_buffer(cb, &begin) } {
            unsafe { self.vk.device.free_command_buffers(self.pool, &[cb]) };
            return Err(BatchError::Vk(e));
        }
        self.cb = Some(cb);
        self.state = BatchState::Recording;
        Ok(())
    }

    /// Public entry into `begin_recording` for callers that build
    /// their own append loop on top of `append` (e.g.
    /// `KmsBackend::record_paint_batch_op`, which needs to hand the
    /// caller's closure a `&mut PaintBatch` plus the CB and so
    /// can't use `append` directly).
    ///
    /// Errors out if `state != Idle` — callers must check
    /// `state()` before calling.
    pub fn begin_recording_explicit(&mut self) -> Result<(), BatchError> {
        if self.state != BatchState::Idle {
            return Err(BatchError::InvalidState(self.state));
        }
        self.begin_recording()
    }

    /// Current command buffer, or `None` if the batch is not in
    /// `Recording`. `record_paint_batch_op` uses this after
    /// `begin_recording_explicit` to thread the CB into its
    /// closure alongside `&mut self`.
    #[must_use]
    pub fn command_buffer(&self) -> Option<vk::CommandBuffer> {
        if self.state == BatchState::Recording {
            self.cb
        } else {
            None
        }
    }

    /// Poison from an external caller (e.g. when a closure passed
    /// to `record_paint_batch_op` returned an error). Equivalent
    /// to the internal `poison()` but pub-visible.
    pub fn poison_external(&mut self) {
        self.poison();
    }

    /// Move Recording → Closed by ending the CB. No-op on Idle
    /// (transitions to Closed with no CB). Invalid on terminal
    /// states.
    pub fn close(&mut self) -> Result<(), BatchError> {
        match self.state {
            BatchState::Idle => {
                self.state = BatchState::Closed;
                Ok(())
            }
            BatchState::Recording => {
                let cb = self.cb.expect("Recording state implies cb is Some");
                unsafe { self.vk.device.end_command_buffer(cb)? };
                self.state = BatchState::Closed;
                Ok(())
            }
            other => Err(BatchError::InvalidState(other)),
        }
    }

    /// Submit + idle-wait + retire. Phase 3 collapses
    /// Closed→Submitted→Retired into this one call. Phase 4
    /// splits them: submit returns immediately, retirement is
    /// driven by `release_holder`.
    ///
    /// On Idle (no CB allocated): no submit; transitions directly
    /// to Retired. On Poisoned: returns `BatchError::Poisoned`
    /// without touching the queue.
    ///
    /// **Three distinct failure paths**, with different retirement
    /// semantics — DO NOT collapse them:
    ///
    /// 1. **Submit fails** (`queue_submit2` returns Err): the CB
    ///    never entered the queue. Free the CB, retire resources,
    ///    return the error.
    /// 2. **Wait fails** (`queue_submit2` Ok, `queue_wait_idle`
    ///    Err): the CB IS in flight or the device is lost. The GPU
    ///    may still be reading our resources. We must NOT free
    ///    the CB and must NOT call `BatchResource::release` —
    ///    those Vulkan handles are abandoned until device
    ///    destruction. The batch stays in `Submitted` forever
    ///    (its `Drop` honours the same leak; see `Drop` impl).
    ///
    ///    **This is not a recoverable state.** Callers that get
    ///    `BatchError::Vk` from `submit_and_wait` MUST treat the
    ///    KMS renderer as failed: tear the backend down (which
    ///    triggers `VkContext::Drop` → global `device_wait_idle`
    ///    if the device is still responsive, otherwise driver
    ///    cleanup at process exit) or mark it permanently
    ///    disabled. Continuing to call `record_paint_op` /
    ///    `flush_if_needed` after a leaked Submitted batch is
    ///    not a supported steady state — it produces more
    ///    abandoned CBs each cycle.
    ///
    /// 3. **Both succeed**: free CB, retire resources, return Ok.
    pub fn submit_and_wait(&mut self) -> Result<(), BatchError> {
        match self.state {
            BatchState::Poisoned => return Err(BatchError::Poisoned),
            BatchState::Retired => return Err(BatchError::InvalidState(BatchState::Retired)),
            BatchState::Submitted => return Err(BatchError::InvalidState(BatchState::Submitted)),
            BatchState::Idle => {
                self.state = BatchState::Closed;
                self.retire_now();
                return Ok(());
            }
            BatchState::Recording => self.close()?,
            BatchState::Closed => {}
        }
        let cb = self.cb.expect("Closed implies cb was allocated");
        let cb_info = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
        let submit = [vk::SubmitInfo2::default().command_buffer_infos(&cb_info)];

        // Path 1: submit fails. CB never queued; safe to free + retire.
        if let Err(e) = unsafe {
            self.vk
                .device
                .queue_submit2(self.vk.graphics_queue, &submit, vk::Fence::null())
        } {
            unsafe { self.vk.device.free_command_buffers(self.pool, &[cb]) };
            self.cb = None;
            self.state = BatchState::Closed; // back to a poisonable state
            self.poison();
            return Err(BatchError::Vk(e));
        }

        // Now Submitted — CB is in flight.
        self.state = BatchState::Submitted;

        // Path 2 / 3: wait. On wait failure the CB and our resources
        // may still be referenced by the GPU. Leak rather than UB.
        match unsafe { self.vk.device.queue_wait_idle(self.vk.graphics_queue) } {
            Ok(()) => {
                // Path 3: clean retirement.
                unsafe { self.vk.device.free_command_buffers(self.pool, &[cb]) };
                self.cb = None;
                self.retire_now();
                Ok(())
            }
            Err(e) => {
                // Path 2: device-lost or similar. Intentionally do
                // NOT free the CB, do NOT release retire_resources
                // — those handles are abandoned. The batch stays
                // in `Submitted` forever; its `Drop` does nothing.
                //
                // Upper layers MUST treat this as a fatal
                // KMS-renderer condition (see method doc above).
                log::error!(
                    "PaintBatch::submit_and_wait: queue_wait_idle failed ({e:?}); \
                     CB and resources abandoned. KMS renderer is in an \
                     unrecoverable state — caller MUST tear down or disable."
                );
                Err(BatchError::Vk(e))
            }
        }
    }

    /// Drop a holder reference. Transitions to Retired when
    /// `holders == 0 && state == Submitted`. Phase 4 wire-up;
    /// phase 3 is dead code today but landed for shape.
    pub fn release_holder(&mut self) {
        debug_assert!(self.holders > 0, "release_holder underflow");
        self.holders = self.holders.saturating_sub(1);
        if self.holders == 0 && self.state == BatchState::Submitted {
            self.retire_now();
        }
    }

    /// Increment the holder refcount. Phase 4 wire-up.
    pub fn acquire_holder(&mut self) {
        debug_assert!(
            !matches!(self.state, BatchState::Retired | BatchState::Poisoned),
            "acquire_holder on terminal batch"
        );
        self.holders += 1;
    }

    /// Internal: move to Retired and release all `BatchResource`s.
    fn retire_now(&mut self) {
        debug_assert!(
            matches!(self.state, BatchState::Closed | BatchState::Submitted),
            "retire_now from {:?}",
            self.state
        );
        if let Some(arena) = self.upload_arena.take() {
            Box::new(arena).release(&self.vk);
        }
        if let Some(arena) = self.descriptor_arena.take() {
            Box::new(arena).release(&self.vk);
        }
        for r in self.retire_resources.drain(..) {
            r.release(&self.vk);
        }
        self.state = BatchState::Retired;
    }

    /// Internal: discard the batch without submit. CB (if any)
    /// is freed without `end_command_buffer` — Vulkan permits
    /// freeing a recording CB that was never submitted. All
    /// `retire_resources` are released.
    fn poison(&mut self) {
        if let Some(cb) = self.cb.take() {
            // If the CB is in a recording state (begin_command_buffer was
            // called but end_command_buffer was not), some Vulkan drivers
            // (e.g. the amdgpu radeon driver) crash when attempting to
            // free or reset an open CB. Skip the explicit free for
            // Recording-state batches — the CB will be released when the
            // command pool itself is destroyed (OpsCommandPool::Drop calls
            // queue_wait_idle + destroy_command_pool, which implicitly
            // frees all CBs in the pool, including recording ones).
            if self.state != BatchState::Recording {
                unsafe { self.vk.device.free_command_buffers(self.pool, &[cb]) };
            }
        }
        if let Some(arena) = self.upload_arena.take() {
            Box::new(arena).release(&self.vk);
        }
        if let Some(arena) = self.descriptor_arena.take() {
            Box::new(arena).release(&self.vk);
        }
        for r in self.retire_resources.drain(..) {
            r.release(&self.vk);
        }
        self.state = BatchState::Poisoned;
    }
}

impl Drop for PaintBatch {
    fn drop(&mut self) {
        match self.state {
            // Terminal: nothing to do.
            BatchState::Retired | BatchState::Poisoned => {}
            // CB is in flight from a wait-failure (device-lost)
            // path. Resources are intentionally abandoned —
            // touching the CB or memory here would be UB. The
            // KMS renderer should already be in teardown by the
            // time this Drop runs.
            BatchState::Submitted => {
                log::error!(
                    "PaintBatch::drop while Submitted — abandoned resources \
                     (CB + arenas + descriptor pools). KMS renderer is in an \
                     unrecoverable state."
                );
            }
            // Idle / Recording / Closed: nothing on the GPU yet.
            // Safe to poison + free.
            BatchState::Idle | BatchState::Recording | BatchState::Closed => {
                self.poison();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_state_machine_transitions_are_typed() {
        let a = BatchState::Idle;
        let b = a;
        assert_eq!(a, b);
        assert_ne!(BatchState::Closed, BatchState::Submitted);
        assert_ne!(BatchState::Submitted, BatchState::Retired);
        assert_ne!(BatchState::Retired, BatchState::Poisoned);
    }

    // VkContext-backed lifecycle tests (Idle→Closed→Retired empty
    // batch; double-submit detection; double-retire detection;
    // append-after-close rejection) live as hardware smoke under
    // 3A T5 once `flush_if_needed` is the entry point. The hand-
    // unit-testable surface here is the `BatchState` discriminants
    // and the error enum.

    #[test]
    fn batch_error_displays_state() {
        let e = BatchError::InvalidState(BatchState::Submitted);
        let s = format!("{e}");
        assert!(s.contains("Submitted"), "got: {s}");
    }

    #[test]
    #[ignore = "requires VkContext mock harness (not available in unit tests); \
                T5 hardware smoke covers this via flush_if_needed"]
    fn append_failure_poisons_batch() {
        // (pseudo-Vulkan harness; if the test infra can't construct
        // a VkContext, this becomes a hardware smoke step in T5.)
        //
        // 1. Open a batch.
        // 2. Call append with a closure that returns
        //    vk::Result::ERROR_DEVICE_LOST.
        // 3. Assert batch.state() == BatchState::Poisoned.
        // 4. Assert a second append on the same batch returns
        //    BatchError::Poisoned.
    }
}
