# Rendering re-architecture — phase 3 implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

Date: 2026-05-12
Status: reconciled — supersedes both the original `phase3.md` (insufficient resource lifetimes) and `phase3-codex.md` (too high-level to execute). Adopts codex's 3A→3B→3C→3D structure with writing-plans-skill-compliant detail on 3A; 3B–3D are sketched as briefs and **must be expanded after 3A lands** against the real shape of the ownership APIs.

**Goal:** Establish batch-owned resource lifetimes in `PaintBatch` (CB, host-upload arena, descriptor arena, retirement refcount, flush-reason API) so subsequent migrations can move recorder work off `run_one_shot_op` without staging-buffer / descriptor-set / scratch-image aliasing across un-submitted ops. A close-time `vkQueueWaitIdle` is allowed to remain as a temporary submit/retire bridge — phase 4 swaps it for a timeline-semaphore signal.

**Architecture:** `PaintBatch` becomes the owner of every resource referenced by commands recorded into it, with a `Idle → Recording → Closed → Submitted → Retired` state machine plus a `Poisoned` terminal for failed appends. Three new collaborator types — `BatchUploadArena` (host-visible chunked allocator), `BatchDescriptorArena` (per-batch descriptor pool with chunk growth), and a `BatchResource` trait for retire-time cleanup — sit on `PaintBatch`. `BatchFlushReason` enumerates why the batch closes; `KmsBackend::flush_if_needed(reason)` is the single boundary callers cross. Recorder migration is **deliberately deferred**: phase 3 lands the destination model, phase 3B–3D migrate audited families in three correctness tranches, and phase 4 retires the close-time waitIdle.

**Tech Stack:** Rust 2021, `ash` for Vulkan, no new external dependencies. Modifies `kms/scheduler/paint_batch.rs` + adjacent scheduler files; adds new `kms/scheduler/batch_upload_arena.rs` and `kms/scheduler/batch_descriptor_arena.rs`; minimal touch to `kms/backend.rs` and `kms/vk/render_pipeline.rs` for the descriptor-ownership audit.

**Reference:**
- `docs/superpowers/specs/2026-05-12-rendering-rearchitecture-hld.md` — HLD; especially the `PaintBatch` section ("Shape of the new code") and "Required invariants" — these spell out batch-owned scratch arena and the flush-reason rule.
- `docs/superpowers/plans/2026-05-12-rendering-rearchitecture-phase1-results.md` / `…-phase2-results.md` — predecessors. Phase 2 caught a real ring-sizing bug late (memory `feedback_ring_sizing_vs_pipeline_depth`); pin chunk/pool sizing against pipeline depth at design time, not at smoke time.
- `docs/superpowers/specs/2026-05-12-waitidle-catalogue.md` — every `vkQueueWaitIdle` site, classified. Phase 3 doesn't retire any per-op site; phase 3B–3D start to.
- `docs/superpowers/plans/2026-05-12-rendering-rearchitecture-phase3-codex.md` — codex's competing brief; this plan adopts its phase split and acceptance shape verbatim.

---

## Design rule (load-bearing)

**`PaintBatch` owns every resource referenced by commands it records.** If a command will read a buffer, descriptor set, scratch image, image view, sampler, host memory range, or any temporary allocation **after `record_paint_op` returns**, that object must either:

- be owned by the batch and freed at the batch's `Retired` transition, or
- be proven immutable and longer-lived than every batch that can reference it (e.g. pipeline objects, sampler objects, render-pass-compatible image views on persistent drawables).

Anything else stays on `run_one_shot_op` and is not migrated yet.

The HLD invariant — "any request that needs CPU-visible pixels or externally-visible GPU completion must force the current `PaintBatch` to submit before returning" — is codified in 3A T5's `flush_if_needed(reason)`.

---

## Pre-task: global checks

Every task ends with:

```bash
cargo +nightly fmt
cargo clippy
cargo test
```

Use explicit `git add <file>` per file — **do NOT use `git commit -am`** (phase 1's `3184fd9` swept in `docs/status.md`).

**Pedantic clippy: NOT enforced for phase 3.** Run `cargo clippy` without `-W clippy::pedantic`. Don't chase `#[must_use]`, `missing_errors_doc`, `doc_markdown`, etc.

## Plan bugs caught during 3A execution (apply to 3B+ implementations)

Codified after 3A landed; each was a real failure-to-compile or quality issue caught in review:

1. **`#[derive(Debug)]` on any type holding `Arc<VkContext>` will NOT compile.** `VkContext` (in `kms/vk/device.rs`) does not implement `Debug` — its Vulkan handles are opaque. Use a manual `Debug` impl with `finish_non_exhaustive()`, matching the `CompositePoolRing` precedent. Example from `BatchUploadArena`:

   ```rust
   impl std::fmt::Debug for BatchUploadArena {
       fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
           f.debug_struct("BatchUploadArena")
               .field("chunks", &self.chunks.len())
               .finish_non_exhaustive()
       }
   }
   ```

   The spec's `#[derive(Debug)]` annotations in 3A T1/T2/T3 source listings are wrong; treat them as "needs manual impl."

2. **`unsafe impl Send` requires a `// SAFETY:` comment at the impl site.** The phase-6.8 single-core invariant is documented in `BatchResource`'s trait doc but Rust convention wants the justification at every `unsafe impl` too. Pattern:

   ```rust
   // SAFETY: The KMS backend's single-threaded-core invariant
   // (phase 6.8) guarantees these are never moved across threads.
   unsafe impl Send for ... {}
   ```

3. **3A T1's `dirty_outputs` local in `composite_and_flip` is dead after T5.** T1 introduces it for `close_and_submit(dirty_outputs)`; T5 replaces that call with `flush_if_needed(VisibleComposite)` which rebuilds the vec internally. T5 must delete the now-unused local — the spec doesn't call it out explicitly. Caught at T5 review.

## Out of scope (deferred)

- Removing the close-time `vkQueueWaitIdle` — phase 4.
- `GetImage` targeted `VkFence` — phase 5.
- KMS sync-file / `OUT_FENCE_PTR` rework beyond phase 2's behaviour.
- Damage-clipped composite (`FB_DAMAGE_CLIPS`).
- Global rewrite of every `run_one_shot_op` site — phase 3B–3D migrate audited families incrementally; many will remain on `run_one_shot_op` at phase-3 close.

---

# Phase 3A — batch resource ownership

Five tasks. Each lands a piece of the destination model; together they let phase 3B start migrating audited families safely.

## Task 1: `PaintBatch` state machine + retirement refcount + `BatchResource` trait

**Files:**
- Modify: `crates/yserver/src/kms/scheduler/paint_batch.rs`
- Modify: `crates/yserver/src/kms/scheduler/mod.rs`
- Modify: `crates/yserver/src/kms/scheduler/in_flight.rs` (no behaviour change; expose retire hook for `OutputFrame`)

**State machine:**

```
Idle ── append ──▶ Recording ── close ──▶ Closed ── submit ──▶ Submitted ── retire ──▶ Retired
  │                    │                                                                  ▲
  └─ close (no-op) ─┐  └─ poison ──▶ Poisoned ─────────────────────────── drop ──────────┘
                   ▼
                Retired (degenerate)
```

- **Idle**: no CB, no work, no resources. Default for a freshly opened batch.
- **Recording**: CB allocated and `begin_command_buffer` called; appends valid.
- **Closed**: `end_command_buffer` called; no more appends accepted; submit not yet issued. Phase 3 may transition Closed→Submitted immediately at close (close_and_submit); phase 4 separates them.
- **Submitted**: `vkQueueSubmit2` called; CB is in flight. `holders > 0` means at least one `OutputFrame` still depends on this batch's work being retired.
- **Retired**: every holder has signalled GPU retirement; batch resources are released via their `BatchResource::release` calls. Terminal.
- **Poisoned**: a recorder errored mid-append; the batch is discarded without submit. Terminal. Resources are released the same way as Retired.

In phase 3 the Closed→Submitted→Retired sequence is collapsed by the close-time `vkQueueWaitIdle`: by the time `submit_and_wait` returns, holders are 0 and the batch is immediately retired. The state machine API keeps the three states distinct so phase 4 can split them without ABI churn.

**Retirement refcount:** `holders: u32`. Incremented when an `OutputFrame` captures a reference to this batch (phase 4 wires this; phase 3 leaves it at 0). Decremented by `release_holder()` when an `OutputFrame` GPU-retires. The batch transitions to `Retired` when `state == Submitted && holders == 0`.

**`BatchResource` trait:** a small trait for retire-time cleanup. Examples: an `UploadArenaChunk`, a per-batch `VkDescriptorPool`, an unfreed `VkImage` allocated for a per-batch mask scratch. The batch holds `Vec<Box<dyn BatchResource>>` and drains it on retirement.

- [ ] **Step 1: Replace `paint_batch.rs` with the phase-3A shape**

```rust
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

use std::sync::Arc;

use ash::vk;

use crate::kms::vk::device::VkContext;

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

#[derive(Debug)]
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
    /// output and never produce an OutputFrame for it.
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
}

impl PaintBatch {
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
        }
    }

    pub fn state(&self) -> BatchState {
        self.state
    }

    /// Whether `append` would accept more work. False for any
    /// terminal state and for `Closed` / `Submitted` / `Retired`.
    pub fn is_recording_open(&self) -> bool {
        matches!(self.state, BatchState::Idle | BatchState::Recording)
    }

    /// Adopt `resource` for release at `Retired` / `Poisoned`.
    /// Used by `BatchUploadArena`, per-batch descriptor pool, etc.
    pub fn adopt(&mut self, resource: Box<dyn BatchResource>) {
        debug_assert!(
            !matches!(self.state, BatchState::Retired | BatchState::Poisoned),
            "PaintBatch::adopt called on terminal batch"
        );
        self.retire_resources.push(resource);
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
    /// to Retired. On Poisoned: returns BatchError::Poisoned
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
        for r in self.retire_resources.drain(..) {
            r.release(&self.vk);
        }
        self.state = BatchState::Retired;
    }

    /// Internal: discard the batch without submit. CB (if any)
    /// is freed without `end_command_buffer` — Vulkan permits
    /// freeing a recording CB that was never submitted. All
    /// retire_resources are released.
    fn poison(&mut self) {
        if let Some(cb) = self.cb.take() {
            unsafe { self.vk.device.free_command_buffers(self.pool, &[cb]) };
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
}
```

- [ ] **Step 2: Update `RenderScheduler` in `scheduler/mod.rs`**

```rust
use std::sync::Arc;

use ash::vk;

use crate::kms::vk::device::VkContext;

use self::{in_flight::InFlight, paint_batch::{BatchError, BatchState, PaintBatch}};

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
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a paint batch if one isn't already open. Returns the
    /// batch's `frame_id`. Phase 3 takes `vk` + `pool` so the batch
    /// can lazy-allocate a primary CB on first append.
    pub fn open_batch(
        &mut self,
        vk: Arc<VkContext>,
        pool: vk::CommandPool,
    ) -> u64 {
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
    pub fn close_and_submit(
        &mut self,
        dirty_outputs: Vec<usize>,
    ) -> Result<(), BatchError> {
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
    pub fn current_batch_state(&self) -> Option<BatchState> {
        self.current_paint_batch.as_ref().map(|b| b.state())
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
```

The three phase-1 tests (`open_batch_allocates_monotonic_frame_ids`, `open_batch_is_idempotent_within_a_cycle`, `close_batch_drops_current`) are removed — they required the no-arg constructor that's gone. Monotonic frame ids are covered by `InFlight::allocate_frame_id` tests (phase 1).

- [ ] **Step 3: Wire `composite_and_flip` to the new API**

In `crates/yserver/src/kms/backend.rs`, replace `let frame_id = self.scheduler.open_batch();` and `let _batch = self.scheduler.close_batch();` per the phase-2 wiring, with:

```rust
        // No Vulkan / no ops pool → composite path is unavailable.
        let vk_arc = match self.vk.as_ref() {
            Some(v) => v.clone(),
            None => {
                log::debug!("composite cycle: no Vulkan; skipping");
                return Ok(());
            }
        };
        let pool_handle = match self.ops_command_pool.as_ref() {
            Some(p) => p.handle(),
            None => {
                log::debug!("composite cycle: no ops pool; skipping");
                return Ok(());
            }
        };
        let frame_id = self.scheduler.open_batch(vk_arc, pool_handle);
        log::debug!(
            "composite cycle frame_id={} in_flight_len={}",
            frame_id,
            self.scheduler.in_flight.len()
        );

        // …existing top_levels / visible_per_output build…

        // Candidate outputs (passed the damage gate). Pass to
        // close_and_submit as `dirty_outputs` for audit logging
        // only — the actual phase-4 holder list is built by
        // `OutputFrame::new` AFTER `try_vulkan_composite_flip`
        // succeeds (which can still skip a candidate via the
        // flip-pending / BO-availability gates further down).
        let dirty_outputs: Vec<usize> = (0..self.outputs.len())
            .filter(|&i| self.outputs[i].damage.needs_composite())
            .collect();

        // Flush paint BEFORE the per-output composite loop. Until
        // 3B starts migrating recorders, the batch is Idle on every
        // cycle and this is a cheap state transition.
        if let Err(e) = self.scheduler.close_and_submit(dirty_outputs) {
            log::warn!("composite cycle: paint batch submit failed: {e}");
        }

        // Audit assertion (debug-only): no batch may be open during
        // the per-output composite loop. Recorders that would
        // auto-open one here would race with composite (the F2
        // finding from codex's review).
        debug_assert!(
            self.scheduler.current_batch_state().is_none(),
            "paint batch leaked into composite loop"
        );

        // …existing per-output composite loop…

        Ok(())
    }
```

The trailing `let _batch = self.scheduler.close_batch();` from phase 1/2 is **deleted**.

- [ ] **Step 4: Verify**

```bash
cargo +nightly fmt
cargo clippy
cargo test
```

Expected: clean. The scheduler unit tests pass. Backend tests that called `open_batch()` with no args are updated (T2 of phase 2 should have left exactly one such site).

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/scheduler/paint_batch.rs \
        crates/yserver/src/kms/scheduler/mod.rs \
        crates/yserver/src/kms/backend.rs
git commit -m "feat(scheduler): PaintBatch state machine + holders refcount + BatchResource trait"
```

---

## Task 2: `BatchUploadArena`

**Files:**
- Create: `crates/yserver/src/kms/scheduler/batch_upload_arena.rs`
- Modify: `crates/yserver/src/kms/scheduler/mod.rs` (module registration)
- Modify: `crates/yserver/src/kms/scheduler/paint_batch.rs` (storage + accessor)

A host-visible, append-only chunked allocator. Each allocation returns a stable `(vk::Buffer, offset, mapped_ptr, size)` quadruple that remains valid until the batch retires. Chunks are added on demand; the previous chunk's contents and offsets are never invalidated by a grow.

**Sizing rationale.** A worst-case batch is bounded by the per-frame work: at most one PutImage per top-level window (say 32 windows × 1080p × 4 bytes = ~256 MiB worst case, but in practice ≤ 10 MiB), plus glyph atlas uploads (≤ 1 MiB typical), plus mask scratch uploads (≤ 4 MiB worst case). A 1 MiB chunk fits typical batches in one allocation; growth doubles up to a 64 MiB cap per chunk. The arena allocates chunks as needed; there is no upper bound on **total** arena size per batch — phase-4 latency limits (3A T5) will catch pathological cases.

Per the phase-2 ring-sizing lesson (memory `feedback_ring_sizing_vs_pipeline_depth`): the depth of this allocator is **batch lifetime**, not per-op. Two PutImages in one batch must each get their own non-overlapping range — that's the explicit acceptance criterion.

- [ ] **Step 1: Create `batch_upload_arena.rs`**

```rust
//! Host-visible, append-only upload arena owned by `PaintBatch`.
//!
//! Returns stable `(buffer, offset, mapped_ptr, size)` quadruples
//! that remain valid until the batch retires. Chunked: a single
//! buffer would either be wastefully large for small batches or
//! invalidate offsets on grow. Per-chunk allocation is bumped from
//! `current_offset`; new chunks are added when the active chunk
//! can't fit the requested size.
//!
//! Owned by `PaintBatch`. Released at batch retirement (Retired or
//! Poisoned) via the `BatchResource` trait — each chunk's
//! `VkBuffer + VkDeviceMemory + mapping` is destroyed.

use std::{ptr::NonNull, sync::Arc};

use ash::vk;

use crate::kms::scheduler::paint_batch::BatchResource;
use crate::kms::vk::device::VkContext;

#[derive(Debug, thiserror::Error)]
pub enum ArenaError {
    #[error("vulkan: {0:?}")]
    Vk(vk::Result),
    #[error("no host-visible host-coherent memory type")]
    NoMemoryType,
}

impl From<vk::Result> for ArenaError {
    fn from(r: vk::Result) -> Self {
        ArenaError::Vk(r)
    }
}

/// A stable allocation within a batch. Valid until batch retirement.
#[derive(Debug, Clone, Copy)]
pub struct UploadAllocation {
    pub buffer: vk::Buffer,
    pub offset: u64,
    pub size: u64,
    /// CPU-mapped pointer at `buffer + offset`. Host-coherent, no
    /// flush needed. Caller writes via `copy_nonoverlapping`.
    pub mapped_ptr: NonNull<u8>,
}

unsafe impl Send for UploadAllocation {}

#[derive(Debug)]
struct Chunk {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    base_ptr: NonNull<u8>,
    size: u64,
    /// Bytes used in this chunk. Monotonic within the batch.
    used: u64,
}

unsafe impl Send for Chunk {}

#[derive(Debug)]
pub struct BatchUploadArena {
    vk: Arc<VkContext>,
    chunks: Vec<Chunk>,
    /// First chunk's initial capacity; subsequent chunks double up
    /// to `MAX_CHUNK_SIZE`.
    min_chunk_size: u64,
}

const MIN_CHUNK_SIZE: u64 = 1024 * 1024; // 1 MiB
const MAX_CHUNK_SIZE: u64 = 64 * 1024 * 1024; // 64 MiB

impl BatchUploadArena {
    pub fn new(vk: Arc<VkContext>) -> Self {
        Self {
            vk,
            chunks: Vec::new(),
            min_chunk_size: MIN_CHUNK_SIZE,
        }
    }

    /// Allocate `size` bytes aligned to `alignment` (must be a
    /// power of two). Returns a stable allocation; the
    /// `mapped_ptr` is writable until batch retirement.
    pub fn alloc(&mut self, size: u64, alignment: u64) -> Result<UploadAllocation, ArenaError> {
        debug_assert!(alignment.is_power_of_two(), "alignment not pow2");
        if size == 0 {
            return Err(ArenaError::Vk(vk::Result::ERROR_VALIDATION_FAILED_EXT));
        }

        // Try to fit in the active chunk.
        if let Some(chunk) = self.chunks.last_mut() {
            let aligned = (chunk.used + alignment - 1) & !(alignment - 1);
            if aligned + size <= chunk.size {
                let offset = aligned;
                chunk.used = aligned + size;
                // SAFETY: chunk.base_ptr is mapped for the full
                // chunk size, and offset+size ≤ chunk.size.
                let mapped_ptr = unsafe {
                    NonNull::new_unchecked(chunk.base_ptr.as_ptr().add(offset as usize))
                };
                return Ok(UploadAllocation {
                    buffer: chunk.buffer,
                    offset,
                    size,
                    mapped_ptr,
                });
            }
        }

        // Grow: allocate a new chunk.
        let next_size = self
            .chunks
            .last()
            .map(|c| (c.size * 2).min(MAX_CHUNK_SIZE))
            .unwrap_or(self.min_chunk_size)
            .max(size); // never undersize for the request
        let chunk = Self::allocate_chunk(&self.vk, next_size)?;
        let mapped_ptr = chunk.base_ptr;
        let buffer = chunk.buffer;
        let mut chunk = chunk;
        chunk.used = size;
        self.chunks.push(chunk);
        Ok(UploadAllocation {
            buffer,
            offset: 0,
            size,
            mapped_ptr,
        })
    }

    fn allocate_chunk(vk: &VkContext, size: u64) -> Result<Chunk, ArenaError> {
        let buf_info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { vk.device.create_buffer(&buf_info, None)? };

        let mem_reqs = unsafe { vk.device.get_buffer_memory_requirements(buffer) };
        let mem_props = unsafe {
            vk.instance
                .get_physical_device_memory_properties(vk.physical_device)
        };
        let want = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let mt = (0..mem_props.memory_type_count).find(|&i| {
            mem_reqs.memory_type_bits & (1 << i) != 0
                && mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(want)
        });
        let Some(mt) = mt else {
            unsafe { vk.device.destroy_buffer(buffer, None) };
            return Err(ArenaError::NoMemoryType);
        };
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mt);
        let memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
            Ok(m) => m,
            Err(e) => {
                unsafe { vk.device.destroy_buffer(buffer, None) };
                return Err(ArenaError::Vk(e));
            }
        };
        if let Err(e) = unsafe { vk.device.bind_buffer_memory(buffer, memory, 0) } {
            unsafe {
                vk.device.free_memory(memory, None);
                vk.device.destroy_buffer(buffer, None);
            }
            return Err(ArenaError::Vk(e));
        }
        let mapped = match unsafe {
            vk.device.map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
        } {
            Ok(p) => p,
            Err(e) => {
                unsafe {
                    vk.device.free_memory(memory, None);
                    vk.device.destroy_buffer(buffer, None);
                }
                return Err(ArenaError::Vk(e));
            }
        };
        let base_ptr = NonNull::new(mapped.cast::<u8>()).expect("vkMapMemory non-null");
        Ok(Chunk {
            buffer,
            memory,
            base_ptr,
            size,
            used: 0,
        })
    }
}

impl BatchResource for BatchUploadArena {
    fn release(self: Box<Self>, vk: &VkContext) {
        for chunk in self.chunks {
            unsafe {
                vk.device.unmap_memory(chunk.memory);
                vk.device.destroy_buffer(chunk.buffer, None);
                vk.device.free_memory(chunk.memory, None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The allocator's bump arithmetic is testable without Vk:
    // factor the alignment-and-fit math into a pure helper, then
    // unit-test it. Full VkBuffer/Memory paths are validated by
    // hardware smoke in 3A T5.

    fn align_up(offset: u64, alignment: u64) -> u64 {
        (offset + alignment - 1) & !(alignment - 1)
    }

    #[test]
    fn align_up_pow2() {
        assert_eq!(align_up(0, 16), 0);
        assert_eq!(align_up(1, 16), 16);
        assert_eq!(align_up(16, 16), 16);
        assert_eq!(align_up(17, 16), 32);
        assert_eq!(align_up(1023, 256), 1024);
    }

    #[test]
    fn fits_in_chunk() {
        // (used, alignment, size, chunk_size) → would_fit
        let cases = [
            (0u64, 16u64, 100u64, 1024u64, true),
            (900, 16, 100, 1024, true),  // 912 + 100 = 1012 ≤ 1024
            (912, 16, 200, 1024, false), // 912 + 200 = 1112 > 1024
            (0, 256, 1024, 1024, true),
        ];
        for (used, align, size, chunk_size, expected) in cases {
            let aligned = align_up(used, align);
            let fits = aligned + size <= chunk_size;
            assert_eq!(fits, expected, "used={used} align={align} size={size}");
        }
    }
}
```

- [ ] **Step 2: Wire arena into `PaintBatch`**

The arena is owned as a strongly-typed `Option<BatchUploadArena>` field on `PaintBatch`, separate from `retire_resources` (which is `Vec<Box<dyn BatchResource>>` and doesn't let callers downcast back). Lazy-init on first `upload_arena_mut` call; released alongside `retire_resources` at `retire_now` / `poison`.

```rust
pub struct PaintBatch {
    // …existing fields from T1…
    upload_arena: Option<BatchUploadArena>,
}

impl PaintBatch {
    /// Mutable reference to the per-batch upload arena, lazy-init
    /// on first call.
    pub fn upload_arena_mut(&mut self) -> &mut BatchUploadArena {
        if self.upload_arena.is_none() {
            self.upload_arena = Some(BatchUploadArena::new(self.vk.clone()));
        }
        self.upload_arena.as_mut().unwrap()
    }
}
```

In both `retire_now` and `poison`, drain the arena **before** the generic `retire_resources` list (arena retirement is part of batch retirement, not a generic resource):

```rust
        if let Some(arena) = self.upload_arena.take() {
            Box::new(arena).release(&self.vk);
        }
        for r in self.retire_resources.drain(..) {
            r.release(&self.vk);
        }
```

`new` initializes `upload_arena: None` (matching the lazy-init pattern).

`BatchDescriptorArena` (T3) follows the same pattern in its own field.

- [ ] **Step 3: Run tests**

```bash
cargo +nightly fmt
cargo clippy
cargo test -p yserver kms::scheduler::batch_upload_arena
```

Expected: 2 tests pass (align_up_pow2, fits_in_chunk). PaintBatch tests still pass.

- [ ] **Step 4: Commit**

```bash
git add crates/yserver/src/kms/scheduler/batch_upload_arena.rs \
        crates/yserver/src/kms/scheduler/mod.rs \
        crates/yserver/src/kms/scheduler/paint_batch.rs
git commit -m "feat(scheduler): BatchUploadArena — chunked host-visible per-batch allocator"
```

---

## Task 3: `BatchDescriptorArena` (paint-side per-batch descriptor pool)

**Files:**
- Create: `crates/yserver/src/kms/scheduler/batch_descriptor_arena.rs`
- Modify: `crates/yserver/src/kms/scheduler/paint_batch.rs` (add accessor)
- Modify: `crates/yserver/src/kms/scheduler/mod.rs` (module registration)
- Modify: `crates/yserver/src/kms/vk/render_pipeline.rs` (audit + optional: expose layouts for external allocation)

**Scope.** The composite path already has per-output pools via phase 2's `CompositePoolRing` — those are NOT touched. Paint-side recorders that allocate descriptor sets today use `RenderPipelineCache::reset_descriptors` + `allocate_descriptor_for_views` (one pool, reset every op). That pool is unsafe to share inside one CB across multiple recorder appends — a second op's `reset_descriptors` invalidates the first op's already-recorded descriptor binding.

`BatchDescriptorArena` owns one `VkDescriptorPool` per batch (chunk-grown). When phase 3D migrates render/text, those recorders allocate from this arena instead of the shared pool.

This task lays the infrastructure and audits existing paint-side pools but **does NOT migrate any recorder** — that's 3D's job. The shared `RenderPipelineCache.reset_descriptors` stays in place; 3B's migrations (fill, simple copy) don't touch it.

- [ ] **Step 1: Audit paint-side descriptor pools**

```bash
rg -nB1 -A4 'create_descriptor_pool\|allocate_descriptor_sets\|reset_descriptor_pool' \
   crates/yserver/src/kms/vk/ | grep -v 'composite_pool_ring\|pipeline.rs\b'
```

Expected hits (catalogue what you find): `render_pipeline.rs` (RenderPipelineCache), `text_pipeline.rs` (text descriptors), possibly `dst_readback.rs`. Each is a paint-side pool that 3D's migrations will need to route through `BatchDescriptorArena`. Write the catalogue into a short comment block at the top of the new arena file so 3D's plan author has the list to work from.

- [ ] **Step 2: Create `batch_descriptor_arena.rs`**

```rust
//! Per-batch descriptor pool for paint-side recorders.
//!
//! Phase 3D migrations (render, text) route descriptor allocations
//! through this arena so multiple recorder appends in one CB don't
//! invalidate each other's descriptor sets via shared-pool reset.
//!
//! Sizing: each batch gets one pool sized for a typical batch
//! (256 sets, 1024 COMBINED_IMAGE_SAMPLER, 256 UNIFORM_BUFFER,
//! 64 STORAGE_BUFFER). Growth allocates an additional pool chunk
//! when the active pool is exhausted — recorded sets in earlier
//! chunks stay valid because pools are released only at batch
//! retirement.

use std::sync::Arc;

use ash::vk;

use crate::kms::scheduler::paint_batch::BatchResource;
use crate::kms::vk::device::VkContext;

#[derive(Debug)]
pub struct BatchDescriptorArena {
    vk: Arc<VkContext>,
    pools: Vec<vk::DescriptorPool>,
    /// Approximate sets remaining in the active pool. When 0, the
    /// next `allocate_set` grows. This is heuristic — Vulkan
    /// returns `OUT_OF_POOL_MEMORY` if a specific descriptor type
    /// is exhausted before the set count is.
    sets_remaining_in_active: u32,
}

const SETS_PER_POOL: u32 = 256;
const SAMPLERS_PER_POOL: u32 = 1024;
const UNIFORMS_PER_POOL: u32 = 256;
const STORAGE_PER_POOL: u32 = 64;

impl BatchDescriptorArena {
    pub fn new(vk: Arc<VkContext>) -> Self {
        Self {
            vk,
            pools: Vec::new(),
            sets_remaining_in_active: 0,
        }
    }

    /// Allocate one descriptor set with `layout`. Grows the pool
    /// if the active one is exhausted (or if allocation returns
    /// `OUT_OF_POOL_MEMORY`).
    pub fn allocate_set(
        &mut self,
        layout: vk::DescriptorSetLayout,
    ) -> Result<vk::DescriptorSet, vk::Result> {
        if self.sets_remaining_in_active == 0 {
            self.grow()?;
        }
        let pool = *self.pools.last().expect("just grew");
        let layouts = [layout];
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool)
            .set_layouts(&layouts);
        match unsafe { self.vk.device.allocate_descriptor_sets(&alloc_info) } {
            Ok(sets) => {
                self.sets_remaining_in_active -= 1;
                Ok(sets[0])
            }
            Err(vk::Result::ERROR_OUT_OF_POOL_MEMORY)
            | Err(vk::Result::ERROR_FRAGMENTED_POOL) => {
                // Pool is full despite our counter; force grow + retry once.
                self.sets_remaining_in_active = 0;
                self.grow()?;
                let pool = *self.pools.last().expect("just grew");
                let alloc_info = vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&layouts);
                let sets = unsafe { self.vk.device.allocate_descriptor_sets(&alloc_info)? };
                self.sets_remaining_in_active -= 1;
                Ok(sets[0])
            }
            Err(e) => Err(e),
        }
    }

    fn grow(&mut self) -> Result<(), vk::Result> {
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(SAMPLERS_PER_POOL),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(UNIFORMS_PER_POOL),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(STORAGE_PER_POOL),
        ];
        let info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(SETS_PER_POOL)
            .pool_sizes(&pool_sizes);
        let pool = unsafe { self.vk.device.create_descriptor_pool(&info, None)? };
        self.pools.push(pool);
        self.sets_remaining_in_active = SETS_PER_POOL;
        Ok(())
    }
}

impl BatchResource for BatchDescriptorArena {
    fn release(self: Box<Self>, vk: &VkContext) {
        for p in self.pools {
            unsafe { vk.device.destroy_descriptor_pool(p, None) };
        }
    }
}
```

- [ ] **Step 3: Wire into `PaintBatch`**

Same pattern as the upload arena — separate `Option<BatchDescriptorArena>` field, lazy-init accessor, drained at retire/poison alongside the upload arena.

- [ ] **Step 4: Verify**

```bash
cargo +nightly fmt
cargo clippy
cargo test
```

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/scheduler/batch_descriptor_arena.rs \
        crates/yserver/src/kms/scheduler/paint_batch.rs \
        crates/yserver/src/kms/scheduler/mod.rs
git commit -m "feat(scheduler): BatchDescriptorArena — per-batch paint descriptor pool"
```

---

## Task 4: Layout-state failure policy

**Files:**
- Modify: `crates/yserver/src/kms/vk/target.rs` (or whichever module owns `DrawableImage::set_current_layout`)
- Modify: `crates/yserver/src/kms/scheduler/paint_batch.rs` (poison hook)

**Problem.** Recorders today mutate CPU-side image layout state (`DrawableImage::set_current_layout(SHADER_READ_ONLY_OPTIMAL)`) **during** recording — between the barrier records that will set the GPU layout and the recorder's return. If the recorder errors after that mutation (or if its batch is later poisoned without submit), CPU state says the layout is one thing while GPU state hasn't moved. Subsequent recorders that read `current_layout` and emit a `from_old_to_new` barrier emit the wrong barrier.

**Policy: poison-and-revalidate.** When a batch is poisoned:

1. Every drawable that was touched in this batch must be marked layout-invalid.
2. The next paint op on that drawable emits a barrier from `vk::ImageLayout::UNDEFINED` (which is always-valid as a `srcLayout` for a discard-style transition).
3. Drawable contents are NOT preserved — the caller bumps the relevant output's dirty generation so the drawable is repainted from source.

In practice the most common form of "touched in this batch" is "any drawable whose `&mut DrawableImage` was borrowed by a recorder." We can track this without callsite changes by exposing a `record_paint_op` variant that takes `&mut [&mut DrawableImage]` as touched drawables:

```rust
pub fn record_paint_op_touching<F>(
    &mut self,
    touched: &mut [&mut DrawableImage],
    record: F,
) -> Result<(), vk::Result>
where
    F: FnOnce(&VkContext, vk::CommandBuffer) -> Result<(), vk::Result>,
```

On poison, walk the captured drawables and call `mark_layout_invalid()` (added in this task on `DrawableImage`), which sets `current_layout = UNDEFINED` and bumps the drawable's dirty bit.

**Phase 3A policy: poison-and-discard, audit-gated per tranche.** 3A audits only the recorders that 3B will migrate (fill, copy-distinct, copy-same). Each later tranche (3C, 3D) audits its own families before migrating them; if a family fails the audit, that tranche must either (a) fix the recorder to mutate layout state only on the success-path tail, or (b) implement the touched-drawable invalidation hook described next, before migrating.

**Touched-drawable invalidation hook (sketch, not implemented in 3A).** If 3C/3D audits surface a recorder with early `set_current_layout`, add a `record_paint_op_touching` variant that captures the touched `&mut DrawableImage` borrows; on poison, walk them and call `mark_layout_invalid` (sets `current_layout = UNDEFINED` + bumps the relevant output's dirty generation so the drawable is repainted before next composite). 3A leaves this unimplemented because no 3B family needs it.

- [ ] **Step 1: Audit the 3B recorder families for late-mutation invariant**

```bash
rg -nB2 -A20 'set_current_layout' crates/yserver/src/kms/vk/ops/fill.rs \
                                  crates/yserver/src/kms/vk/ops/copy.rs
```

For each of `fill::record_fill_rectangles`, `fill::record_logic_fill`, `copy::record_copy_area_distinct`, `copy::record_copy_area_same`, verify:
- Every `?` / `return Err(...)` / `match … { Err(e) => return … }` appears **before** the first `set_current_layout` call.
- Or: every `set_current_layout` is followed only by infallible operations.

If the audit fails for any 3B recorder, fix the recorder first (move the `set_current_layout` call to after the last fallible call). 3A cannot land 3B-migration-ready without this audit passing.

`copy::record_copy_area_same_overlap` migrates in 3D (it uses `CopyScratch`); its audit is deferred. `image::record_put_image` and `record_get_image` migrate in 3C (they use staging); their audits are deferred. `render::record_render_composite`, `text::record_text_run`, `traps::record_*` migrate in 3D; deferred.

Write the audit results into a comment block in `paint_batch.rs`:

```rust
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
//! Audited 2026-05-12 (load-bearing for 3B):
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
```

- [ ] **Step 2: Add a `Poisoned` integration test**

```rust
    #[test]
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
```

Mark with `#[ignore]` if a real Vk harness isn't available; the hardware smoke in T5 covers it.

- [ ] **Step 3: Verify**

```bash
cargo +nightly fmt
cargo clippy
cargo test
```

- [ ] **Step 4: Commit**

```bash
git add crates/yserver/src/kms/scheduler/paint_batch.rs
git commit -m "feat(scheduler): layout-state policy — poison-and-discard with late-mutation audit"
```

---

## Task 5: `BatchFlushReason` enum + `flush_if_needed` + `record_paint_op` helper

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs`
- Modify: `crates/yserver/src/kms/scheduler/paint_batch.rs` (re-export reason enum)

The HLD's required-invariant section enumerates **flush reasons**. Codifying them as an enum makes the "did you remember to flush?" audit grep-able.

```rust
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
    /// An explicit protocol barrier requested it. (Place-holder for
    /// future use; X11 doesn't define one directly today.)
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
```

`KmsBackend::flush_if_needed(reason)` calls `scheduler.close_and_submit(...)` and logs the reason at trace level. Reasons are not selective today — every flush is a full close — but the parameter exists so phase 4 can use it to decide between "submit + signal" and "submit + wait" paths.

`KmsBackend::record_paint_op<F>(F)` is the single entry point for paint recorders (signature identical to `run_one_shot_op`'s closure for migration compatibility). **Phase 3A does not migrate any recorder** — `record_paint_op` exists from this task forward but has zero call sites; the first call site lands in 3B.

- [ ] **Step 1: Add `BatchFlushReason` to `paint_batch.rs`**

(As above, plus a `#[derive(Debug, Clone, Copy)]` and a `Display` impl emitting the variant name for logging.)

- [ ] **Step 2: Add `flush_if_needed` + `record_paint_op` on `KmsBackend`**

```rust
    /// Flush the current paint batch for `reason`.
    ///
    /// Error semantics depend on `reason`:
    ///
    /// - `VisibleComposite` / `SizeLimit` / `LatencyLimit` /
    ///   `Shutdown`: best-effort. A Poisoned batch (recorder
    ///   failure earlier this cycle) is acceptable — composite
    ///   will sample whatever mirrors currently hold; the
    ///   recorder's affected drawables are already marked
    ///   dirty for the next cycle.
    /// - `Readback` / `ExternalSync` / `ProtocolBarrier`: the
    ///   caller's contract requires the batch's work to have
    ///   COMPLETED before this returns. A Poisoned or
    ///   InvalidState batch means we cannot promise that; surface
    ///   the failure so the caller can fail the request (return
    ///   `BadAlloc`-shaped X error, return zeros from GetImage,
    ///   etc.).
    ///
    /// **Any `Err(vk::Result)` returned here is fatal**: it comes
    /// from `submit_and_wait`'s path 2 (wait failure ⇒ abandoned
    /// CB/resources). Callers MUST propagate up to the main loop
    /// and enter backend teardown / disabled-renderer state;
    /// continuing to schedule paint work after this is not a
    /// supported steady state.
    pub fn flush_if_needed(
        &mut self,
        reason: crate::kms::scheduler::paint_batch::BatchFlushReason,
    ) -> Result<(), ash::vk::Result> {
        use crate::kms::scheduler::paint_batch::{BatchError, BatchFlushReason};
        log::trace!("flush_if_needed: reason={reason:?}");
        let dirty_outputs: Vec<usize> = (0..self.outputs.len())
            .filter(|&i| self.outputs[i].damage.needs_composite())
            .collect();
        let result = self.scheduler.close_and_submit(dirty_outputs);
        let strict = matches!(
            reason,
            BatchFlushReason::Readback
                | BatchFlushReason::ExternalSync
                | BatchFlushReason::ProtocolBarrier
        );
        match result {
            Ok(()) => Ok(()),
            Err(BatchError::Vk(r)) => Err(r),
            Err(BatchError::Poisoned) if strict => {
                log::warn!(
                    "flush_if_needed({reason:?}): batch was Poisoned; \
                     caller's completion guarantee cannot be honoured"
                );
                Err(ash::vk::Result::ERROR_DEVICE_LOST)
            }
            Err(BatchError::InvalidState(s)) if strict => {
                log::error!(
                    "flush_if_needed({reason:?}): batch in invalid state {s:?}; \
                     caller's completion guarantee cannot be honoured"
                );
                Err(ash::vk::Result::ERROR_UNKNOWN)
            }
            // Best-effort reasons swallow Poisoned / InvalidState.
            Err(_) => Ok(()),
        }
    }

    /// Append a paint-recorder op into the current `PaintBatch`'s
    /// command buffer. Closure receives `(&VkContext, &mut PaintBatch, vk::CommandBuffer)`.
    ///
    /// **This is the load-bearing API**, with `&mut PaintBatch`
    /// exposed so 3C recorders can call `batch.upload_arena_mut()` /
    /// `batch.descriptor_arena_mut()` / `batch.adopt(resource)` from
    /// inside the closure. 3B fill/copy recorders ignore the batch
    /// parameter (use the thin `record_paint_op` shim below).
    ///
    /// Landing the wide signature now means 3C does not need to
    /// refactor the API introduced in 3A.
    pub fn record_paint_batch_op<F>(&mut self, record: F) -> Result<(), ash::vk::Result>
    where
        F: FnOnce(
            &crate::kms::vk::device::VkContext,
            &mut crate::kms::scheduler::paint_batch::PaintBatch,
            ash::vk::CommandBuffer,
        ) -> Result<(), ash::vk::Result>,
    {
        let Some(vk_arc) = self.vk.as_ref().cloned() else {
            return Err(ash::vk::Result::ERROR_INITIALIZATION_FAILED);
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return Err(ash::vk::Result::ERROR_INITIALIZATION_FAILED);
        };
        // open_batch consumes the Arc — clone for the closure
        // invocation below.
        let _ = self.scheduler.open_batch(vk_arc.clone(), pool_handle);
        let batch = self
            .scheduler
            .current_paint_batch
            .as_mut()
            .expect("open_batch just ran");
        // We need `&mut PaintBatch` AND the closure has to receive
        // both `&mut PaintBatch` and the CB. `append`'s F sees only
        // `(&VkContext, CommandBuffer)`. Inline the equivalent of
        // `batch.append(record)` so the user closure can take
        // `batch` too. The CB must come from `batch`'s state
        // machine — replicate the lazy-open path here.
        use crate::kms::scheduler::paint_batch::{BatchError, BatchState};
        match batch.state() {
            BatchState::Poisoned => return Err(ash::vk::Result::ERROR_DEVICE_LOST),
            BatchState::Closed | BatchState::Submitted | BatchState::Retired => {
                log::error!(
                    "record_paint_batch_op: batch in non-recording state {:?}",
                    batch.state()
                );
                return Err(ash::vk::Result::ERROR_UNKNOWN);
            }
            BatchState::Idle => {
                // The lazy-open path lives on PaintBatch itself —
                // expose a method that begins recording and returns
                // the CB, then call into the user closure. See
                // `PaintBatch::begin_recording_explicit` added in
                // T1 step 1.
                if let Err(e) = batch.begin_recording_explicit() {
                    return Err(match e {
                        BatchError::Vk(r) => r,
                        _ => ash::vk::Result::ERROR_UNKNOWN,
                    });
                }
            }
            BatchState::Recording => {}
        }
        let cb = batch.command_buffer().expect("Recording implies cb");
        // Borrow split: vk_arc was cloned above; pass it by reference.
        match record(&vk_arc, batch, cb) {
            Ok(()) => Ok(()),
            Err(e) => {
                batch.poison_external();
                Err(e)
            }
        }
    }

    /// Thin shim for recorders that don't need the batch handle
    /// (fill, copy-distinct, copy-same). Matches the existing
    /// `run_one_shot_op` closure signature for textual-rewrite
    /// migration in 3B.
    pub fn record_paint_op<F>(&mut self, record: F) -> Result<(), ash::vk::Result>
    where
        F: FnOnce(&crate::kms::vk::device::VkContext, ash::vk::CommandBuffer) -> Result<(), ash::vk::Result>,
    {
        self.record_paint_batch_op(|vk, _batch, cb| record(vk, cb))
    }
```

- [ ] **Step 3: Audit every CPU-visible / sync-export request handler**

Per codex's F3, the plan must enumerate every site that needs `flush_if_needed(Readback | ExternalSync)`. Catalogue:

```bash
rg -n 'record_get_image\|read_mirror_pixels\|hw_cursor_refresh' crates/yserver/src/kms/backend.rs
rg -n 'PresentPixmap\|present_pixmap\|dri3\|SyncTriggerFence\|sync_trigger' crates/yserver/src/
```

For each hit, append a `flush_if_needed(Readback)` (or `ExternalSync`) **before** the existing per-op `run_one_shot_op`. Until 3B migrates the first recorder, these flushes are no-ops on Idle batches — but the call sites are in place when 3B's first migration creates a meaningful pending batch.

Document the catalogued sites in a comment block at the top of the section of `backend.rs` that holds these handlers.

- [ ] **Step 4: Wire `composite_and_flip` to use `flush_if_needed(VisibleComposite)`**

Replace the direct `close_and_submit` call from 3A T1 step 3 with:

```rust
        if let Err(e) = self.flush_if_needed(BatchFlushReason::VisibleComposite) {
            // Per submit_and_wait's path-2 semantics, this is fatal:
            // a CB and its resources have been abandoned. Stop
            // normal rendering. The exact teardown path is the
            // caller's choice (mark a `renderer_failed` flag,
            // bubble up out of the event loop, abort, etc.) —
            // pick the one that fits the existing error
            // propagation in this function's signature.
            log::error!(
                "composite cycle: paint batch flush returned fatal {e:?}; \
                 KMS renderer is in an unrecoverable state"
            );
            return Err(std::io::Error::other(format!(
                "PaintBatch::submit_and_wait failed: {e:?}"
            )));
        }
```

- [ ] **Step 5: Hardware smoke**

```bash
just yserver-mate-hw-release   # or local equivalent
```

Expected: desktop comes up; no regression vs phase 2. No `paint batch submit failed` warnings.

XTS regression check (memory `feedback_xts_iteration`):

```bash
just xts-yserver
```

Expected: matches phase-2 xts baseline.

- [ ] **Step 6: Commit**

```bash
git add crates/yserver/src/kms/backend.rs \
        crates/yserver/src/kms/scheduler/paint_batch.rs
git commit -m "feat(kms): BatchFlushReason + flush_if_needed + record_paint_op (no call sites)"
```

---

# Renderer-disabled design (prerequisite for 3B)

3A landed `flush_if_needed(VisibleComposite)` returning `io::Result<()>` and `composite_and_flip` propagating Vk failures as `io::Error::other(...)`. The trait surface at `crates/yserver-core/src/backend/trait_def.rs` does NOT carry that error — `Backend::on_page_flip_ready` returns `()`. So today's Vk failure path is **caught at the trait boundary and dropped on the floor** (the `on_page_flip_ready` impl in `KmsBackend` calls `composite_and_flip()` and returns `()`).

Phase 3B is where this becomes a real correctness problem: the first time a recorder migrates, a `Poisoned` batch or a wait-failure can produce a real fatal `Err`, and silently dropping it means the next composite tick will try to use abandoned Vulkan handles.

**3B must land a `renderer_failed` strategy before its first recorder migration.** Three options; design discussion will pick one before 3B T1:

**Option A — in-band disabled flag (recommended).** Add `renderer_failed: bool` on `KmsBackend`. Set true on any fatal `flush_if_needed` Err. Gate every paint entry point on it: `record_paint_op`, `flush_if_needed`, `composite_and_flip` early-return `Ok(())` (or a no-op pixman-shadow fallback) when the flag is set. Pros: in-process; no trait churn; survivable for clients that only need the X server's input/wire path. Cons: screen freezes at the last good frame; users may not notice.

**Option B — trait surface propagates error.** Change `Backend::on_page_flip_ready` to return `io::Result<()>`. Core loop terminates on fatal renderer failure. Pros: explicit, no zombie state. Cons: invasive trait change; ynest backend (host-X11) has different failure modes that don't fit a single error type cleanly.

**Option C — panic.** The HLD's `submit_and_wait` doc says device-lost is "not a recoverable state." Crashing the process and relying on an external supervisor (systemd, a session manager) to restart yserver is defensible. Pros: simplest; matches the actual semantic. Cons: aggressive; no graceful X-client disconnect; logs and external state may be inconsistent.

**Recommendation: Option A** for phase 3, with a `log::error!` so a supervisor can pick up the failure from logs and (optionally) restart. Phase 4's timeline-semaphore work can revisit if it changes the failure-mode shape. The fatal `Err` path in `composite_and_flip` keeps the current `return Err(...)` (so anyone calling it directly sees the error), and a thin wrapper in `on_page_flip_ready` sets the flag + logs.

3B T0 owns the implementation: add the flag, gate the three entry points, ensure tests cover both the live and disabled states.

---

# Phase 3B — migrate scratch-free paint families (sketch)

After 3A lands. Re-plan as a writing-plans-skill-compliant detail block before executing — the ownership APIs (especially the layout-state policy under real load) will inform what each migration looks like.

**In scope:**
- `fill::record_fill_rectangles` and `record_logic_fill` (no scratch deps).
- `copy::record_copy_area_distinct` and `record_copy_area_same` (no scratch deps; the same-overlap variant that uses `CopyScratch` defers to 3C).

**Out of scope:** every other family. PutImage uses `OpsStaging` (shared buffer); MaskScratch, Glyph atlas, Gradient all have lifetime issues; render+text+traps additionally share descriptor pools.

**Per-family audit checklist (codex's brief, codified):**

1. Recorder does not read host staging memory after recording returns. (Fill/copy: trivially true — no staging.)
2. Recorder does not depend on descriptor reset/reuse. (Fill: no descriptors. Copy: no descriptors.)
3. Recorder does not reuse scratch images/buffers whose contents can be overwritten before batch retirement. (Fill: no scratch. Copy distinct/same: no scratch.)
4. Layout-state mutations obey 3A T4's late-mutation policy. (Both already audited in 3A T4 step 1.)
5. Recorder has a fallback path that can remain synchronous if the audit later turns up a defect. (Both keep their `run_one_shot_op` capability — migration is a per-call-site swap.)

**Approximate task count:** 2 tasks (fill, then copy), each ~5 steps.

**Acceptance:**
- All fill / non-overlap-copy call sites in `backend.rs` go through `record_paint_op`. Hard count from current grep: fill 4 hits, copy-distinct + copy-same = 4 hits (the 5th copy hit is same_overlap, deferred to 3C).
- `cargo test`, xts, rendercheck, hardware smoke green.
- `flush_if_needed(VisibleComposite)` in `composite_and_flip` is now load-bearing — the batch carries real recorded work into it. Verify the per-cycle log shows `state=Recording → Closed → Retired` once during the cycle.

---

# Phase 3C — migrate upload-backed paint (sketch)

After 3B. Each migration converts a scratch's "internal `run_one_shot_op` with shared staging" into "record into the batch CB; upload bytes via `BatchUploadArena`."

**Order (per codex's recommendation):**

1. `image::record_put_image` (PutImage / MIT-SHM PutImage). Today uses `OpsStaging` — replace with `BatchUploadArena::alloc + copy + cmd_copy_buffer_to_image`.
2. `mirror.record_upload_rect` (cursor mirror upload). Same shape.
3. `MaskScratch::upload_r8` → `record_upload_r8_via_arena(arena, cb)`. Mask **image** stays shared but is written from per-batch arena offsets, so two uploads in one batch DO NOT overwrite each other's staging (the image is still single — needs the F5 fix from codex: serialise upload+draw pairs so the image contents are valid between barriers within one CB).
4. Glyph atlas upload (`GlyphAtlas::record_upload` per codex's grep). Atlas image is persistent; upload bytes go via arena.
5. Gradient upload. The current `GradientPicture::new_*` + `upload_initial` allocates a temporary staging buffer per gradient and submits inline. Replace: gradient-create allocates the destination image, defers the upload until the next batch close by capturing arena allocation in the gradient picture; the recorder side emits the buffer→image copy into whichever batch is open when the gradient is first sampled. Alternatively (simpler): force `flush_if_needed(ExternalSync)` at gradient-create time and keep the synchronous upload — gradients are created once per `RenderCreateLinearGradient` request, not per-frame.

**Per-family audit checklist (additions to 3B's):**

6. Upload bytes are allocated from `BatchUploadArena`, NOT from `OpsStaging` / scratch-internal buffers.
7. Scratch images that hold the uploaded content are either (a) per-batch (allocated from a per-batch image pool — adds a new arena), or (b) consumed within the same CB via upload-then-draw barrier ordering with no second writer before the draw.

For MaskScratch, the simplest correct answer is **(b)**: each `(mask upload, draw using that mask)` pair becomes an atomic closure passed to `record_paint_op`, not two separate appends. The HLD design allows this — recorders can take `&mut PaintBatch` and freely intermix barriers and draws.

**Approximate task count:** 5 tasks. Plus 1 or 2 prep tasks for "make MaskScratch / GlyphAtlas / GradientPicture take `&mut BatchUploadArena` in their upload-into-cb variants."

**Acceptance:**
- Two PutImage requests recorded into one batch produce distinct staged contents.
- Two mask uploads (different glyphs / different traps) in one batch don't alias.
- Glyph atlas uploads in one batch don't alias.
- All migrated families go through `BatchUploadArena`; `OpsStaging` is dead code for paint paths (still used by GetImage's readback until phase 5).

---

# Phase 3D — migrate descriptor/scratch-heavy paint (sketch)

After 3C. The hard ones: render, traps, text.

**In scope:**
1. `traps` — both rasterize variants. Uses MaskScratch (now arena-backed from 3C) + a render-pipeline draw. The two get folded into one `record_paint_op` closure so barrier ordering is explicit.
2. `render::record_render_composite` — uses MaskScratch + gradient + RenderPipelineCache descriptors. Descriptors route through `BatchDescriptorArena` (from 3A T3) instead of `RenderPipelineCache.reset_descriptors`.
3. `text::record_text_run` — uses glyph atlas + text pipeline descriptors. Same routing.

**Per-family audit checklist (additions):**

8. Descriptor allocations route through `BatchDescriptorArena`. The pipeline-cache shared pool is not reset during recording.
9. The `RenderPipelineCache.reset_descriptors` method is either removed (callers gone) or scoped to a path no longer reached from `record_paint_op` (e.g., the synchronous fallback that wasn't migrated).
10. The `copy::record_copy_area_same_overlap` migration (deferred from 3B) happens here, using `CopyScratch` either per-batch or via the same upload-then-use-within-CB ordering rule as MaskScratch.

**Approximate task count:** 4 tasks (traps, render, text, copy-same-overlap).

**Acceptance:**
- `RenderPipelineCache::reset_descriptors` has zero callers in the migrated paths.
- Two render-composite ops in one batch each get their own descriptor set; the second doesn't invalidate the first.
- xts / rendercheck / hardware smoke green for the traps and render workloads (mate-control-center hover + wezterm-open were the original symptom workloads).

---

# Phase 4 handoff

At the end of phase 3:

- `PaintBatch` owns command buffer, upload arena, descriptor arena, retire resources.
- `BatchFlushReason` exists; `flush_if_needed` is the single boundary callers cross.
- Some recorder families are migrated (3B); some are arena-backed (3C); some are descriptor-arena-backed (3D); others remain on `run_one_shot_op`. That's acceptable — phase 4's invariant is "no hot-path waitIdle on the **migrated** paths."

Phase 4 replaces the close-time `vkQueueWaitIdle` in `PaintBatch::submit_and_wait` with a timeline-semaphore signal threaded into composite's `waitSemaphores`. The state machine's `Closed → Submitted → Retired` split materialises: submit returns immediately, and `release_holder` drives `Retired`. The `holders` refcount is already in place.

Phase 5 retires GetImage's targeted-fence path.

---

# Validation (across the whole phase)

Each task ends with `cargo +nightly fmt && cargo clippy && cargo test`. Each migration task (3B–3D) additionally runs:

```bash
just xts-yserver
just rendercheck-yserver
```

Plus hardware smoke (`just yserver-mate-hw-release` or similar) with these manual cases (per codex's brief):

- repeated PutImage before a visible composite
- text drawing with multiple glyph uploads in one frame
- RENDER masks and gradients
- multi-output movement (old + new region dirty propagation, per phase 1)
- GetImage / readback after offscreen pixmap drawing with no dirty output (exercises `flush_if_needed(Readback)`)
- mate-control-center hover (the original GPU-saturation symptom — should NOT regress vs phase 2)

Write a `phase3-results.md` doc capturing preflight checks, cutover greps, xts/rendercheck status, hardware smoke notes, and any newly catalogued `vkQueueWaitIdle` sites that survived phase 3.

---

# Done conditions

Phase 3 is complete when (3A is the gate; 3B–3D are incremental):

**3A complete (gate):**
1. `PaintBatch` has the `Idle → Recording → Closed → Submitted → Retired` state machine + `Poisoned` terminal, with submit / wait failures handled by three distinct paths (submit-fail → poison; wait-fail → intentional leak until backend drain; both-ok → retire).
2. `BatchUploadArena` exists, owned per-batch, stable-offset, chunk-grown.
3. `BatchDescriptorArena` exists, owned per-batch, paint-side pool sized per the constants in T3.
4. `BatchFlushReason` enum + `KmsBackend::flush_if_needed(reason)` are the only entry points for batch close. Strict reasons (Readback / ExternalSync / ProtocolBarrier) surface Poisoned / InvalidState as `ERROR_DEVICE_LOST` / `ERROR_UNKNOWN`; best-effort reasons swallow them.
5. `KmsBackend::record_paint_batch_op<F>` (`F: (&VkContext, &mut PaintBatch, vk::CommandBuffer) -> …`) exists with zero call sites (3B adds them). `record_paint_op` is the narrow shim for closures that don't need the batch handle.
6. Layout-state failure policy is documented; **fill** and **copy-distinct** / **copy-same** recorders are audited for late-mutation (load-bearing for 3B); other families are flagged as deferred audits, gating 3C/3D entry.
7. CPU-visible / sync-export request handlers all call `flush_if_needed(Readback | ExternalSync)` before their per-op `run_one_shot_op`. Catalogue lives at the top of the relevant `backend.rs` section.
8. `cargo test`, xts, rendercheck, hardware smoke all green; no regressions vs phase 2.

**3B / 3C / 3D each:**
- Per-family audit checklist passes before any call-site rewrite.
- All migrated call sites in that tranche go through `record_paint_op` and (for 3C/3D) the arenas.
- Two simultaneous in-batch uses of the migrated family produce non-aliasing resource state — proven by the acceptance tests in each phase's sketch.
- xts / rendercheck / hardware smoke remain green.

**Out at phase-3 close:** hot-path `vkQueueWaitIdle` in `PaintBatch::submit_and_wait` (phase 4 retires); GetImage's per-op submit (phase 5 retires); any unmigrated recorder families.
